//! Сообщения в темах форума. Ответить может любой вошедший (в закрытую тему — только admin),
//! менять и удалять — автор сообщения.
//!
//! Удаление: сообщение без ответов удаляется целиком, с ответами — становится заглушкой
//! «сообщение удалено» (текст и автор не показываются), чтобы ветка не рвалась. Заглушка, у
//! которой не осталось ответов, удаляется следом.

use crate::models::{CreatePost, Post, PostRow, UpdatePost, POST_COLUMNS};
use crate::refs;
use crate::validate::{self, MAX_POST_BODY};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AppError, AppResult, AppState, AuthUser, Role};
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

/// Ответить в теме или на сообщение (`parent_id`).
#[utoipa::path(
    post, path = "/threads/{id}/posts", tag = "forum",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    request_body = CreatePost,
    responses(
        (status = 201, description = "Создано", body = Post),
        (status = 400, description = "Неверные поля, родителя нет в теме или он удалён", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Тема закрыта", body = ErrorBody),
        (status = 404, description = "Тема не найдена", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(thread_id): Path<Uuid>,
    JsonBody(req): JsonBody<CreatePost>,
) -> AppResult<(StatusCode, Json<Post>)> {
    let body = validate::required("body", &req.body, MAX_POST_BODY)?;

    let mut tx = state.db.begin().await?;
    // Блокировка темы: счётчик и last_post_at меняются по очереди.
    let locked: Option<bool> =
        sqlx::query_scalar("SELECT is_locked FROM forum_threads WHERE id = $1 FOR UPDATE")
            .bind(thread_id)
            .fetch_optional(&mut *tx)
            .await?;
    match locked {
        None => return Err(AppError::NotFound),
        Some(true) if user.role < Role::Admin => return Err(AppError::Forbidden),
        Some(_) => {}
    }
    if let Some(parent) = req.parent_id {
        let deleted: Option<bool> = sqlx::query_scalar(
            "SELECT deleted_at IS NOT NULL FROM forum_posts WHERE id = $1 AND thread_id = $2",
        )
        .bind(parent)
        .bind(thread_id)
        .fetch_optional(&mut *tx)
        .await?;
        match deleted {
            None => {
                return Err(AppError::BadRequest(
                    "parent_id: no such post in this thread".into(),
                ))
            }
            Some(true) => {
                return Err(AppError::BadRequest(
                    "parent_id: cannot reply to a deleted post".into(),
                ))
            }
            Some(false) => {}
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO forum_posts (thread_id, author_id, parent_id, body)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(thread_id)
    .bind(user.id)
    .bind(req.parent_id)
    .bind(body)
    .fetch_one(&mut *tx)
    .await
    .map_err(validate::missing_reference)?;
    sqlx::query(
        "UPDATE forum_threads SET posts_count = posts_count + 1, last_post_at = now()
         WHERE id = $1",
    )
    .bind(thread_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(user_id = %user.id, thread_id = %thread_id, post_id = %id, "post created");
    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(refs::post(&state, row).await?)))
}

/// Изменить своё сообщение. Ставит `edited_at`; в закрытой теме тоже можно.
#[utoipa::path(
    patch, path = "/posts/{id}", tag = "forum",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сообщения")),
    request_body = UpdatePost,
    responses(
        (status = 200, description = "Изменено", body = Post),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужое сообщение", body = ErrorBody),
        (status = 404, description = "Нет или удалено", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdatePost>,
) -> AppResult<Json<Post>> {
    let body = validate::required("body", &req.body, MAX_POST_BODY)?;
    let mut tx = state.db.begin().await?;
    // NO KEY UPDATE не мешает параллельным ответам на это сообщение.
    owned(&mut *tx, id, &user, "FOR NO KEY UPDATE").await?;
    sqlx::query("UPDATE forum_posts SET body = $2, edited_at = now() WHERE id = $1")
        .bind(id)
        .bind(body)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let row = load(&state.db, id).await?.ok_or(AppError::NotFound)?;
    Ok(Json(refs::post(&state, row).await?))
}

/// Удалить своё сообщение. Если на него есть ответы, остаётся заглушка «сообщение удалено».
#[utoipa::path(
    delete, path = "/posts/{id}", tag = "forum",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сообщения")),
    responses(
        (status = 204, description = "Удалено"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужое сообщение", body = ErrorBody),
        (status = 404, description = "Нет или уже удалено", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    owned(&mut *tx, id, &user, "").await?;
    remove(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(user_id = %user.id, post_id = %id, "post deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Сообщение пользователя `user`: нет или удалено — 404, чужое — 403.
async fn owned(db: impl PgExecutor<'_>, id: Uuid, user: &AuthUser, lock: &str) -> AppResult<()> {
    let author: Option<Uuid> = sqlx::query_scalar(&format!(
        "SELECT author_id FROM forum_posts WHERE id = $1 AND deleted_at IS NULL {lock}"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    match author {
        Some(author) if author == user.id => Ok(()),
        Some(_) => Err(AppError::Forbidden),
        None => Err(AppError::NotFound),
    }
}

/// Удалить сообщение (права уже проверены): целиком или в заглушку, если есть ответы.
/// Возвращает автора. Нет или уже удалено — 404.
pub(crate) async fn remove(tx: &mut PgConnection, id: Uuid) -> AppResult<Uuid> {
    // Сначала тема, потом сообщение — в том же порядке, что и при ответе (create): иначе ответ
    // (держит тему, ждёт родителя) и удаление родителя (держит его, ждёт тему) заблокируют друг друга.
    let thread_id: Option<Uuid> =
        sqlx::query_scalar("SELECT thread_id FROM forum_posts WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let thread_id = thread_id.ok_or(AppError::NotFound)?;
    sqlx::query("SELECT 1 FROM forum_threads WHERE id = $1 FOR UPDATE")
        .bind(thread_id)
        .execute(&mut *tx)
        .await?;
    let post: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT author_id, parent_id FROM forum_posts
         WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (author, mut parent) = post.ok_or(AppError::NotFound)?;

    let has_replies: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forum_posts WHERE parent_id = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if has_replies {
        sqlx::query("UPDATE forum_posts SET body = NULL, deleted_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("DELETE FROM forum_posts WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // Заглушки выше по ветке, у которых не осталось ответов, больше не нужны.
        while let Some(id) = parent {
            parent = sqlx::query_scalar::<_, Option<Uuid>>(
                "DELETE FROM forum_posts p WHERE p.id = $1 AND p.deleted_at IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM forum_posts c WHERE c.parent_id = p.id)
                 RETURNING p.parent_id",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        }
    }
    sqlx::query("UPDATE forum_threads SET posts_count = posts_count - 1 WHERE id = $1")
        .bind(thread_id)
        .execute(&mut *tx)
        .await?;
    Ok(author)
}

async fn load(db: impl PgExecutor<'_>, id: Uuid) -> AppResult<Option<PostRow>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {POST_COLUMNS} FROM forum_posts p WHERE p.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?)
}
