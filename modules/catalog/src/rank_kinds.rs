//! Балл «Рекомендуемые» и вес популярности в поиске — по видам (`catalog_rank_kinds`).
//!
//! `score = wq·q + wp·p + wr·r + ws·s + wc·c` у активных сущностей вида:
//! - `q` — байесовский рейтинг `(v·r + m·C)/(v + m)`: IMDb (голосов ≥ `min_votes`), иначе оценки
//!   сайта; `m` — `trust_votes`, `C` — среднее по виду; шкала 5 → 0 … 9 → 1;
//! - `p` — популярность: TMDB popularity (фильмы, сериалы), работы автора (книги); `ln(1+x)` к
//!   99-му перцентилю вида;
//! - `r` — свежесть `0.5^(возраст / half_life_years)`; ещё не вышедшие — 0 (у них «Скоро»);
//! - `s` — активность на сайте: число оценок, `ln` к 99-му перцентилю;
//! - `c` — полнота карточки: обложка 0,6 + описание 0,4.
//!
//! Всё считает PostgreSQL одним проходом на вид и пишет в `entity_stats.rank_score`; слой Б
//! получает только пары (номер, балл) — [`crate::index::sync::apply_scores`], без Tantivy.
//! `search_popularity` — доля популярности в релевантности поиска (0 — только текст, 1 — только
//! популярность), [`crate::index::weights`]. У строки `person` есть только она.

use crate::jobs::{self, DoneEvent, Reporter, Step, StepStatus};
use axum::extract::State;
use axum::response::Response;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path as UrlPath, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::{PgExecutor, PgPool};
use utoipa::{IntoParams, ToSchema};

/// Виды с баллом (у людей — только вес в поиске).
pub const SCORED_KINDS: [&str; 5] = ["movie", "series", "book", "game", "painting"];
const ALL_KINDS: [&str; 6] = ["movie", "series", "book", "game", "painting", "person"];

/// Настройки сортировки вида.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct KindRanking {
    /// `movie`, `series`, `book`, `game`, `painting`, `person`; в запросе не нужен (вид — из
    /// адреса), присланный игнорируется.
    #[serde(default)]
    pub kind: String,
    /// Вес рейтинга (IMDb или оценки сайта), 0–10.
    #[schema(example = 0.5)]
    pub weight_quality: f64,
    /// Вес популярности (TMDB или работы автора), 0–10.
    #[schema(example = 0.3)]
    pub weight_popularity: f64,
    /// Вес свежести, 0–10.
    #[schema(example = 0.2)]
    pub weight_recency: f64,
    /// Вес активности на сайте (число оценок), 0–10.
    #[schema(example = 0.1)]
    pub weight_site: f64,
    /// Вес полноты карточки (обложка, описание), 0–10.
    #[schema(example = 0.0)]
    pub weight_complete: f64,
    /// Доля популярности в релевантности поиска, 0–1 (0 — только текст).
    #[schema(example = 0.6)]
    pub search_popularity: f64,
    /// «Доверительные» голоса байесовского рейтинга.
    #[schema(example = 5000)]
    pub trust_votes: i32,
    /// Через сколько лет свежесть падает вдвое, 0.1–100.
    #[schema(example = 4.0)]
    pub half_life_years: f64,
    /// Меньше голосов IMDb — рейтинг IMDb не учитывается.
    #[schema(example = 1000)]
    pub min_votes: i32,
    /// Когда сохранены; в запросе игнорируется.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct KindPreviewQuery {
    /// 1–200, по умолчанию 50.
    pub limit: Option<i64>,
}

/// Строка предпросмотра: сущность и составляющие балла.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct KindPreviewRow {
    pub slug: String,
    pub title: String,
    pub release_date: Option<NaiveDate>,
    /// Байесовский рейтинг (`null` — нет оценок).
    pub quality: Option<f64>,
    /// Вклад рейтинга, популярности, свежести, сайта, полноты; `score` — их сумма.
    pub q_part: f64,
    pub p_part: f64,
    pub r_part: f64,
    pub s_part: f64,
    pub c_part: f64,
    pub score: f64,
}

const COLUMNS: &str = "kind, weight_quality, weight_popularity, weight_recency, weight_site, \
     weight_complete, search_popularity, trust_votes, half_life_years, min_votes, updated_at";

