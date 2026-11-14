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
//!
//! В индекс попадают только люди хотя бы с одной работой в активной сущности ([`INDEXED_PERSON`]);
//! документ несёт `works_count` и `top_works` для карточки. Выдача «все роли» без запроса — не
//! общий рейтинг (кино перевесило бы книги и игры), а чередование ролей ([`mix_plan`]): порядок
//! внутри роли — по `fame`.

use crate::jobs::{Reporter, Step};
use crate::models::{page_bounds, PeopleFacets, PeoplePage, PeopleSearchQuery, PersonHit};
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
use std::collections::{BTreeMap, HashMap};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::search::SearchError;
use shared::{AppError, AppResult, AppState};
use uuid::Uuid;

/// Имя индекса людей (без префикса).
pub const INDEX: &str = "people";

/// Людей в одной пачке перестройки: документы маленькие.
pub const BATCH: usize = 2_000;

/// Человек попадает в индекс, только если у него есть работа в активной сущности.
const INDEXED_PERSON: &str = "EXISTS (SELECT 1 FROM person_credit_counts pc
                                      WHERE pc.person_id = p.id AND pc.active_credits > 0)";

/// Число работ человека (без сезонов) и до 3 самых известных; `p` — таблица людей снаружи.
const WORKS_LATERAL: &str = "LEFT JOIN LATERAL (
        SELECT count(*) AS works_count,
               coalesce(jsonb_agg(jsonb_build_object('slug', x.slug, 'title', x.title,
                   'kind', x.kind, 'year', x.year, 'poster_url', x.cover_url) ORDER BY x.rn)
                   FILTER (WHERE x.rn <= 3), '[]'::jsonb) AS top_works
        FROM (SELECT e.slug, e.title, e.kind::text AS kind, e.cover_url,
                     extract(year FROM e.release_date)::int AS year,
                     row_number() OVER (ORDER BY coalesce(s.tmdb_votes, 0) DESC,
                                        e.release_date DESC NULLS LAST, e.id) AS rn
              FROM (SELECT DISTINCT entity_id FROM entity_credits WHERE person_id = p.id) c
              JOIN entities e ON e.id = c.entity_id AND e.active AND e.kind <> 'season'
              LEFT JOIN entity_stats s ON s.entity_id = e.id) x
    ) w ON true";

/// Колонки документа; `p` — люди, `ps` — `person_stats`, `w` — [`WORKS_LATERAL`].
const DOC_COLUMNS: &str = "p.id, p.slug, p.full_name, p.original_name, p.birth_date, p.photo_url,
                           ps.known_for, coalesce(ps.fame, 0) AS fame,
                           coalesce(w.works_count, 0) AS works_count,
                           coalesce(w.top_works, '[]'::jsonb) AS top_works";

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
    /// Число работ: «N работ» на карточке.
    works_count: i64,
    /// Массив `{slug, title, kind, year, poster_url}`, до 3 элементов.
    top_works: Value,
}

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
        "SELECT count(*) FROM people p WHERE {INDEXED_PERSON}"
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
        // Сначала пачка id, потом `person_stats`: при JOIN до LIMIT merge join читал
        // `person_stats` с начала на каждой пачке (квадратично).
        let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
            "SELECT {DOC_COLUMNS}
             FROM (SELECT * FROM people p WHERE {INDEXED_PERSON}
                     AND ($1::uuid IS NULL OR p.id > $1) ORDER BY p.id LIMIT $2) p
             LEFT JOIN person_stats ps ON ps.person_id = p.id
             {WORKS_LATERAL}
             ORDER BY p.id"
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
        "SELECT {DOC_COLUMNS}
         FROM people p LEFT JOIN person_stats ps ON ps.person_id = p.id
         {WORKS_LATERAL}
         WHERE p.id = ANY($1) AND {INDEXED_PERSON} ORDER BY p.id"
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

