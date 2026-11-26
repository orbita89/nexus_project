//! Поиск людей на Tantivy ([`crate::index`]) и известность (`person_stats.fame`).
//!
//! Индекс строится вместе с работами (перестройка [`crate::search`]), правки админки попадают в
//! него через [`update_index`]. Документ компактный ([`PersonDoc`]): имя, имя латиницей, фото,
//! дата рождения, основная роль и известность — без биографии и фильмографии.
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

use crate::models::{page_bounds, PeopleFacets, PeoplePage, PeopleSearchQuery, PersonHit};
use crate::search::{unavailable, BoxError};
use crate::validate;
use axum::extract::State;
use axum::Json;
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::{AppError, AppResult, AppState};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Людей в одной пачке перестройки: документы маленькие.
pub const BATCH: usize = 2_000;

/// Человек попадает в индекс, только если у него есть работа в активной сущности.
pub(crate) const INDEXED_PERSON: &str = "EXISTS (SELECT 1 FROM person_credit_counts pc
                                      WHERE pc.person_id = p.id AND pc.active_credits > 0)";

/// Число работ человека (без сезонов) и до 3 самых известных; `p` — таблица людей снаружи.
pub(crate) const WORKS_LATERAL: &str = "LEFT JOIN LATERAL (
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

/// [`WORKS_LATERAL`] сразу для всех людей — во временную таблицу `person_works_tmp` сессии.
/// Полная перестройка: один последовательный проход (~1,5 мин на 4 млн титров) вместо
/// LATERAL на каждого человека (точечные чтения титров и работ, ~20 мин на 1,2 млн людей).
pub(crate) const WORKS_ALL_TEMP: &str = "CREATE TEMP TABLE person_works_tmp AS
    WITH c AS (SELECT DISTINCT person_id, entity_id FROM entity_credits),
    x AS (SELECT c.person_id, e.slug, e.title, e.kind::text AS kind, e.cover_url,
                 extract(year FROM e.release_date)::int AS year,
                 row_number() OVER (PARTITION BY c.person_id
                                    ORDER BY coalesce(s.tmdb_votes, 0) DESC,
                                             e.release_date DESC NULLS LAST, e.id) AS rn
          FROM c JOIN entities e ON e.id = c.entity_id AND e.active AND e.kind <> 'season'
          LEFT JOIN entity_stats s ON s.entity_id = e.id)
    SELECT person_id, count(*) AS works_count,
           coalesce(jsonb_agg(jsonb_build_object('slug', slug, 'title', title, 'kind', kind,
                    'year', year, 'poster_url', cover_url) ORDER BY rn)
                    FILTER (WHERE rn <= 3), '[]'::jsonb) AS top_works
    FROM x GROUP BY person_id";

