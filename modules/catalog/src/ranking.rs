//! Балл «Рекомендуемые» (сортировка каталога по умолчанию) и его настройки в админке.
//!
//! `score = weight_quality·q + weight_popularity·p + weight_recency·r` у вышедших фильмов
//! (`release_date ≤ сегодня`) с `imdb_votes ≥ min_votes`:
//! - `q` — байесовский рейтинг IMDb `(v·r + m·C)/(v + m)` (`m` = `trust_votes`, `C` — средний по
//!   таким фильмам), шкала 5 → 0 … 9 → 1 с обрезкой;
//! - `p` — `ln(1+popularity)/ln(1+p99)` (99-й перцентиль TMDB popularity), обрезка до 1;
//! - `r` — `0.5^(возраст в годах / half_life_years)`: свежее выше, классика не исчезает.
//!
//! Остальные — без балла (в конце сортировки). Балл и `quality` хранятся в `entity_stats`,
//! пересчитываются после «Обновить рейтинги» и сохранения настроек (`PUT /admin/ranking/stream`).
//! Предпросмотр (`POST /admin/ranking/preview`) считает ту же формулу с настройками из запроса,
//! ничего не записывая.

use crate::jobs::{self, DoneEvent, Reporter, Step, StepStatus};
use crate::models::{RankPreviewQuery, RankPreviewRow, RankingSettings};
use crate::search::{reindex_locked, ReindexError, ReindexLock, ReindexOptions};
use axum::extract::State;
use axum::response::Response;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::{PgExecutor, PgPool};

const SETTINGS_COLUMNS: &str = "weight_quality, weight_popularity, weight_recency, trust_votes, \
     half_life_years, min_votes, soon_days, updated_at";

/// Составляющие балла по формуле (параметры $1..$6 — настройки) у вышедших фильмов с
/// `imdb_votes ≥ $6`: `entity_id, quality, q_part, p_part, r_part`.
const FORMULA: &str = "
    WITH base AS (
        SELECT s.entity_id, s.imdb_rating, s.imdb_votes, s.tmdb_popularity, e.release_date
        FROM entity_stats s JOIN entities e ON e.id = s.entity_id
        WHERE e.kind = 'movie' AND e.release_date <= current_date
          AND s.imdb_rating IS NOT NULL AND s.imdb_votes >= $6
    ),
    c AS (
        SELECT avg(imdb_rating) AS mean,
               percentile_cont(0.99) WITHIN GROUP (ORDER BY coalesce(tmdb_popularity, 0)) AS p99
        FROM base
    ),
    q AS (
        SELECT b.*, (b.imdb_votes * b.imdb_rating + $4 * c.mean) / (b.imdb_votes + $4) AS quality,
               c.p99
        FROM base b, c
    )
    SELECT entity_id, quality,
           $1 * least(greatest((quality - 5) / 4, 0), 1) AS q_part,
           $2 * coalesce(least(ln(1 + coalesce(tmdb_popularity, 0)) / nullif(ln(1 + p99), 0), 1), 0)
               AS p_part,
           $3 * power(0.5, ((current_date - release_date) / 365.25) / $5) AS r_part
    FROM q";

pub(crate) async fn settings(db: impl PgExecutor<'_>) -> Result<RankingSettings, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {SETTINGS_COLUMNS} FROM catalog_ranking"))
        .fetch_one(db)
        .await
}

fn validate(s: &RankingSettings) -> AppResult<()> {
    let bad =
        |field: &str, range: &str| Err(AppError::BadRequest(format!("{field} must be {range}")));
    for (field, value) in [
        ("weight_quality", s.weight_quality),
        ("weight_popularity", s.weight_popularity),
        ("weight_recency", s.weight_recency),
    ] {
        if !value.is_finite() || !(0.0..=10.0).contains(&value) {
            return bad(field, "between 0 and 10");
        }
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
    if !(0..=365).contains(&s.soon_days) {
        return bad("soon_days", "between 0 and 365");
    }
    Ok(())
}

/// Пересчитать `quality` и `rank_score` всех строк `entity_stats` по сохранённым настройкам.
/// Возвращает, у скольких строк балл есть.
pub(crate) async fn recompute(db: &PgPool) -> Result<i64, sqlx::Error> {
    let mut tx = db.begin().await?;
    let s = settings(&mut *tx).await?;
    sqlx::query(&format!(
        "CREATE TEMP TABLE ranked ON COMMIT DROP AS
         SELECT entity_id, quality, q_part + p_part + r_part AS score FROM ({FORMULA}) f"
    ))
    .bind(s.weight_quality)
    .bind(s.weight_popularity)
    .bind(s.weight_recency)
    .bind(f64::from(s.trust_votes))
    .bind(s.half_life_years)
    .bind(s.min_votes)
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE UNIQUE INDEX ON ranked (entity_id)")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE entity_stats s SET quality = r.quality, rank_score = r.score
         FROM ranked r
         WHERE s.entity_id = r.entity_id
           AND (s.quality, s.rank_score) IS DISTINCT FROM (r.quality, r.score)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE entity_stats s SET quality = NULL, rank_score = NULL
         WHERE (s.quality IS NOT NULL OR s.rank_score IS NOT NULL)
           AND NOT EXISTS (SELECT 1 FROM ranked r WHERE r.entity_id = s.entity_id)",
    )
    .execute(&mut *tx)
    .await?;
    let ranked: i64 = sqlx::query_scalar("SELECT count(*) FROM ranked")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ranked)
}

