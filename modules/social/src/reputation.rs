//! Рейтинг пользователя («карма»): [`BASE`] за регистрацию плюс веса за чужие лайки.
//!
//! Считается **не** счётчиком на триггерах, а полным пересчётом раз в сутки: слагаемых будет
//! больше (каналы, темы форума), веса будут меняться, а пересчёт с нуля всегда даёт одно и то
//! же число — его не нужно чинить после каждой правки правил. Весь пересчёт — один запрос,
//! строки заводятся только тем, у кого есть хоть одно слагаемое; остальным рейтинг равен
//! [`BASE`] и хранить его незачем.
//!
//! Границы: считаем только по своим таблицам (`user_likes`, `review_votes` + `reviews`,
//! `collection_reactions` + `collections`). В `users` не заглядываем — поэтому пересчёт не знает
//! о пользователях без единого лайка, и это ровно те, у кого [`BASE`].

use crate::models::Reputation;
use axum::extract::State;
use axum::Json;
use serde_json::Value;
use shared::error::ErrorBody;
use shared::{AdminUser, AppResult, AppState};
use uuid::Uuid;

/// Сколько даётся за регистрацию.
pub const BASE: i32 = 30;
/// Лайк профиля: человек понравился целиком, это дороже одной удачной рецензии.
pub const WEIGHT_PROFILE_LIKE: i32 = 2;
/// Лайк чужой рецензии (`review_votes.value = 1`). Дизлайки не вычитаются: рейтинг
/// показывает пользу, а не разрешает минусовать за непопулярное мнение.
pub const WEIGHT_REVIEW_LIKE: i32 = 1;
/// Реакция на твою коллекцию.
pub const WEIGHT_COLLECTION_REACTION: i32 = 1;

/// Как часто пересчитывать.
pub const INTERVAL_SECS: u64 = 24 * 60 * 60;
/// Ключ advisory-блокировки: при нескольких экземплярах приложения пересчёт делает один.
const LOCK_KEY: i64 = 0x7265_7075_7400_0001;

/// Один запрос: считает слагаемые, пишет строки и стирает те, у кого слагаемых не осталось
/// (их рейтинг снова [`BASE`]). Возвращает, сколько строк осталось.
pub async fn recalculate(state: &AppState) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "WITH profile AS (
             SELECT liked_id AS user_id, count(*) AS n FROM user_likes GROUP BY 1
         ), review AS (
             SELECT r.user_id, count(*) AS n
               FROM review_votes v JOIN reviews r ON r.id = v.review_id
              WHERE v.value = 1 GROUP BY 1
         ), collection AS (
             SELECT c.user_id, count(*) AS n
               FROM collection_reactions cr JOIN collections c ON c.id = cr.collection_id
              GROUP BY 1
         ), totals AS (
             SELECT user_id,
                    sum(profile_likes) AS profile_likes,
                    sum(review_likes) AS review_likes,
                    sum(collection_reactions) AS collection_reactions
               FROM (
                   SELECT user_id, n AS profile_likes, 0 AS review_likes, 0 AS collection_reactions
                     FROM profile
                   UNION ALL
                   SELECT user_id, 0, n, 0 FROM review
                   UNION ALL
                   SELECT user_id, 0, 0, n FROM collection
               ) AS parts
              GROUP BY user_id
         )
         INSERT INTO user_reputation (user_id, score, parts, computed_at)
         SELECT user_id,
                $1 + $2 * profile_likes + $3 * review_likes + $4 * collection_reactions,
                jsonb_build_object(
                    'profile_likes', profile_likes,
                    'review_likes', review_likes,
                    'collection_reactions', collection_reactions
                ),
                now()
           FROM totals
         ON CONFLICT (user_id) DO UPDATE
            SET score = EXCLUDED.score,
                parts = EXCLUDED.parts,
                computed_at = EXCLUDED.computed_at",
    )
    .bind(BASE)
    .bind(WEIGHT_PROFILE_LIKE)
    .bind(WEIGHT_REVIEW_LIKE)
    .bind(WEIGHT_COLLECTION_REACTION)
    .execute(&mut *tx)
    .await?;
    // Не обновлённые этим проходом слагаемых больше не имеют: у них снова BASE. `now()` внутри
    // транзакции неподвижен, поэтому только что записанные строки (`computed_at = now()`)
    // сюда не попадают, а строки прошлых проходов — попадают все.
    sqlx::query("DELETE FROM user_reputation WHERE computed_at < now()")
        .execute(&mut *tx)
        .await?;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_reputation")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows as u64)
}

/// Рейтинг пользователя: строки нет — [`BASE`] и нули в разбивке.
pub(crate) async fn of(state: &AppState, user_id: Uuid) -> AppResult<Reputation> {
    let row: Option<(i32, Value, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT score, parts, computed_at FROM user_reputation WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(match row {
        Some((score, parts, computed_at)) => Reputation {
            score,
            parts,
            computed_at: Some(computed_at),
        },
        None => Reputation {
            score: BASE,
            parts: serde_json::json!({
                "profile_likes": 0,
                "review_likes": 0,
                "collection_reactions": 0,
            }),
            computed_at: None,
        },
    })
}

/// Суточный пересчёт: первый проход — сразу после старта. Несколько экземпляров приложения
/// договариваются advisory-блокировкой: кто её взял, тот и считает, остальные пропускают проход.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(INTERVAL_SECS));
        loop {
            ticker.tick().await;
            match run_locked(&state).await {
                Ok(Some(rows)) => tracing::info!(rows, "user reputation recalculated"),
                // Считает другой экземпляр — это норма, не ошибка.
                Ok(None) => tracing::debug!("user reputation recalculation skipped: locked"),
                Err(error) => tracing::warn!(%error, "user reputation recalculation failed"),
            }
        }
    });
}

/// Пересчёт под advisory-блокировкой. `None` — блокировку держит кто-то другой.
async fn run_locked(state: &AppState) -> AppResult<Option<u64>> {
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(LOCK_KEY)
        .fetch_one(&state.db)
        .await?;
    if !locked {
        return Ok(None);
    }
    let result = recalculate(state).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .execute(&state.db)
        .await;
    result.map(Some)
}

/// Пересчитать рейтинги сейчас: для админки и для тестов, чтобы не ждать суточного прохода.
#[utoipa::path(
    post, operation_id = "admin_recalculate_reputation", path = "/admin/reputation/recalc",
    tag = "social-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Пересчитано", body = RecalcResult),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn recalc(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<Json<RecalcResult>> {
    let rows = recalculate(&state).await?;
    tracing::info!(admin_id = %admin.id, rows, "user reputation recalculated by admin");
    Ok(Json(RecalcResult { rows }))
}

/// Сколько пользователей получили строку рейтинга (у остальных — [`BASE`]).
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RecalcResult {
    #[schema(example = 128)]
    pub rows: u64,
}
