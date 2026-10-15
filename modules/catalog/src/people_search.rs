//! Поиск людей через Meilisearch: индекс `people` и известность (`person_stats.fame`).
//!
//! Индекс строится и переключается вместе с `entities` (та же перестройка [`crate::search`]:
//! теневой индекс, проверка размера, swap, ротация), правки админки попадают в него через
//! [`update_index`]. Документ компактный ([`PersonDoc`]): имя, имя латиницей, фото, дата
//! рождения, основная роль и известность — без биографии и фильмографии.
//!
//! **Известность** (`fame`) — сумма по работам в активных сущностях «вес роли × число оценок
//! TMDB фильма»: режиссёр и первые 3 актёра — 1, актёры 4–10 — 0.4, остальные актёры — 0.1,
//! сценарист и композитор — 0.3, прочие — 0.15; за фильм — наибольший вес человека. Число
//! оценок копится годами, поэтому мера устойчива (TMDB popularity человека — «тренд недели»:
//! наверх выходят случайные люди). Ею ранжируются совпадения одинакового качества: «Брэд» —
//! сначала Брэд Питт; без запроса — самые известные.

use crate::jobs::{Reporter, Step};
use crate::models::{page_bounds, Page, PeopleSearchQuery, PersonHit};
use crate::people::PUBLIC_PERSON;
use crate::search::{
    search_exact, shadow_of, unavailable, upsert_and_delete, BoxError, MAX_TOTAL_HITS,
};
use crate::validate;
use axum::extract::State;
use axum::http::Method;
use axum::Json;
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::{json, Value};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::search::SearchError;
use shared::{AppError, AppResult, AppState};
use uuid::Uuid;

/// Имя индекса людей (без префикса).
pub const INDEX: &str = "people";

/// Людей в одной пачке перестройки: документы маленькие.
pub const BATCH: usize = 2_000;

/// Вес работы человека в фильме (см. описание модуля). `billing` — номер актёра в титрах.
const ROLE_WEIGHT: &str = "CASE WHEN c.role = 'director' THEN 1.0
                               WHEN c.role = 'actor' AND c.billing <= 3 THEN 1.0
                               WHEN c.role = 'actor' AND c.billing <= 10 THEN 0.4
                               WHEN c.role = 'actor' THEN 0.1
                               WHEN c.role IN ('writer', 'composer') THEN 0.3
                               ELSE 0.15 END";

/// Пересчитывает `person_stats` (`fame`, `known_for`): всех людей (`None`, ~20 с на 900 тыс.)
/// или только `people` (их фильмы целиком — номер в титрах считается по всему составу).
/// Строки без изменений не переписываются. Возвращает число изменённых.
pub async fn refresh_fame(db: &sqlx::PgPool, people: Option<&[Uuid]>) -> sqlx::Result<u64> {
    let (target, films) = match people {
        None => ("SELECT id FROM people", ""),
        Some(_) => (
            "SELECT id FROM people WHERE id = ANY($1)",
            "JOIN (SELECT DISTINCT entity_id FROM entity_credits WHERE person_id = ANY($1)) f
                 ON f.entity_id = c.entity_id",
        ),
    };
    let sql = format!(
        "WITH target AS ({target}),
         c AS (
             SELECT c.person_id, c.entity_id, c.role,
                    CASE WHEN c.role = 'actor' THEN row_number() OVER
                        (PARTITION BY c.entity_id, c.role ORDER BY c.position, c.id) END AS billing
             FROM entity_credits c {films}
         ),
         w AS (
             SELECT c.person_id, c.entity_id, c.role, max({ROLE_WEIGHT}) AS w,
                    coalesce(max(s.tmdb_votes), 0) AS votes
             FROM c
             JOIN target t ON t.id = c.person_id
             JOIN entities e ON e.id = c.entity_id AND e.active
             LEFT JOIN entity_stats s ON s.entity_id = c.entity_id
             GROUP BY 1, 2, 3
         ),
         fame AS (
             SELECT person_id, sum(w * votes) AS fame
             FROM (SELECT person_id, entity_id, max(w) AS w, max(votes) AS votes
                   FROM w GROUP BY 1, 2) per_film
             GROUP BY 1
         ),
         known AS (
             SELECT DISTINCT ON (person_id) person_id, role
             FROM (SELECT person_id, role, sum(w * votes) AS score, count(*) AS works
                   FROM w GROUP BY 1, 2) per_role
             ORDER BY person_id, score DESC, works DESC, role
         )
         INSERT INTO person_stats (person_id, fame, known_for)
         SELECT t.id, coalesce(f.fame, 0), k.role
         FROM target t
         LEFT JOIN fame f ON f.person_id = t.id
         LEFT JOIN known k ON k.person_id = t.id
         ON CONFLICT (person_id) DO UPDATE SET fame = EXCLUDED.fame, known_for = EXCLUDED.known_for
         WHERE person_stats.fame IS DISTINCT FROM EXCLUDED.fame
            OR person_stats.known_for IS DISTINCT FROM EXCLUDED.known_for"
    );
    let query = sqlx::query(&sql);
    let query = match people {
        Some(ids) => query.bind(ids),
        None => query,
    };
    Ok(query.execute(db).await?.rows_affected())
}