/// Шаг `rank` в потоке: пересчёт балла.
pub(crate) async fn recompute_step(db: &PgPool, report: &Reporter) -> bool {
    let started = report.start(Step::Rank);
    match recompute(db).await {
        Ok(ranked) => {
            report.finish(
                Step::Rank,
                StepStatus::Done,
                format!("Балл «Рекомендуемые» у {ranked} фильмов"),
                started,
            );
            true
        }
        Err(error) => {
            report.finish(Step::Rank, StepStatus::Failed, error.to_string(), started);
            false
        }
    }
}

/// Настройки балла «Рекомендуемые» и окна «Скоро премьера».
#[utoipa::path(
    get, operation_id = "get_ranking", path = "/admin/ranking", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Настройки", body = RankingSettings),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
) -> AppResult<Json<RankingSettings>> {
    Ok(Json(settings(&state.db).await?))
}

/// Предпросмотр: топ вышедших фильмов по формуле с настройками из тела — с составляющими балла и
/// признаком `active`. Ничего не записывает: подбирать веса, глядя на список.
#[utoipa::path(
    post, operation_id = "preview_ranking", path = "/admin/ranking/preview", tag = "catalog-admin",
    security(("bearer" = [])),
    params(RankPreviewQuery),
    request_body = RankingSettings,
    responses(
        (status = 200, description = "Топ по формуле", body = Vec<RankPreviewRow>),
        (status = 400, description = "Неверные настройки", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn preview(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Query(query): Query<RankPreviewQuery>,
    JsonBody(s): JsonBody<RankingSettings>,
) -> AppResult<Json<Vec<RankPreviewRow>>> {
    validate(&s)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let rows = sqlx::query_as(&format!(
        "SELECT e.slug, e.title, e.release_date, e.active, st.imdb_rating, st.imdb_votes,
                st.tmdb_popularity, f.quality, f.q_part, f.p_part, f.r_part,
                f.q_part + f.p_part + f.r_part AS score
         FROM ({FORMULA}) f
         JOIN entities e ON e.id = f.entity_id
         JOIN entity_stats st ON st.entity_id = f.entity_id
         ORDER BY score DESC, e.id
         LIMIT $7"
    ))
    .bind(s.weight_quality)
    .bind(s.weight_popularity)
    .bind(s.weight_recency)
    .bind(f64::from(s.trust_votes))
    .bind(s.half_life_years)
    .bind(s.min_votes)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Сохранить настройки и применить, потоком: шаг `rank` (пересчёт балла), затем перестройка
/// поиска без проверки размера (меняются сортировка и окно «Скоро премьера»), последним `done`.
/// Уже идёт перестройка или операция TMDB — `409` до потока.
#[utoipa::path(
    put, operation_id = "save_ranking", path = "/admin/ranking/stream", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = RankingSettings,
    responses(
        (status = 200, description = "Сохранено; поток пересчёта", content_type = "text/event-stream",
            body = jobs::JobEvent),
        (status = 400, description = "Неверные настройки", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт перестройка поиска или операция TMDB", body = ErrorBody),
    )
)]
pub async fn save_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(s): JsonBody<RankingSettings>,
) -> AppResult<Response> {
    validate(&s)?;
    let lock = match ReindexLock::acquire(&state).await {
        Ok(lock) => Some(lock),
        Err(ReindexError::Disabled) => None,
        Err(error) => return Err(error.into()),
    };
    sqlx::query(
        "UPDATE catalog_ranking SET weight_quality = $1, weight_popularity = $2,
             weight_recency = $3, trust_votes = $4, half_life_years = $5, min_votes = $6,
             soon_days = $7, updated_at = now(), updated_by = $8",
    )
    .bind(s.weight_quality)
    .bind(s.weight_popularity)
    .bind(s.weight_recency)
    .bind(s.trust_votes)
    .bind(s.half_life_years)
    .bind(s.min_votes)
    .bind(s.soon_days)
    .bind(admin.id)
    .execute(&state.db)
    .await?;
    tracing::info!(admin_id = %admin.id, ?s, "ranking settings saved");

    let reindex = ReindexOptions {
        force: true,
        revalidate: false,
    };
    let mut steps = vec![Step::Rank];
    if lock.is_some() {
        steps.extend(reindex.steps());
    }
    Ok(jobs::stream(&steps, move |report| async move {
        let mut done = DoneEvent::default();
        let ranked = recompute_step(&state.db, &report).await;
        done.ok = match lock {
            Some(lock) if ranked => {
                let result = reindex_locked(&state, &lock, reindex, &report).await;
                lock.release(&state).await;
                result.is_ok()
            }
            Some(lock) => {
                lock.release(&state).await;
                false
            }
            None => ranked,
        };
        done
    }))
}