/// Составляющие балла вида `$9` (параметры $1..$8 — настройки):
/// `entity_id, quality, q_part, p_part, r_part, s_part, c_part`.
const FORMULA: &str = "
    WITH authors AS (
        -- Работы самого плодовитого автора книги: один проход с группировкой (только книги).
        SELECT c.entity_id, max(pc.active_credits)::float8 AS works
        FROM entity_credits c
        JOIN person_credit_counts pc ON pc.person_id = c.person_id
        WHERE $9 = 'book' AND c.role = 'author'
        GROUP BY c.entity_id
    ),
    base AS (
        SELECT e.id AS entity_id, e.release_date,
               s.imdb_rating, coalesce(s.imdb_votes, 0) AS imdb_votes,
               r.average AS site_avg, coalesce(r.count, 0) AS site_count,
               CASE WHEN e.kind = 'book' THEN coalesce(a.works, 0)
                    ELSE coalesce(s.tmdb_popularity, 0) END AS pop,
               (CASE WHEN e.cover_url IS NOT NULL THEN 0.6 ELSE 0 END
                -- octet_length — по заголовку значения, длинное описание не распаковывается.
                + CASE WHEN octet_length(e.description) > 0 THEN 0.4 ELSE 0 END) AS complete
        FROM entities e
        LEFT JOIN entity_stats s ON s.entity_id = e.id
        LEFT JOIN entity_ratings r ON r.entity_id = e.id
        LEFT JOIN authors a ON a.entity_id = e.id
        WHERE e.kind::text = $9 AND e.active AND (e.kind <> 'book' OR e.parent_id IS NULL)
    ),
    c AS (
        SELECT avg(imdb_rating) FILTER (WHERE imdb_votes >= $6 AND imdb_rating IS NOT NULL)
                   AS imdb_mean,
               avg(site_avg) FILTER (WHERE site_count > 0) AS site_mean,
               percentile_cont(0.99) WITHIN GROUP (ORDER BY pop) AS p99,
               percentile_cont(0.99) WITHIN GROUP (ORDER BY site_count) AS s99
        FROM base
    ),
    q AS (
        SELECT b.*, c.p99, c.s99,
               CASE WHEN b.imdb_rating IS NOT NULL AND b.imdb_votes > 0 AND b.imdb_votes >= $6
                        THEN (b.imdb_votes * b.imdb_rating + $4 * c.imdb_mean) / (b.imdb_votes + $4)
                    WHEN b.site_count > 0
                        THEN (b.site_count * b.site_avg + $4 * coalesce(c.site_mean, b.site_avg))
                             / (b.site_count + $4)
               END AS quality
        FROM base b, c
    )
    SELECT entity_id, quality,
           $1 * coalesce(least(greatest((quality - 5) / 4, 0), 1), 0) AS q_part,
           $2 * coalesce(least(ln(1 + pop) / nullif(ln(1 + p99), 0), 1), 0) AS p_part,
           $3 * CASE WHEN release_date IS NULL OR release_date > current_date THEN 0
                     ELSE power(0.5, ((current_date - release_date) / 365.25) / $5) END AS r_part,
           $7 * coalesce(least(ln(1 + site_count) / nullif(ln(1 + greatest(s99, 1)), 0), 1), 0)
               AS s_part,
           $8 * complete AS c_part
    FROM q";

fn bind_formula<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    s: &KindRanking,
    kind: &'q str,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    query
        .bind(s.weight_quality)
        .bind(s.weight_popularity)
        .bind(s.weight_recency)
        .bind(f64::from(s.trust_votes))
        .bind(s.half_life_years)
        .bind(s.min_votes)
        .bind(s.weight_site)
        .bind(s.weight_complete)
        .bind(kind)
}

fn check_kind(kind: &str) -> AppResult<&'static str> {
    ALL_KINDS
        .iter()
        .find(|k| **k == kind)
        .copied()
        .ok_or_else(|| AppError::NotFound)
}

