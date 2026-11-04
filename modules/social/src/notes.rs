//! Личные заметки о произведениях: текст «для себя», который видит только автор.
//!
//! Заметка — отдельная запись, а не приватная рецензия: публичный отзыв и своя пометка нужны
//! одновременно. Поэтому заметки не попадают ни в `/entities/{slug}/reviews`, ни в
//! `/users/{username}/reviews`, ни в ленту, ни в админские ручки, и чужого
//! `/users/{username}/notes` нет — только свой `/users/me/notes`. Оценка остаётся в рецензии
//! и публична; в `reviews_count` и репутацию заметки не идут — это не публикация.
//! Событий в `realtime` они по той же причине не порождают.

use crate::models::{Note, NoteRow, NoteWithEntity, PageQuery, PutNote, NOTE_COLUMNS};
use crate::refs;
use crate::validate::{self, MAX_REVIEW_BODY};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Своя заметка о произведении.
#[utoipa::path(
    get, operation_id = "get_own_note", path = "/entities/{slug}/note", tag = "notes",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Заметка", body = Note),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или заметки нет", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Note>> {
    let entity = refs::entity(&state, &slug).await?;
    let row: Option<NoteRow> = sqlx::query_as(&format!(
        "SELECT {NOTE_COLUMNS} FROM entity_notes WHERE user_id = $1 AND entity_id = $2"
    ))
    .bind(user.id)
    .bind(entity.id)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(note(row.ok_or(AppError::NotFound)?)))
}

/// Написать или заменить свою заметку целиком. Одна заметка на произведение.
#[utoipa::path(
    put, operation_id = "put_note", path = "/entities/{slug}/note", tag = "notes",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body = PutNote,
    responses(
        (status = 200, description = "Заметка сохранена", body = Note),
        (status = 400, description = "Пустой или слишком длинный текст", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn put_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<PutNote>,
) -> AppResult<Json<Note>> {
    // Пустой текст — 400, а не удаление: заметку убирает DELETE.
    let body = validate::required("body", &req.body, MAX_REVIEW_BODY)?;
    let entity = refs::entity(&state, &slug).await?;
    let row: NoteRow = sqlx::query_as(&format!(
        "INSERT INTO entity_notes (user_id, entity_id, body) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, entity_id) DO UPDATE SET body = EXCLUDED.body
         RETURNING {NOTE_COLUMNS}"
    ))
    .bind(user.id)
    .bind(entity.id)
    .bind(&body)
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(Json(note(row)))
}

/// Удалить свою заметку.
#[utoipa::path(
    delete, operation_id = "delete_note", path = "/entities/{slug}/note", tag = "notes",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или заметки нет", body = ErrorBody),
    )
)]
pub async fn delete_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    let deleted: Option<uuid::Uuid> = sqlx::query_scalar(
        "DELETE FROM entity_notes WHERE user_id = $1 AND entity_id = $2 RETURNING id",
    )
    .bind(user.id)
    .bind(entity.id)
    .fetch_optional(&state.db)
    .await?;
    deleted.ok_or(AppError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Свои заметки, новые сверху: вкладка «Заметки» в личном кабинете. Чужих заметок нет нигде.
#[utoipa::path(
    get, operation_id = "list_own_notes", path = "/users/me/notes", tag = "notes",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница заметок", body = Page<NoteWithEntity>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<NoteWithEntity>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM entity_notes WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<NoteRow> = sqlx::query_as(&format!(
        "SELECT {NOTE_COLUMNS} FROM entity_notes WHERE user_id = $1
         ORDER BY created_at DESC, id LIMIT $2 OFFSET $3"
    ))
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: refs::notes(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Строка без `entity_id`: на карточке произведение и так известно.
fn note(row: NoteRow) -> Note {
    Note {
        id: row.id,
        body: row.body,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}