/// Документ индекса людей.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct PersonDoc {
    id: Uuid,
    slug: String,
    full_name: String,
    /// Имя латиницей: «brad pitt» находит «Брэд Питт».
    original_name: Option<String>,
    birth_date: Option<NaiveDate>,
    photo_url: Option<String>,
    /// Основная роль (`actor`, `director`, ...): фильтр и подпись в выдаче.
    known_for: Option<String>,
    /// Известность: правило ранжирования `fame:desc`.
    fame: f64,
}

const DOC_SELECT: &str = "SELECT p.id, p.slug, p.full_name, p.original_name, p.birth_date,
                                 p.photo_url, ps.known_for, coalesce(ps.fame, 0) AS fame
                          FROM people p LEFT JOIN person_stats ps ON ps.person_id = p.id";

pub(crate) fn settings() -> Value {
    json!({
        "searchableAttributes": ["full_name", "original_name"],
        "filterableAttributes": ["known_for"],
        "sortableAttributes": ["fame", "full_name"],
        // Известность — после всех правил качества совпадения: «Брэд» → сначала Брэд Питт,
        // но точное совпадение имени важнее славы. Без запроса — по известности.
        "rankingRules": ["words", "typo", "proximity", "attribute", "sort", "exactness", "fame:desc"],
        "pagination": { "maxTotalHits": MAX_TOTAL_HITS },
    })
}

/// Число людей, которые попадут в индекс (для `progress`).
pub(crate) async fn count(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM people p WHERE {PUBLIC_PERSON}"
    ))
    .fetch_one(db)
    .await?;
    Ok(u64::try_from(total).unwrap_or(0))
}

/// Перестройка: известность всех людей, затем все публичные люди пачками по [`BATCH`] (keyset по
/// id) в индекс `version`. Пока Meilisearch применяет пачку, читается следующая.
pub(crate) async fn fill(
    state: &AppState,
    version: &str,
    report: &Reporter,
) -> Result<usize, BoxError> {
    let started = std::time::Instant::now();
    let changed = refresh_fame(&state.db, None).await?;
    tracing::info!(changed, elapsed = ?started.elapsed(), "person fame refreshed");
    let total = count(&state.db).await?;
    report.progress(Step::FillPeople, 0, total);

    let mut after: Option<Uuid> = None;
    let mut indexed: usize = 0;
    let mut pending: Option<(Value, usize)> = None;
    loop {
        let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
            "{DOC_SELECT} WHERE {PUBLIC_PERSON} AND ($1::uuid IS NULL OR p.id > $1)
             ORDER BY p.id LIMIT $2"
        ))
        .bind(after)
        .bind(BATCH as i64)
        .fetch_all(&state.db)
        .await?;
        let Some(last) = docs.last() else { break };
        after = Some(last.id);
        let count = docs.len();
        let task = state
            .search
            .call(
                Method::POST,
                &format!("/indexes/{version}/documents"),
                Some(&serde_json::to_value(&docs)?),
            )
            .await?;
        if let Some((task, count)) = pending.replace((task, count)) {
            state.search.wait(&task).await?;
            indexed += count;
            report.progress(Step::FillPeople, indexed as u64, total.max(indexed as u64));
        }
    }
    if let Some((task, count)) = pending {
        state.search.wait(&task).await?;
        indexed += count;
        report.progress(Step::FillPeople, indexed as u64, total.max(indexed as u64));
    }
    Ok(indexed)
}

