//! Модерация: admin удаляет любую рецензию и коллекцию.

use axum::extract::State;
use axum::http::StatusCode;
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AdminUser, AppError, AppResult, AppState};
use uuid::Uuid;

/// Удалить любую рецензию.
#[utoipa::path(
    delete, path = "/admin/reviews/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id рецензии")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete_review(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let author: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM reviews WHERE id = $1 RETURNING user_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let author = author.ok_or(AppError::NotFound)?;
    tracing::info!(admin_id = %admin.id, review_id = %id, author_id = %author, "review deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любую коллекцию, в том числе приватную.
#[utoipa::path(
    delete, path = "/admin/collections/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let owner: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM collections WHERE id = $1 RETURNING user_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let owner = owner.ok_or(AppError::NotFound)?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, owner_id = %owner, "collection deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}