/// Создаёт `person_works_tmp` ([`WORKS_ALL_TEMP`]) на соединении `conn`; запросы пачек на нём же
/// соединяют её вместо [`WORKS_LATERAL`]: `LEFT JOIN person_works_tmp w ON w.person_id = p.id`.
pub(crate) async fn create_works_temp(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    drop_works_temp(conn).await?;
    sqlx::query("SET work_mem = '256MB'")
        .execute(&mut *conn)
        .await?;
    let created = sqlx::query(WORKS_ALL_TEMP).execute(&mut *conn).await;
    sqlx::query("RESET work_mem").execute(&mut *conn).await?;
    created?;
    sqlx::query("ALTER TABLE person_works_tmp ADD PRIMARY KEY (person_id)")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Убирает `person_works_tmp`: она живёт до конца сессии, а соединение вернётся в пул.
pub(crate) async fn drop_works_temp(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("DROP TABLE IF EXISTS person_works_tmp")
        .execute(conn)
        .await?;
    Ok(())
}

/// JOIN к `person_works_tmp` вместо [`WORKS_LATERAL`].
pub(crate) const WORKS_TEMP_JOIN: &str = "LEFT JOIN person_works_tmp w ON w.person_id = p.id";

/// Колонки документа; `p` — люди, `ps` — `person_stats`, `w` — [`WORKS_LATERAL`].
pub(crate) const DOC_COLUMNS: &str =
    "p.id, p.slug, p.full_name, p.original_name, p.birth_date, p.photo_url,
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

/// Поиск по индексу людей: страница, счётчики ролей и чередование ролей без запроса.
async fn search_tantivy(
    state: &AppState,
    q: &str,
    role: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<Json<PeoplePage>> {
    use crate::index::PeopleSort;
    let Some(Ok(index)) = crate::index::sync::open(state) else {
        return Err(unavailable());
    };
    let (q, role) = (q.to_owned(), role.map(str::to_owned));
    let (hits, total, counts) = shared::tantivy::blocking(move || {
        let counts = index.people_role_counts(&q)?;
        let (lim, off) = (limit as usize, offset as usize);
        if role.is_none() && q.is_empty() {
            let total: i64 = counts.values().sum();
            let order = mix_order(&counts);
            let (slots, windows) = if order.is_empty() {
                (Vec::new(), HashMap::new())
            } else {
                mix_plan(&order, &counts, limit, offset.min(total))
            };
            let mut by_role: HashMap<&str, std::vec::IntoIter<Value>> = HashMap::new();
            for (r, (start, count)) in &windows {
                let (hits, _) = index.search_people(
                    "",
                    Some(r),
                    PeopleSort::Fame,
                    *count as usize,
                    *start as usize,
                )?;
                by_role.insert(r, hits.into_iter());
            }
            let hits: Vec<Value> = slots
                .iter()
                .filter_map(|r| by_role.get_mut(r).and_then(Iterator::next))
                .collect();
            Ok((hits, total, counts))
        } else {
            let (hits, total) =
                index.search_people(&q, role.as_deref(), PeopleSort::Relevance, lim, off)?;
            Ok((hits, total as i64, counts))
        }
    })
    .await
    .map_err(|error: crate::index::IndexError| {
        tracing::warn!(%error, "people tantivy search failed");
        unavailable()
    })?;
    let items: Vec<PersonHit> = serde_json::from_value(Value::Array(hits))
        .map_err(|e| AppError::Internal(format!("unexpected people hits: {e}")))?;
    Ok(Json(PeoplePage {
        items,
        total,
        limit,
        offset,
        facets: PeopleFacets { known_for: counts },
    }))
}

/// Документ индекса людей.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub(crate) struct PersonDoc {
    pub(crate) id: Uuid,
    pub(crate) slug: String,
    pub(crate) full_name: String,
    /// Имя латиницей: «brad pitt» находит «Брэд Питт».
    pub(crate) original_name: Option<String>,
    pub(crate) birth_date: Option<NaiveDate>,
    pub(crate) photo_url: Option<String>,
    /// Основная роль (`actor`, `director`, ...): фильтр и подпись в выдаче.
    pub(crate) known_for: Option<String>,
    /// Известность: правило ранжирования `fame:desc`.
    pub(crate) fame: f64,
    /// Число работ: «N работ» на карточке.
    pub(crate) works_count: i64,
    /// Массив `{slug, title, kind, year, poster_url}`, до 3 элементов.
    pub(crate) top_works: Value,
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

/// Правка затронула людей: их известность пересчитывается, документы переотправляются (и в
/// теневой индекс идущей перестройки), скрытые и удалённые — удаляются. Ждёт применения.
pub(crate) async fn update_index(state: &AppState, people: &[Uuid]) -> Result<(), BoxError> {
    if people.is_empty() {
        return Ok(());
    }
    refresh_fame(&state.db, Some(people)).await?;
    crate::index::sync::update_people(state, people).await
}

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
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
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
    search_tantivy(&state, q, role.as_deref(), limit, offset).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
    }

    #[test]
    fn mix_alternates_roles_and_actors_get_double_share() {
        let counts = counts(&[
            ("actor", 100),
            ("author", 50),
            ("director", 40),
            ("game", 30),
        ]);
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
