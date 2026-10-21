//! Модерация: admin удаляет любую рецензию, комментарий к ней, коллекцию, тему и сообщение,
//! помечает коллекции особыми (с читаемым адресом),
//! закрывает темы.

use crate::collections;
use crate::comments;
use crate::events;
use crate::models::{Collection, FeatureCollection};
use crate::posts;
use crate::refs;
use crate::slug;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::PgConnection;
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

/// Занят ли slug другой коллекцией: текущий адрес или бывший (история, в том числе удалённых).
async fn slug_taken(tx: &mut PgConnection, slug: &str, id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM collections WHERE slug = $1 AND id <> $2)
             OR EXISTS (SELECT 1 FROM collection_slug_history
                        WHERE slug = $1 AND collection_id IS DISTINCT FROM $2)",
    )
    .bind(slug)
    .bind(id)
    .fetch_one(tx)
    .await?)
}

/// Пометить публичную коллекцию особой, поменять её порядок или адрес.
#[utoipa::path(
    put, operation_id = "admin_feature_collection", path = "/admin/collections/{id}/featured",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body(content = FeatureCollection, description = "Можно не передавать тело: в конец, slug — транслит названия"),
    responses(
        (status = 200, description = "Особая", body = Collection),
        (status = 400, description = "Коллекция приватная; неверный slug (формат, UUID, служебное слово)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
        (status = 409, description = "slug занят другой коллекцией (сейчас или раньше)", body = ErrorBody),
    )
)]
pub async fn feature_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    body: Option<JsonBody<FeatureCollection>>,
) -> AppResult<Json<Collection>> {
    let req = body.map(|JsonBody(req)| req).unwrap_or_default();
    let wanted = req.slug.as_deref().map(slug::validate).transpose()?;

    let mut tx = state.db.begin().await?;
    // Выбор и запись slug — по очереди: две коллекции не займут один адрес.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('collection-slug', 0))")
        .execute(&mut *tx)
        .await?;
    let found: Option<(bool, Option<String>, String)> =
        sqlx::query_as("SELECT is_public, slug, title FROM collections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (is_public, current, title) = found.ok_or(AppError::NotFound)?;
    if !is_public {
        return Err(AppError::BadRequest(
            "featured collection must be public".into(),
        ));
    }
    let new_slug = match (wanted, current.clone()) {
        (Some(wanted), _) => {
            if slug_taken(&mut tx, &wanted, id).await? {
                return Err(AppError::Conflict(format!(
                    "slug {wanted} is already taken"
                )));
            }
            wanted
        }
        (None, Some(current)) => current,
        (None, None) => {
            let base = slug::from_title(&title);
            let mut candidate = base.clone();
            let mut n = 2;
            while slug_taken(&mut tx, &candidate, id).await? {
                candidate = format!("{base}-{n}");
                n += 1;
            }
            candidate
        }
    };
    if let Some(old) = current.filter(|old| *old != new_slug) {
        sqlx::query(
            "INSERT INTO collection_slug_history (slug, collection_id) VALUES ($1, $2)
             ON CONFLICT (slug) DO UPDATE SET collection_id = EXCLUDED.collection_id",
        )
        .bind(&old)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    // Свой бывший адрес можно вернуть: из истории он уходит.
    sqlx::query("DELETE FROM collection_slug_history WHERE slug = $1 AND collection_id = $2")
        .bind(&new_slug)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE collections SET slug = $3,
             featured_position = COALESCE($2, featured_position,
                 (SELECT COALESCE(max(featured_position) + 1, 0) FROM collections
                  WHERE featured_position IS NOT NULL))
         WHERE id = $1",
    )
    .bind(id)
    .bind(req.position)
    .bind(&new_slug)
    .execute(&mut *tx)
    .await
    .map_err(collections::featured_must_be_public)?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, slug = %new_slug, "collection featured by admin");
    let row = collections::load(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(refs::collection(&state, row).await?))
}

/// Снять с коллекции отметку «особая». slug уходит в историю: старый адрес ведёт на
/// `/collections/{id}`, другой коллекции он не достанется.
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
    let mut tx = state.db.begin().await?;
    let current: Option<Option<String>> =
        sqlx::query_scalar("SELECT slug FROM collections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let current = current.ok_or(AppError::NotFound)?;
    if let Some(old) = &current {
        sqlx::query(
            "INSERT INTO collection_slug_history (slug, collection_id) VALUES ($1, $2)
             ON CONFLICT (slug) DO UPDATE SET collection_id = EXCLUDED.collection_id",
        )
        .bind(old)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE collections SET featured_position = NULL, slug = NULL WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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
