//! Реакции зрителей на произведение: 🔥 😢 🤔 😴. Одна на зрителя; счётчики — в сводке
//! `GET /entities/{slug}/rating`, своя реакция — отдельным запросом (сводка одинакова для всех).

use crate::models::{Reaction, ReactionKind};
use crate::refs;
use crate::validate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Своя реакция на произведение.
#[utoipa::path(
    get, operation_id = "get_own_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Реакция", body = Reaction),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или реакции нет", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Reaction>> {
    let entity = refs::entity(&state, &slug).await?;
    let kind: Option<ReactionKind> = sqlx::query_scalar(
        "SELECT kind FROM entity_reactions WHERE entity_id = $1 AND user_id = $2",
    )
    .bind(entity.id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?;
    let kind = kind.ok_or(AppError::NotFound)?;
    Ok(Json(Reaction { kind }))
}

/// Поставить реакцию; другая заменяет прежнюю. До выхода — нельзя, как и оценки.
#[utoipa::path(
    put, operation_id = "put_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body = Reaction,
    responses(
        (status = 200, description = "Реакция сохранена", body = Reaction),
        (status = 400, description = "Неизвестная реакция", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
        (status = 409, description = "Ещё не вышла (`code`: `not_released`)", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<Reaction>,
) -> AppResult<Json<Reaction>> {
    let entity = refs::entity(&state, &slug).await?;
    refs::require_released(&entity)?;
    sqlx::query(
        "INSERT INTO entity_reactions (entity_id, user_id, kind) VALUES ($1, $2, $3)
         ON CONFLICT (entity_id, user_id) DO UPDATE SET kind = EXCLUDED.kind, created_at = now()
         WHERE entity_reactions.kind <> EXCLUDED.kind",
    )
    .bind(entity.id)
    .bind(user.id)
    .bind(req.kind)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(Json(req))
}

/// Убрать свою реакцию → 204 (даже если её не было).
#[utoipa::path(
    delete, operation_id = "delete_reaction", path = "/entities/{slug}/reaction", tag = "reviews",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Реакции нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    sqlx::query("DELETE FROM entity_reactions WHERE entity_id = $1 AND user_id = $2")
        .bind(entity.id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
