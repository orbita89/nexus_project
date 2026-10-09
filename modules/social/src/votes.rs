//! Лайки и дизлайки рецензий. Счётчики — в самой рецензии (`likes_count`, `dislikes_count`), их
//! пишет триггер на `review_votes`. Свои голоса — отдельным запросом: публичный список рецензий
//! одинаков для всех и кэшируется.

use crate::models::{PutVote, VoteResult};
use crate::refs;
use crate::validate;
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AppError, AppResult, AppState, AuthUser};
use sqlx::PgConnection;
use std::collections::HashMap;
use uuid::Uuid;

/// Поставить лайк (`1`) или дизлайк (`-1`). Повторный вызов с другим `value` меняет голос.
#[utoipa::path(
    put, operation_id = "vote_review", path = "/reviews/{id}/vote", tag = "reviews",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id рецензии")),
    request_body = PutVote,
    responses(
        (status = 200, description = "Голос учтён", body = VoteResult),
        (status = 400, description = "`value` не 1 и не -1", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Своя рецензия (`code`: `own_review`)", body = ErrorBody),
        (status = 404, description = "Рецензия не найдена", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<PutVote>,
) -> AppResult<Json<VoteResult>> {
    if !matches!(req.value, 1 | -1) {
        return Err(AppError::BadRequest("value must be 1 or -1".into()));
    }
    let mut tx = state.db.begin().await?;
    let author = review_author(&mut tx, id).await?;
    if author == user.id {
        return Err(AppError::Forbidden.with_code("own_review"));
    }
    sqlx::query(
        "INSERT INTO review_votes (review_id, user_id, value) VALUES ($1, $2, $3)
         ON CONFLICT (review_id, user_id) DO UPDATE SET value = EXCLUDED.value
         WHERE review_votes.value <> EXCLUDED.value",
    )
    .bind(id)
    .bind(user.id)
    .bind(req.value)
    .execute(&mut *tx)
    .await
    .map_err(validate::missing_reference)?;
    let result = counts(&mut tx, id, Some(req.value)).await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// Снять свой голос. Голоса не было — тоже 200.
#[utoipa::path(
    delete, operation_id = "unvote_review", path = "/reviews/{id}/vote", tag = "reviews",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id рецензии")),
    responses(
        (status = 200, description = "Голоса нет, `value`: `null`", body = VoteResult),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Рецензия не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<VoteResult>> {
    let mut tx = state.db.begin().await?;
    review_author(&mut tx, id).await?;
    sqlx::query("DELETE FROM review_votes WHERE review_id = $1 AND user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    let result = counts(&mut tx, id, None).await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// Свои голоса за рецензии сущности: `{"<id рецензии>": 1 | -1}`. Одним запросом на карточку.
#[utoipa::path(
    get, operation_id = "list_own_review_votes", path = "/entities/{slug}/votes/mine",
    tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "id рецензии → голос",
            body = HashMap<String, i16>,
            example = json!({ "0199b2a4-6c1e-7d3a-9f00-1a2b3c4d5e6f": 1 })),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn mine(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<HashMap<Uuid, i16>>> {
    let entity = refs::entity(&state, &slug).await?;
    let votes: Vec<(Uuid, i16)> = sqlx::query_as(
        "SELECT v.review_id, v.value FROM review_votes v JOIN reviews r ON r.id = v.review_id
         WHERE v.user_id = $1 AND r.entity_id = $2",
    )
    .bind(user.id)
    .bind(entity.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(votes.into_iter().collect()))
}

/// Автор рецензии; нет рецензии — 404. Строка блокируется так же, как её потом заблокирует
/// триггер счётчиков (`NO KEY UPDATE`): параллельные голоса за одну рецензию идут по очереди и не
/// взаимоблокируются на повышении блокировки.
async fn review_author(tx: &mut PgConnection, id: Uuid) -> AppResult<Uuid> {
    let author: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM reviews WHERE id = $1 FOR NO KEY UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    author.ok_or(AppError::NotFound)
}

async fn counts(tx: &mut PgConnection, id: Uuid, value: Option<i16>) -> AppResult<VoteResult> {
    Ok(sqlx::query_as(
        "SELECT likes_count, dislikes_count, $2::smallint AS value FROM reviews WHERE id = $1",
    )
    .bind(id)
    .bind(value)
    .fetch_one(&mut *tx)
    .await?)
}
