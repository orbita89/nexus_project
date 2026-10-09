//! Комментарии к рецензиям: читать — всем, писать — любой вошедший, удалять — автор комментария
//! (admin — через `/admin/review-comments/{id}`). Удалённый комментарий пропадает из списка.
//! `comments_count` рецензии пишет триггер на `review_comments`.

use crate::events;
use crate::models::{
    CreateReviewComment, PageQuery, ReviewComment, ReviewCommentRow, COMMENT_COLUMNS,
};
use crate::refs;
use crate::validate::{self, MAX_COMMENT_BODY};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use uuid::Uuid;

/// Комментарии к рецензии по времени: старые сверху.
#[utoipa::path(
    get, operation_id = "list_review_comments", path = "/reviews/{id}/comments", tag = "reviews",
    params(("id" = Uuid, Path, description = "id рецензии"), PageQuery),
    responses(
        (status = 200, description = "Страница комментариев", body = Page<ReviewComment>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Рецензия не найдена", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<ReviewComment>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: Option<i64> = sqlx::query_scalar("SELECT comments_count FROM reviews WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let total = total.ok_or(AppError::NotFound)?;
    let rows: Vec<ReviewCommentRow> = sqlx::query_as(&format!(
        "SELECT {COMMENT_COLUMNS} FROM review_comments
         WHERE review_id = $1 AND deleted_at IS NULL
         ORDER BY created_at, id LIMIT $2 OFFSET $3"
    ))
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: refs::comments(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Прокомментировать рецензию. Оценку без текста комментировать нельзя.
#[utoipa::path(
    post, operation_id = "create_review_comment", path = "/reviews/{id}/comments", tag = "reviews",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id рецензии")),
    request_body = CreateReviewComment,
    responses(
        (status = 201, description = "Создан", body = ReviewComment),
        (status = 400, description = "Пустой или длиннее 2000 символов текст", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Рецензия не найдена", body = ErrorBody),
        (status = 409, description = "Оценка без текста (`code`: `review_without_text`)", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<CreateReviewComment>,
) -> AppResult<(StatusCode, Json<ReviewComment>)> {
    let body = validate::required("body", &req.body, MAX_COMMENT_BODY)?;
    let mut tx = state.db.begin().await?;
    // Строку рецензии потом обновит триггер счётчика: блокируем сразу так же.
    let review: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT entity_id, body IS NOT NULL FROM reviews WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (entity_id, has_text) = review.ok_or(AppError::NotFound)?;
    if !has_text {
        return Err(
            AppError::Conflict("review has no text to comment on".into())
                .with_code("review_without_text"),
        );
    }
    let row: ReviewCommentRow = sqlx::query_as(&format!(
        "INSERT INTO review_comments (review_id, author_id, body) VALUES ($1, $2, $3)
         RETURNING {COMMENT_COLUMNS}"
    ))
    .bind(id)
    .bind(user.id)
    .bind(&body)
    .fetch_one(&mut *tx)
    .await
    .map_err(validate::missing_reference)?;
    tx.commit().await?;

    tracing::info!(user_id = %user.id, review_id = %id, comment_id = %row.id, "review comment created");
    events::publish(
        &state,
        events::review_comment("review.commented", id, row.id, entity_id, user.id),
    );
    let comment = refs::comments(&state, vec![row])
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(comment)))
}

/// Удалить свой комментарий.
#[utoipa::path(
    delete, operation_id = "delete_review_comment", path = "/review-comments/{id}", tag = "reviews",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id комментария")),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужой комментарий", body = ErrorBody),
        (status = 404, description = "Нет или уже удалён", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    remove(&state, id, Some(&user)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Удаляет комментарий (`deleted_at`) и публикует событие. `owner` — только свой, иначе 403;
/// `None` — модерация.
pub(crate) async fn remove(
    state: &AppState,
    id: Uuid,
    owner: Option<&AuthUser>,
) -> AppResult<Uuid> {
    let mut tx = state.db.begin().await?;
    let found: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT c.author_id, c.review_id, r.entity_id
         FROM review_comments c JOIN reviews r ON r.id = c.review_id
         WHERE c.id = $1 AND c.deleted_at IS NULL
         FOR NO KEY UPDATE OF c, r",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (author, review_id, entity_id) = found.ok_or(AppError::NotFound)?;
    if owner.is_some_and(|owner| owner.id != author) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("UPDATE review_comments SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    events::publish(
        state,
        events::review_comment("review.comment_deleted", review_id, id, entity_id, author),
    );
    Ok(author)
}
