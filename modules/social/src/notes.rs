//! Личные заметки о произведениях: текст «для себя», который видит только автор.
//!
//! Заметка — отдельная запись, а не приватная рецензия: публичный отзыв и своя пометка нужны
//! одновременно. Поэтому заметки не попадают ни в `/entities/{slug}/reviews`, ни в
//! `/users/{username}/reviews`, ни в ленту, ни в админские ручки, и чужого
//! `/users/{username}/notes` нет — только свой `/users/me/notes`. Оценка остаётся в рецензии
//! и публична; в `reviews_count` и репутацию заметки не идут — это не публикация.
//! Событий в `realtime` они по той же причине не порождают.
//!
//! Заметки о людях каталога (`/people/{slug}/note`) устроены так же и лежат в `person_notes`.

use crate::models::{
    Note, NoteKind, NoteRow, NotesQuery, OwnNotesPage, PersonNoteRow, PutNote, NOTE_COLUMNS,
    PERSON_NOTE_COLUMNS,
};
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
/// `kind=entity` (по умолчанию) — о произведениях, `kind=person` — о людях.
#[utoipa::path(
    get, operation_id = "list_own_notes", path = "/users/me/notes", tag = "notes",
    security(("bearer" = [])),
    params(NotesQuery),
    responses(
        (status = 200, description = "Страница заметок: `NoteWithEntity` или, при `kind=person`, `NoteWithPerson`", body = OwnNotesPage),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<NotesQuery>,
) -> AppResult<Json<OwnNotesPage>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    if query.kind.unwrap_or_default() == NoteKind::Person {
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM person_notes WHERE user_id = $1")
            .bind(user.id)
            .fetch_one(&state.db)
            .await?;
        let rows: Vec<PersonNoteRow> = sqlx::query_as(&format!(
            "SELECT {PERSON_NOTE_COLUMNS} FROM person_notes WHERE user_id = $1
             ORDER BY created_at DESC, id LIMIT $2 OFFSET $3"
        ))
        .bind(user.id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        return Ok(Json(OwnNotesPage::People(Page {
            items: refs::person_notes(&state, rows).await?,
            total,
            limit,
            offset,
        })));
    }
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
    Ok(Json(OwnNotesPage::Entities(Page {
        items: refs::notes(&state, rows).await?,
        total,
        limit,
        offset,
    })))
}

/// Своя заметка о человеке каталога.
#[utoipa::path(
    get, operation_id = "get_own_person_note", path = "/people/{slug}/note", tag = "notes",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 200, description = "Заметка", body = Note),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человек не найден или заметки нет", body = ErrorBody),
    )
)]
pub async fn get_own_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Note>> {
    let person = refs::person(&state, &slug).await?;
    let row: Option<PersonNoteRow> = sqlx::query_as(&format!(
        "SELECT {PERSON_NOTE_COLUMNS} FROM person_notes WHERE user_id = $1 AND person_id = $2"
    ))
    .bind(user.id)
    .bind(person.id)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(person_note(row.ok_or(AppError::NotFound)?)))
}

/// Написать или заменить свою заметку о человеке целиком. Одна заметка на человека.
#[utoipa::path(
    put, operation_id = "put_person_note", path = "/people/{slug}/note", tag = "notes",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    request_body = PutNote,
    responses(
        (status = 200, description = "Заметка сохранена", body = Note),
        (status = 400, description = "Пустой или слишком длинный текст", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человек не найден", body = ErrorBody),
    )
)]
pub async fn put_own_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<PutNote>,
) -> AppResult<Json<Note>> {
    let body = validate::required("body", &req.body, MAX_REVIEW_BODY)?;
    let person = refs::person(&state, &slug).await?;
    let row: PersonNoteRow = sqlx::query_as(&format!(
        "INSERT INTO person_notes (user_id, person_id, body) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, person_id) DO UPDATE SET body = EXCLUDED.body
         RETURNING {PERSON_NOTE_COLUMNS}"
    ))
    .bind(user.id)
    .bind(person.id)
    .bind(&body)
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(Json(person_note(row)))
}

/// Удалить свою заметку о человеке.
#[utoipa::path(
    delete, operation_id = "delete_person_note", path = "/people/{slug}/note", tag = "notes",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug человека", example = "denis-villeneuve")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Человек не найден или заметки нет", body = ErrorBody),
    )
)]
pub async fn delete_own_person(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let person = refs::person(&state, &slug).await?;
    let deleted: Option<uuid::Uuid> = sqlx::query_scalar(
        "DELETE FROM person_notes WHERE user_id = $1 AND person_id = $2 RETURNING id",
    )
    .bind(user.id)
    .bind(person.id)
    .fetch_optional(&state.db)
    .await?;
    deleted.ok_or(AppError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

fn person_note(row: PersonNoteRow) -> Note {
    Note {
        id: row.id,
        body: row.body,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
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