const RETRIEVE: [&str; 9] = [
    "id", "slug", "full_name", "original_name", "birth_date", "photo_url", "known_for",
    "works_count", "top_works",
];

/// Порядок ролей в чередовании: по убыванию числа людей, затем по имени; актёры — дважды
/// (двойная доля), вторым вхождением посередине круга.
fn mix_order(counts: &BTreeMap<String, i64>) -> Vec<&str> {
    let mut roles: Vec<(&str, i64)> = counts
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(role, n)| (role.as_str(), *n))
        .collect();
    roles.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut order: Vec<&str> = roles.into_iter().map(|(role, _)| role).collect();
    if order.contains(&"actor") {
        order.insert(order.len() / 2 + 1, "actor");
    }
    order
}

/// Раскладка страницы `[offset, offset + limit)` выдачи «все роли»: по кругу [`mix_order`], роль
/// с исчерпанными людьми пропускается. Возвращает роли по позициям страницы и для каждой роли
/// `(сколько людей роли было до страницы, сколько на странице)`. Детерминированно: соседние
/// страницы не пересекаются и не теряют людей.
fn mix_plan<'a>(
    order: &[&'a str],
    counts: &BTreeMap<String, i64>,
    limit: i64,
    offset: i64,
) -> (Vec<&'a str>, HashMap<&'a str, (i64, i64)>) {
    let mut taken: HashMap<&str, i64> = HashMap::new();
    let mut before: HashMap<&str, i64> = HashMap::new();
    let mut slots = Vec::new();
    let mut cursor = 0;
    for position in 0..offset.saturating_add(limit) {
        if position == offset {
            before = taken.clone();
        }
        let Some(role) = (0..order.len())
            .map(|step| order[(cursor + step) % order.len()])
            .find(|role| taken.get(role).copied().unwrap_or(0) < counts[*role])
        else {
            break;
        };
        cursor = (cursor + 1) % order.len();
        *taken.entry(role).or_default() += 1;
        if position >= offset {
            slots.push(role);
        }
    }
    let windows = taken
        .iter()
        .map(|(role, end)| {
            let start = before.get(role).copied().unwrap_or(0);
            (*role, (start, end - start))
        })
        .filter(|(_, (_, n))| *n > 0)
        .collect();
    (slots, windows)
}