fn validate(s: &KindRanking) -> AppResult<()> {
    let bad =
        |field: &str, range: &str| Err(AppError::BadRequest(format!("{field} must be {range}")));
    for (field, value) in [
        ("weight_quality", s.weight_quality),
        ("weight_popularity", s.weight_popularity),
        ("weight_recency", s.weight_recency),
        ("weight_site", s.weight_site),
        ("weight_complete", s.weight_complete),
    ] {
        if !value.is_finite() || !(0.0..=10.0).contains(&value) {
            return bad(field, "between 0 and 10");
        }
    }
    if !s.search_popularity.is_finite() || !(0.0..=1.0).contains(&s.search_popularity) {
        return bad("search_popularity", "between 0 and 1");
    }
    if !s.half_life_years.is_finite() || !(0.1..=100.0).contains(&s.half_life_years) {
        return bad("half_life_years", "between 0.1 and 100");
    }
    if !(0..=10_000_000).contains(&s.trust_votes) {
        return bad("trust_votes", "between 0 and 10000000");
    }
    if !(0..=10_000_000).contains(&s.min_votes) {
        return bad("min_votes", "between 0 and 10000000");
    }
    Ok(())
}

pub(crate) async fn settings(
    db: impl PgExecutor<'_>,
    kind: &str,
) -> Result<KindRanking, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM catalog_rank_kinds WHERE kind = $1"
    ))
    .bind(kind)
    .fetch_one(db)
    .await
}

/// Пересчитать балл вида по сохранённым настройкам: одним проходом во временную таблицу, затем
/// запись только изменившихся. Возвращает, у скольких сущностей балл есть.
pub(crate) async fn recompute_kind(db: &PgPool, kind: &str) -> Result<i64, sqlx::Error> {
    let mut tx = db.begin().await?;
    let s = settings(&mut *tx, kind).await?;
    // Слабое железо: без параллельных воркеров (их общая память в /dev/shm контейнера — 64 МБ).
    sqlx::query("SET LOCAL work_mem = '64MB'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL max_parallel_workers_per_gather = 0")
        .execute(&mut *tx)
        .await?;
    let create = format!(
        "CREATE TEMP TABLE ranked ON COMMIT DROP AS
         SELECT entity_id, quality, q_part + p_part + r_part + s_part + c_part AS score
         FROM ({FORMULA}) f"
    );
    bind_formula(sqlx::query(&create), &s, kind)
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE UNIQUE INDEX ON ranked (entity_id)")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO entity_stats (entity_id, quality, rank_score)
         SELECT entity_id, quality, score FROM ranked
         ON CONFLICT (entity_id) DO UPDATE
             SET quality = EXCLUDED.quality, rank_score = EXCLUDED.rank_score
             WHERE (entity_stats.quality, entity_stats.rank_score)
                   IS DISTINCT FROM (EXCLUDED.quality, EXCLUDED.rank_score)",
    )
    .execute(&mut *tx)
    .await?;
    // Скрытые и неактивные сущности вида — без балла.
    sqlx::query(
        "UPDATE entity_stats s SET quality = NULL, rank_score = NULL
         FROM entities e
         WHERE e.id = s.entity_id AND e.kind::text = $1
           AND (s.quality IS NOT NULL OR s.rank_score IS NOT NULL)
           AND NOT EXISTS (SELECT 1 FROM ranked r WHERE r.entity_id = s.entity_id)",
    )
    .bind(kind)
    .execute(&mut *tx)
    .await?;
    let ranked: i64 = sqlx::query_scalar("SELECT count(*) FROM ranked")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ranked)
}

/// Все виды с баллом. Возвращает, у скольких сущностей балл есть.
pub(crate) async fn recompute_all(db: &PgPool) -> Result<i64, sqlx::Error> {
    let mut total = 0;
    for kind in SCORED_KINDS {
        total += recompute_kind(db, kind).await?;
    }
    Ok(total)
}

