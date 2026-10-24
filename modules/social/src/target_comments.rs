//! Комментарии ко всему, что не рецензия и не коллекция: к произведениям, людям и студиям.
//!
//! Одна таблица `comments` с парой (`target_kind`, `target_id`) и один набор эндпоинтов
//! (`/comments?target=entity:dune-2021`): правила у этих комментариев общие, а три одинаковых
//! таблицы пришлось бы чинить в трёх местах. Цена — строгого FK на цель нет: что она
//! существует, проверяет код, через справочники `shared::directory` (`entities`, `people`,
//! `studios` — чужие таблицы, `social` в них не ходит). Осиротевший комментарий (цель удалили)
//! в выдачу не попадает: справочник не вернёт ссылку.
//!
//! Правила — как в обсуждении коллекции: читать всем, писать вошедшему (не больше
//! [`RATE_COMMENTS`] в минуту), ответы одного уровня (ответ на ответ привязывается к корню),
//! удаление мягкое и удаляет вместе с корнем его ответы. Событий нет.

use crate::models::{
    CreateTargetComment, TargetComment, TargetCommentRow, TargetCommentsQuery,
    TARGET_COMMENT_COLUMNS,
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

/// Лимит частоты: не больше стольких комментариев автора за [`RATE_WINDOW_SECS`] секунд.
pub const RATE_COMMENTS: i64 = 5;
pub const RATE_WINDOW_SECS: u64 = 60;

/// Разобранная цель: вид и id строки в чужой таблице.
pub(crate) struct Target {
    pub kind: &'static str,
    pub id: Uuid,
}

impl Target {
    /// `kind:slug` обратно в строку — в ответе цель та же, что в запросе.
    fn as_string(&self, slug: &str) -> String {
        format!("{}:{slug}", self.kind)
    }
}

/// `entity:dune-2021` → цель. Неизвестный вид — 400, неизвестный slug — 404: адрес пришёл
/// со страницы, которой нет.
pub(crate) async fn parse(state: &AppState, target: &str) -> AppResult<(Target, String)> {
    let (kind, slug) = target.split_once(':').ok_or_else(|| AppError::Invalid {
        field: "target".into(),
        message: "target: expected <kind>:<slug>".into(),
    })?;
    let id = match kind {
        "entity" => refs::entity(state, slug).await?.id,
        "person" => refs::person(state, slug).await?.id,
        "studio" => refs::studio(state, slug).await?.id,
        _ => {
            return Err(AppError::Invalid {
                field: "target".into(),
                message: "target: kind must be entity, person or studio".into(),
            })
        }
    };
    let kind = match kind {
        "entity" => "entity",
        "person" => "person",
        _ => "studio",
    };
    Ok((Target { kind, id }, slug.to_string()))
}

async fn comments(
    state: &AppState,
    rows: Vec<TargetCommentRow>,
    target: &str,
) -> AppResult<Vec<TargetComment>> {
    let users = refs::users(state, rows.iter().map(|r| r.author_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(TargetComment {
                author: users.get(&row.author_id)?.clone(),
                id: row.id,
                target: target.to_string(),
                parent_id: row.parent_id,
                body: row.body,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

/// Комментарии цели по времени: старые сверху, ответы — в той же ленте с `parent_id`.
#[utoipa::path(
    get, operation_id = "list_comments", path = "/comments", tag = "comments",
    params(TargetCommentsQuery),
    responses(
        (status = 200, description = "Страница комментариев", body = Page<TargetComment>),
        (status = 400, description = "`validation` с `field: target` — не вида `kind:slug` или неизвестный вид", body = ErrorBody),
        (status = 404, description = "Нет такой цели", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<TargetCommentsQuery>,
) -> AppResult<Json<Page<TargetComment>>> {
    let (target, slug) = parse(&state, &query.target).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    const FILTER: &str =
        "FROM comments WHERE target_kind = $1 AND target_id = $2 AND deleted_at IS NULL";

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {FILTER}"))
        .bind(target.kind)
        .bind(target.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<TargetCommentRow> = sqlx::query_as(&format!(
        "SELECT {TARGET_COMMENT_COLUMNS} {FILTER} ORDER BY created_at, id LIMIT $3 OFFSET $4"
    ))
    .bind(target.kind)
    .bind(target.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: comments(&state, rows, &target.as_string(&slug)).await?,
        total,
        limit,
        offset,
    }))
}

/// Написать комментарий или ответить на него.
#[utoipa::path(
    post, operation_id = "create_comment", path = "/comments", tag = "comments",
    security(("bearer" = [])),
    request_body = CreateTargetComment,
    responses(
        (status = 201, description = "Создан", body = TargetComment),
        (status = 400, description = "`validation` с `field`: `target` — не вида `kind:slug`; `body` — пусто или длиннее 2000; `parent_id` — нет такого (неудалённого) комментария у этой цели", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой цели", body = ErrorBody),
        (status = 429, description = "Больше 5 комментариев в минуту (`Retry-After`)", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(req): JsonBody<CreateTargetComment>,
) -> AppResult<(StatusCode, Json<TargetComment>)> {
    let body =
        validate::required("body", &req.body, MAX_COMMENT_BODY).map_err(|e| e.on_field("body"))?;
    let (target, slug) = parse(&state, &req.target).await?;

    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM comments
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
                "SELECT parent_id FROM comments
                 WHERE id = $1 AND target_kind = $2 AND target_id = $3 AND deleted_at IS NULL",
            )
            .bind(parent)
            .bind(target.kind)
            .bind(target.id)
            .fetch_optional(&state.db)
            .await?;
            let root = found.ok_or_else(|| AppError::Invalid {
                field: "parent_id".into(),
                message: "parent_id: no such comment on this target".into(),
            })?;
            Some(root.unwrap_or(parent))
        }
    };

    let row: TargetCommentRow = sqlx::query_as(&format!(
        "INSERT INTO comments (target_kind, target_id, author_id, parent_id, body)
         VALUES ($1, $2, $3, $4, $5) RETURNING {TARGET_COMMENT_COLUMNS}"
    ))
    .bind(target.kind)
    .bind(target.id)
    .bind(user.id)
    .bind(parent)
    .bind(&body)
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    tracing::info!(user_id = %user.id, target = %req.target, comment_id = %row.id, "comment created");

    let comment = comments(&state, vec![row], &target.as_string(&slug))
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(comment)))
}

/// Удалить свой комментарий (корневой — вместе с ответами).
#[utoipa::path(
    delete, operation_id = "delete_comment", path = "/comments/{id}", tag = "comments",
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
        "SELECT author_id FROM comments WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let author = author.ok_or(AppError::NotFound)?;
    if owner.is_some_and(|owner| owner.id != author) {
        return Err(AppError::Forbidden);
    }
    sqlx::query(
        "UPDATE comments SET deleted_at = now()
         WHERE (id = $1 OR parent_id = $1) AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(author)
}