/// Поиск людей: по имени (русскому и латиницей) с опечатками и по началу слова. Из одинаково
/// подходящих выше известные (`fame`: работы в фильмах с большим числом оценок). Без `q` и
/// `known_for` — чередование ролей, внутри роли по известности. `known_for` — основная роль
/// (`actor`, `director`, ...). `facets.known_for` — счётчики ролей среди найденных по `q`.
#[utoipa::path(
    get, operation_id = "search_people", path = "/search/people", tag = "catalog",
    params(PeopleSearchQuery),
    responses(
        (status = 200, description = "Страница людей. `total` — всего найдено (точно), `facets.known_for` — людей по ролям", body = PeoplePage),
        (status = 400, description = "Длинный `q` или неверный `known_for`", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<PeopleSearchQuery>,
) -> AppResult<Json<PeoplePage>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > validate::MAX_TITLE {
        return Err(AppError::BadRequest(format!(
            "q must be at most {} characters",
            validate::MAX_TITLE
        )));
    }
    let role = match &query.known_for {
        Some(role) => Some(validate::role(role)?),
        None => None,
    };
    let path = format!("/indexes/{}/search", state.search.index(INDEX));
    let fail = |error: SearchError| {
        if matches!(error, SearchError::Disabled) {
            return unavailable();
        }
        tracing::warn!(%error, "people search request failed");
        unavailable()
    };

    // Счётчики ролей — по `q`, без фильтра роли.
    let facets_body = json!({
        "q": q, "facets": ["known_for"], "page": 1, "hitsPerPage": 0, "attributesToRetrieve": [],
    });
    let counts: BTreeMap<String, i64> =
        match state.search.call(Method::POST, &path, Some(&facets_body)).await {
            Ok(response) => serde_json::from_value::<BTreeMap<String, i64>>(
                response["facetDistribution"]["known_for"].clone(),
            )
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .collect(),
            Err(error) if error.code() == Some("index_not_found") => BTreeMap::new(),
            Err(error) => return Err(fail(error)),
        };
    let facets = PeopleFacets { known_for: counts };

    let (hits, total) = if let (None, true) = (&role, q.is_empty()) {
        // Все роли без запроса: чередование, а не общий рейтинг.
        let total: i64 = facets.known_for.values().sum();
        let order = mix_order(&facets.known_for);
        let (slots, windows) = if order.is_empty() {
            (Vec::new(), HashMap::new())
        } else {
            mix_plan(&order, &facets.known_for, limit, offset.min(total))
        };
        let searches = windows.iter().map(|(role, (start, count))| {
            let body = json!({
                "q": "", "filter": format!("known_for = {role}"), "offset": start,
                "limit": count, "attributesToRetrieve": RETRIEVE,
            });
            let path = &path;
            let state = &state;
            async move {
                let response = state.search.call(Method::POST, path, Some(&body)).await?;
                Ok::<_, SearchError>((*role, response["hits"].clone()))
            }
        });
        let found = futures_util::future::try_join_all(searches).await.map_err(fail)?;
        let mut by_role: HashMap<&str, std::vec::IntoIter<Value>> = found
            .into_iter()
            .map(|(role, hits)| {
                let hits = hits.as_array().cloned().unwrap_or_default();
                (role, hits.into_iter())
            })
            .collect();
        let hits: Vec<Value> = slots
            .iter()
            .filter_map(|role| by_role.get_mut(role).and_then(Iterator::next))
            .collect();
        (Value::Array(hits), total)
    } else {
        let body = json!({
            "q": q,
            // Проверенную роль (a-z и `_`) безопасно подставлять в строку фильтра.
            "filter": role.as_ref().map(|role| format!("known_for = {role}")),
            "attributesToRetrieve": RETRIEVE,
        });
        match search_exact(&state.search, &path, body, limit, offset).await {
            Ok((response, total)) => (response["hits"].clone(), total),
            Err(error) if error.code() == Some("index_not_found") => (json!([]), 0),
            Err(error) => return Err(fail(error)),
        }
    };
    let items: Vec<PersonHit> = serde_json::from_value(hits)
        .map_err(|e| AppError::Internal(format!("unexpected people hits: {e}")))?;
    Ok(Json(PeoplePage {
        items,
        total,
        limit,
        offset,
        facets,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
    }

    #[test]
    fn mix_alternates_roles_and_actors_get_double_share() {
        let counts = counts(&[("actor", 100), ("author", 50), ("director", 40), ("game", 30)]);
        let order = mix_order(&counts);
        assert_eq!(order, ["actor", "author", "director", "actor", "game"]);
        let (slots, windows) = mix_plan(&order, &counts, 10, 0);
        assert_eq!(slots.iter().filter(|r| **r == "actor").count(), 4);
        assert_eq!(windows["author"], (0, 2));
    }

    #[test]
    fn mix_pages_are_contiguous_and_skip_exhausted_roles() {
        let counts = counts(&[("actor", 5), ("author", 1)]);
        let order = mix_order(&counts);
        let (first, w1) = mix_plan(&order, &counts, 3, 0);
        let (second, w2) = mix_plan(&order, &counts, 3, 3);
        let (all, _) = mix_plan(&order, &counts, 6, 0);
        assert_eq!([first.clone(), second.clone()].concat(), all);
        // Роль, чьи люди закончились, на второй странице не нужна.
        assert_eq!(all.len(), 6);
        assert_eq!(w1["actor"].0, 0);
        assert_eq!(w2["actor"].0, w1["actor"].1);
        assert!(!w2.contains_key("author"));
    }
}
