//! Модерация: admin удаляет любую рецензию, коллекцию, тему и сообщение, закрывает темы.

use crate::posts;
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

/// Удалить любую тему вместе с сообщениями.
#[utoipa::path(
    delete, path = "/admin/threads/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete_thread(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let author: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM forum_threads WHERE id = $1 RETURNING author_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let author = author.ok_or(AppError::NotFound)?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, author_id = %author, "thread deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любое сообщение. Если на него есть ответы, остаётся заглушка «сообщение удалено».
#[utoipa::path(
    delete, path = "/admin/posts/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сообщения")),
    responses(
        (status = 204, description = "Удалено"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет или уже удалено", body = ErrorBody),
    )
)]
pub async fn delete_post(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let author = posts::remove(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, post_id = %id, author_id = %author, "post deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Закрыть тему для ответов (повторно — тоже 204). Править и удалять свои сообщения можно.
#[utoipa::path(
    put, path = "/admin/threads/{id}/lock", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Закрыта"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn lock_thread(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    set_locked(&state, id, true).await?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, "thread locked by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Открыть тему для ответов (повторно — тоже 204).
#[utoipa::path(
    delete, path = "/admin/threads/{id}/lock", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Открыта"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn unlock_thread(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    set_locked(&state, id, false).await?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, "thread unlocked by admin");
    Ok(StatusCode::NO_CONTENT)
}

async fn set_locked(state: &AppState, id: Uuid, locked: bool) -> AppResult<()> {
    let updated = sqlx::query("UPDATE forum_threads SET is_locked = $2 WHERE id = $1")
        .bind(id)
        .bind(locked)
        .execute(&state.db)
        .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}
