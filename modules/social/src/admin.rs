//! Модерация: admin удаляет любую рецензию, комментарий к ней, коллекцию, тему и сообщение,
//! помечает коллекции особыми,
//! закрывает темы.

use crate::collections;
use crate::comments;
use crate::events;
use crate::models::{Collection, FeatureCollection};
use crate::posts;
use crate::refs;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use uuid::Uuid;

/// Удалить любую рецензию.
#[utoipa::path(
    delete, operation_id = "admin_delete_review", path = "/admin/reviews/{id}", tag = "social-admin",
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
    let review: Option<(Uuid, Uuid)> =
        sqlx::query_as("DELETE FROM reviews WHERE id = $1 RETURNING user_id, entity_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let (author, entity) = review.ok_or(AppError::NotFound)?;
    tracing::info!(admin_id = %admin.id, review_id = %id, author_id = %author, "review deleted by admin");
    events::publish(&state, events::review("review.deleted", id, entity, author));
    crate::ratings::sync(&state, entity).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любой комментарий к рецензии.
#[utoipa::path(
    delete, operation_id = "admin_delete_review_comment", path = "/admin/review-comments/{id}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id комментария")),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет или уже удалён", body = ErrorBody),
    )
)]
pub async fn delete_review_comment(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let author = comments::remove(&state, id, None).await?;
    tracing::info!(admin_id = %admin.id, comment_id = %id, author_id = %author, "review comment deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любую коллекцию, в том числе приватную.
#[utoipa::path(
    delete, operation_id = "admin_delete_collection", path = "/admin/collections/{id}", tag = "social-admin",
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

/// Пометить публичную коллекцию особой или поменять её порядок среди особых.
#[utoipa::path(
    put, operation_id = "admin_feature_collection", path = "/admin/collections/{id}/featured",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body(content = FeatureCollection, description = "Можно не передавать тело: в конец"),
    responses(
        (status = 200, description = "Особая", body = Collection),
        (status = 400, description = "Коллекция приватная", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn feature_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    body: Option<JsonBody<FeatureCollection>>,
) -> AppResult<Json<Collection>> {
    let req = body.map(|JsonBody(req)| req).unwrap_or_default();
    let updated = sqlx::query(
        "UPDATE collections SET featured_position = COALESCE($2, featured_position,
             (SELECT COALESCE(max(featured_position) + 1, 0) FROM collections
              WHERE featured_position IS NOT NULL))
         WHERE id = $1",
    )
    .bind(id)
    .bind(req.position)
    .execute(&state.db)
    .await
    .map_err(collections::featured_must_be_public)?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(admin_id = %admin.id, collection_id = %id, "collection featured by admin");
    let row = collections::load(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(refs::collection(&state, row).await?))
}

/// Снять с коллекции отметку «особая».
#[utoipa::path(
    delete, operation_id = "admin_unfeature_collection", path = "/admin/collections/{id}/featured",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 204, description = "Больше не особая (повторно — тоже)"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn unfeature_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let updated = sqlx::query("UPDATE collections SET featured_position = NULL WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(admin_id = %admin.id, collection_id = %id, "collection unfeatured by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любую тему вместе с сообщениями.
#[utoipa::path(
    delete, operation_id = "admin_delete_thread", path = "/admin/threads/{id}", tag = "social-admin",
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
    let mut tx = state.db.begin().await?;
    let entities = events::thread_entities(&mut *tx, id).await?;
    let author: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM forum_threads WHERE id = $1 RETURNING author_id")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let author = author.ok_or(AppError::NotFound)?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, author_id = %author, "thread deleted by admin");
    events::publish(
        &state,
        events::thread_changed("thread.deleted", id, &entities),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любое сообщение. Если на него есть ответы, остаётся заглушка «сообщение удалено».
#[utoipa::path(
    delete, operation_id = "admin_delete_post", path = "/admin/posts/{id}", tag = "social-admin",
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
    let removed = posts::remove(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, post_id = %id, author_id = %removed.author_id, "post deleted by admin");
    events::publish(
        &state,
        events::post_changed("post.deleted", removed.thread_id, id),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Закрыть тему для ответов (повторно — тоже 204). Править и удалять свои сообщения можно.
#[utoipa::path(
    put, operation_id = "admin_lock_thread", path = "/admin/threads/{id}/lock", tag = "social-admin",
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
    delete, operation_id = "admin_unlock_thread", path = "/admin/threads/{id}/lock", tag = "social-admin",
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
    let entities = events::thread_entities(&state.db, id).await?;
    events::publish(
        state,
        events::thread_changed("thread.updated", id, &entities),
    );
    Ok(())
}
