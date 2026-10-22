//! Обсуждение коллекции (по образцу комментариев к рецензиям): читать — всем, кто видит
//! коллекцию (приватную — только владелец), писать — любой вошедший, удалять — автор (admin — через
//! `/admin/collection-comments/{id}`).
//!
//! Ответы — один уровень: `parent_id` — корневой комментарий этой коллекции; ответ на ответ
//! привязывается к его корню. Список плоский, старые сверху (ответы — после того, на что
//! отвечают). Удаление мягкое: удалённый пропадает из списка, удаление корня удаляет и ответы.
//! Закрытое обсуждение (`comments_locked`): писать может только admin. Событий нет.

use crate::models::{
    CollectionComment, CollectionCommentRow, CreateCollectionComment, PageQuery,
    COLLECTION_COMMENT_COLUMNS,
};
use crate::refs;
use crate::validate::{self, MAX_COMMENT_BODY};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser, Role};
use sqlx::PgExecutor;
use uuid::Uuid;

/// Лимит частоты: не больше `RATE_COMMENTS` комментариев автора за `RATE_WINDOW_SECS` секунд.
pub const RATE_COMMENTS: i64 = 5;
pub const RATE_WINDOW_SECS: u64 = 60;

/// Коллекция, если её видит `viewer` (публичная или своя), иначе 404: (`comments_locked`).
async fn visible(db: impl PgExecutor<'_>, id: Uuid, viewer: Option<&AuthUser>) -> AppResult<bool> {
    let found: Option<(Uuid, bool, bool)> =
        sqlx::query_as("SELECT user_id, is_public, comments_locked FROM collections WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    match found {
        Some((owner, is_public, locked)) if is_public || viewer.is_some_and(|v| v.id == owner) => {
            Ok(locked)
        }
        _ => Err(AppError::NotFound),
    }
}

async fn comments(
    state: &AppState,
    rows: Vec<CollectionCommentRow>,
) -> AppResult<Vec<CollectionComment>> {
    let users = refs::users(state, rows.iter().map(|r| r.author_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(CollectionComment {
                author: users.get(&row.author_id)?.clone(),
                id: row.id,
                collection_id: row.collection_id,
                parent_id: row.parent_id,
                body: row.body,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

/// Обсуждение коллекции по времени: старые сверху, ответы — в той же ленте с `parent_id`.
#[utoipa::path(
    get, operation_id = "list_collection_comments", path = "/collections/{id}/comments", tag = "collections",
    params(("id" = Uuid, Path, description = "id коллекции"), PageQuery),
    responses(
        (status = 200, description = "Страница комментариев", body = Page<CollectionComment>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет коллекции или она чужая приватная", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<CollectionComment>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    visible(&state.db, id, viewer.as_ref()).await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM collection_comments
         WHERE collection_id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<CollectionCommentRow> = sqlx::query_as(&format!(
        "SELECT {COLLECTION_COMMENT_COLUMNS} FROM collection_comments
         WHERE collection_id = $1 AND deleted_at IS NULL
         ORDER BY created_at, id LIMIT $2 OFFSET $3"
    ))
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: comments(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Написать в обсуждение коллекции или ответить на комментарий.
#[utoipa::path(
    post, operation_id = "create_collection_comment", path = "/collections/{id}/comments", tag = "collections",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body = CreateCollectionComment,
    responses(
        (status = 201, description = "Создан", body = CollectionComment),
        (status = 400, description = "`validation` с `field`: `body` — пусто или длиннее 2000; `parent_id` — нет такого (неудалённого) комментария в этой коллекции", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Обсуждение закрыто (`comments_locked`)", body = ErrorBody),
        (status = 404, description = "Нет коллекции или она чужая приватная", body = ErrorBody),
        (status = 429, description = "Больше 5 комментариев в минуту (`Retry-After`)", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<CreateCollectionComment>,
) -> AppResult<(StatusCode, Json<CollectionComment>)> {
    let body =
        validate::required("body", &req.body, MAX_COMMENT_BODY).map_err(|e| e.on_field("body"))?;
    let locked = visible(&state.db, id, Some(&user)).await?;
    if locked && user.role < Role::Admin {
        return Err(AppError::Forbidden.with_code("comments_locked"));
    }
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM collection_comments
         WHERE author_id = $1 AND created_at > now() - make_interval(secs => $2)",
    )
    .bind(user.id)
    .bind(RATE_WINDOW_SECS as f64)
    .fetch_one(&state.db)
    .await?;
    if recent >= RATE_COMMENTS {
        return Err(AppError::TooManyRequests {
            retry_after_secs: RATE_WINDOW_SECS,
        });
    }
    // Ответ — к корню: ответ на ответ привязывается к его корню.
    let parent = match req.parent_id {
        None => None,
        Some(parent) => {
            let found: Option<Option<Uuid>> = sqlx::query_scalar(
                "SELECT parent_id FROM collection_comments
                 WHERE id = $1 AND collection_id = $2 AND deleted_at IS NULL",
            )
            .bind(parent)
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
            let root = found.ok_or_else(|| AppError::Invalid {
                field: "parent_id".into(),
                message: "parent_id: no such comment in this collection".into(),
            })?;
            Some(root.unwrap_or(parent))
        }
    };
    let row: CollectionCommentRow = sqlx::query_as(&format!(
        "INSERT INTO collection_comments (collection_id, author_id, parent_id, body)
         VALUES ($1, $2, $3, $4) RETURNING {COLLECTION_COMMENT_COLUMNS}"
    ))
    .bind(id)
    .bind(user.id)
    .bind(parent)
    .bind(&body)
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    tracing::info!(user_id = %user.id, collection_id = %id, comment_id = %row.id, "collection comment created");
    let comment = comments(&state, vec![row])
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(comment)))
}

/// Удалить свой комментарий (с ответами, если он корневой).
#[utoipa::path(
    delete, operation_id = "delete_collection_comment", path = "/collection-comments/{id}", tag = "collections",
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

/// Мягко удаляет комментарий и его ответы. `owner` — только свой, иначе 403; `None` — модерация.
/// Возвращает автора.
pub(crate) async fn remove(
    state: &AppState,
    id: Uuid,
    owner: Option<&AuthUser>,
) -> AppResult<Uuid> {
    let mut tx = state.db.begin().await?;
    let author: Option<Uuid> = sqlx::query_scalar(
        "SELECT author_id FROM collection_comments
         WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let author = author.ok_or(AppError::NotFound)?;
    if owner.is_some_and(|owner| owner.id != author) {
        return Err(AppError::Forbidden);
    }
    sqlx::query(
        "UPDATE collection_comments SET deleted_at = now()
         WHERE (id = $1 OR parent_id = $1) AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(author)
}