/// Настройки сортировки всех видов и людей.
#[utoipa::path(
    get, operation_id = "list_rank_kinds", path = "/admin/ranking/kinds", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Настройки по видам", body = Vec<KindRanking>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
) -> AppResult<Json<Vec<KindRanking>>> {
    let rows: Vec<KindRanking> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM catalog_rank_kinds
         ORDER BY array_position(ARRAY['movie','series','book','game','painting','person'], kind)"
    ))
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Предпросмотр: топ вида по формуле с настройками из тела, с составляющими балла. Ничего не
/// записывает. У `person` — пусто (известность людей считается из их работ).
#[utoipa::path(
    post, operation_id = "preview_rank_kind", path = "/admin/ranking/kinds/{kind}/preview",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(("kind" = String, Path, description = "Вид"), KindPreviewQuery),
    request_body = KindRanking,
    responses(
        (status = 200, description = "Топ по формуле", body = Vec<KindPreviewRow>),
        (status = 400, description = "Неверные настройки", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого вида", body = ErrorBody),
    )
)]
pub async fn preview(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    UrlPath(kind): UrlPath<String>,
    Query(query): Query<KindPreviewQuery>,
    JsonBody(s): JsonBody<KindRanking>,
) -> AppResult<Json<Vec<KindPreviewRow>>> {
    let kind = check_kind(&kind)?;
    validate(&s)?;
    if kind == "person" {
        return Ok(Json(Vec::new()));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let sql = format!(
        "SELECT e.slug, e.title, e.release_date, f.quality, f.q_part, f.p_part, f.r_part,
                f.s_part, f.c_part, f.q_part + f.p_part + f.r_part + f.s_part + f.c_part AS score
         FROM ({FORMULA}) f
         JOIN entities e ON e.id = f.entity_id
         ORDER BY score DESC, e.id
         LIMIT $10"
    );
    let rows = bind_formula(sqlx::query(&sql), &s, kind)
        .bind(limit)
        .fetch_all(&state.db)
        .await?
        .iter()
        .map(sqlx::FromRow::from_row)
        .collect::<Result<Vec<KindPreviewRow>, _>>()?;
    Ok(Json(rows))
}

/// Сохранить настройки вида и применить, потоком: `rank` (пересчёт балла вида в БД), `search`
/// (баллы в слой Б без перестройки Tantivy, веса поиска), затем `done`. Статические страницы
/// не пересобираются.
#[utoipa::path(
    put, operation_id = "save_rank_kind", path = "/admin/ranking/kinds/{kind}/stream",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(("kind" = String, Path, description = "Вид")),
    request_body = KindRanking,
    responses(
        (status = 200, description = "Сохранено; поток пересчёта", content_type = "text/event-stream",
            body = jobs::JobEvent),
        (status = 400, description = "Неверные настройки", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого вида", body = ErrorBody),
    )
)]
pub async fn save_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    UrlPath(kind): UrlPath<String>,
    JsonBody(s): JsonBody<KindRanking>,
) -> AppResult<Response> {
    let kind = check_kind(&kind)?;
    validate(&s)?;
    sqlx::query(
        "UPDATE catalog_rank_kinds SET weight_quality = $2, weight_popularity = $3,
             weight_recency = $4, weight_site = $5, weight_complete = $6, search_popularity = $7,
             trust_votes = $8, half_life_years = $9, min_votes = $10, updated_at = now(),
             updated_by = $11
         WHERE kind = $1",
    )
    .bind(kind)
    .bind(s.weight_quality)
    .bind(s.weight_popularity)
    .bind(s.weight_recency)
    .bind(s.weight_site)
    .bind(s.weight_complete)
    .bind(s.search_popularity)
    .bind(s.trust_votes)
    .bind(s.half_life_years)
    .bind(s.min_votes)
    .bind(admin.id)
    .execute(&state.db)
    .await?;
    tracing::info!(admin_id = %admin.id, kind, ?s, "rank kind settings saved");
    let scored = kind != "person";
    let steps: &[Step] = if scored {
        &[Step::Rank, Step::Search]
    } else {
        &[Step::Search]
    };
    Ok(jobs::stream(steps, move |report| async move {
        DoneEvent {
            ok: apply(&state, kind, scored, &report).await,
            ..DoneEvent::default()
        }
    }))
}

async fn apply(state: &AppState, kind: &'static str, scored: bool, report: &Reporter) -> bool {
    if scored {
        let started = report.start(Step::Rank);
        match recompute_kind(&state.db, kind).await {
            Ok(n) => report.finish(
                Step::Rank,
                StepStatus::Done,
                format!("Балл «Рекомендуемые» у {n}"),
                started,
            ),
            Err(error) => {
                report.finish(Step::Rank, StepStatus::Failed, error.to_string(), started);
                return false;
            }
        }
    }
    let started = report.start(Step::Search);
    let result = async {
        crate::index::weights::reload(&state.db).await?;
        if scored {
            crate::index::sync::apply_scores(state).await?;
        }
        Ok::<_, crate::search::BoxError>(())
    }
    .await;
    match result {
        Ok(()) => {
            report.finish(
                Step::Search,
                StepStatus::Done,
                "Слой Б и веса поиска",
                started,
            );
            true
        }
        Err(error) => {
            report.finish(Step::Search, StepStatus::Failed, error.to_string(), started);
            false
        }
    }
}