/// Правка затронула людей: их известность пересчитывается, документы переотправляются (и в
/// теневой индекс идущей перестройки), скрытые и удалённые — удаляются. Ждёт применения.
pub(crate) async fn update_index(state: &AppState, people: &[Uuid]) -> Result<(), BoxError> {
    if people.is_empty() {
        return Ok(());
    }
    refresh_fame(&state.db, Some(people)).await?;
    let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
        "{DOC_SELECT} WHERE p.id = ANY($1) AND {PUBLIC_PERSON} ORDER BY p.id"
    ))
    .bind(people)
    .fetch_all(&state.db)
    .await?;
    let present: Vec<Uuid> = docs.iter().map(|doc| doc.id).collect();
    let docs = serde_json::to_value(&docs)?;

    let main = state.search.index(INDEX);
    upsert_and_delete(&state.search, &main, &docs, people, &present).await?;
    if let Some(shadow) = shadow_of(&main) {
        if let Err(error) = upsert_and_delete(&state.search, &shadow, &docs, people, &present).await
        {
            tracing::warn!(%error, %shadow, "shadow people index sync failed");
        }
    }
    Ok(())
}

/// Поиск людей: по имени (русскому и латиницей) с опечатками и по началу слова. Из одинаково
/// подходящих выше известные (`fame`: работы в фильмах с большим числом оценок); без `q` — самые
/// известные. `known_for` — основная роль (`actor`, `director`, ...).
#[utoipa::path(
    get, operation_id = "search_people", path = "/search/people", tag = "catalog",
    params(PeopleSearchQuery),
    responses(
        (status = 200, description = "Страница людей. `total` — всего найдено (точно)", body = Page<PersonHit>),
        (status = 400, description = "Длинный `q` или неверный `known_for`", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<PeopleSearchQuery>,
) -> AppResult<Json<Page<PersonHit>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > validate::MAX_TITLE {
        return Err(AppError::BadRequest(format!(
            "q must be at most {} characters",
            validate::MAX_TITLE
        )));
    }
    let filter = match &query.known_for {
        Some(role) => {
            // Проверенную роль (a-z и `_`) безопасно подставлять в строку фильтра.
            validate::role(role)?;
            Some(format!("known_for = {role}"))
        }
        None => None,
    };
    let body = json!({
        "q": q,
        "filter": filter,
        "attributesToRetrieve":
            ["id", "slug", "full_name", "original_name", "birth_date", "photo_url", "known_for"],
    });
    let path = format!("/indexes/{}/search", state.search.index(INDEX));
    let (response, total) = match search_exact(&state.search, &path, body, limit, offset).await {
        Ok(found) => found,
        Err(error) if error.code() == Some("index_not_found") => (json!({ "hits": [] }), 0),
        Err(SearchError::Disabled) => return Err(unavailable()),
        Err(error) => {
            tracing::warn!(%error, "people search request failed");
            return Err(unavailable());
        }
    };
    let items: Vec<PersonHit> = serde_json::from_value(response["hits"].clone())
        .map_err(|e| AppError::Internal(format!("unexpected people hits: {e}")))?;
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}
