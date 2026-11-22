//! Закладки и выделения в читалке книг: личные, видит только автор (как заметки, `notes.rs`).
//!
//! - закладка (`bookmark`) — место в книге: EPUB CFI точки, глава, доля прочитанного;
//! - выделение (`highlight`) — фрагмент: CFI диапазона, сам текст, цвет 0–6 и заметка.
//!
//! Страница книги статическая (один HTML для всех), поэтому читалка, блок «Мои пометки» на
//! странице книги и раздел профиля берут всё отсюда после входа. Чужих пометок нет нигде;
//! в ленту, репутацию и `realtime` они не идут — это не публикация.

use crate::refs;
use crate::validate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::directory::EntityRef;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

const MAX_CFI: usize = 2_000;
const MAX_TEXT: usize = 2_000;
const MAX_NOTE: usize = 5_000;
const MAX_CHAPTER: usize = 300;
/// Цветов выделения: 0–6.
const COLORS: i16 = 7;
/// Пометок одного читателя в одной книге — защита от мусора, а не реальный предел.
const MAX_PER_BOOK: i64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum AnnotationKind {
    Bookmark,
    Highlight,
}

/// Закладка или выделение.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Annotation {
    pub id: Uuid,
    pub kind: AnnotationKind,
    /// EPUB CFI: точка (закладка) или диапазон (выделение).
    pub cfi: String,
    /// Выделенный текст; у закладки — `null`.
    pub text: Option<String>,
    /// Цвет выделения 0–6; у закладки — `null`.
    pub color: Option<i16>,
    pub note: Option<String>,
    pub chapter: Option<String>,
    /// Доля прочитанного 0–1 в момент создания.
    pub fraction: Option<f32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, kind, cfi, text, color, note, chapter, fraction, created_at, updated_at";

#[derive(Debug, sqlx::FromRow)]
struct AnnotationRow {
    entity_id: Uuid,
    #[sqlx(flatten)]
    annotation: Annotation,
}

/// Пометка в своём списке (профиль): то же плюс книга.
#[derive(Debug, Serialize, ToSchema)]
pub struct AnnotationWithEntity {
    pub entity: EntityRef,
    #[serde(flatten)]
    pub annotation: Annotation,
}

/// Новая пометка.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAnnotation {
    pub kind: AnnotationKind,
    #[schema(example = "epubcfi(/6/14!/4/2/1:0)")]
    pub cfi: String,
    /// Обязателен у выделения.
    pub text: Option<String>,
    /// 0–6, у выделения по умолчанию 0.
    pub color: Option<i16>,
    pub note: Option<String>,
    pub chapter: Option<String>,
    pub fraction: Option<f32>,
}

/// Правка: цвет и заметка. `note: null` или пустая строка — убрать заметку; поле не передано —
/// не менять.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateAnnotation {
    pub color: Option<i16>,
    #[serde(default, deserialize_with = "crate::models::nullable")]
    #[schema(value_type = Option<String>)]
    pub note: Option<Option<String>>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AnnotationsQuery {
    /// Только закладки или только выделения; по умолчанию — всё.
    #[param(inline)]
    pub kind: Option<AnnotationKind>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

fn color(value: Option<i16>) -> AppResult<Option<i16>> {
    match value {
        Some(c) if !(0..COLORS).contains(&c) => Err(AppError::Invalid {
            field: "color".into(),
            message: format!("цвет 0–{}", COLORS - 1),
        }),
        _ => Ok(value),
    }
}

/// Книга по slug; не книга — 404 (пометки бывают только в читалке).
async fn book(state: &AppState, slug: &str) -> AppResult<EntityRef> {
    let entity = refs::entity(state, slug).await?;
    if entity.kind != "book" {
        return Err(AppError::NotFound);
    }
    Ok(entity)
}

/// Свои пометки в книге — по порядку в книге (доля прочитанного), затем по времени.
#[utoipa::path(
    get, operation_id = "list_book_annotations", path = "/books/{slug}/annotations", tag = "annotations",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug книги", example = "master-i-margarita")),
    responses(
        (status = 200, description = "Закладки и выделения", body = Vec<Annotation>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Книга не найдена", body = ErrorBody),
    )
)]
pub async fn list_book(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Vec<Annotation>>> {
    let entity = book(&state, &slug).await?;
    let rows: Vec<Annotation> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM book_annotations WHERE user_id = $1 AND entity_id = $2
         ORDER BY fraction NULLS LAST, created_at, id"
    ))
    .bind(user.id)
    .bind(entity.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Добавить закладку или выделение. Повторная закладка на то же место возвращает существующую.
#[utoipa::path(
    post, operation_id = "create_book_annotation", path = "/books/{slug}/annotations", tag = "annotations",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug книги", example = "master-i-margarita")),
    request_body = CreateAnnotation,
    responses(
        (status = 201, description = "Создана (или такая закладка уже была)", body = Annotation),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Книга не найдена", body = ErrorBody),
        (status = 409, description = "Слишком много пометок в книге", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<CreateAnnotation>,
) -> AppResult<(StatusCode, Json<Annotation>)> {
    let cfi = validate::required("cfi", &req.cfi, MAX_CFI)?;
    let note = validate::optional("note", req.note, MAX_NOTE)?;
    let chapter = validate::optional("chapter", req.chapter, MAX_CHAPTER)?;
    let fraction = req.fraction.map(|f| f.clamp(0.0, 1.0));
    let (text, color) = match req.kind {
        AnnotationKind::Bookmark => (None, None),
        AnnotationKind::Highlight => (
            Some(validate::required(
                "text",
                req.text.as_deref().unwrap_or(""),
                MAX_TEXT,
            )?),
            Some(color(req.color)?.unwrap_or(0)),
        ),
    };
    let entity = book(&state, &slug).await?;

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM book_annotations WHERE user_id = $1 AND entity_id = $2",
    )
    .bind(user.id)
    .bind(entity.id)
    .fetch_one(&state.db)
    .await?;
    if count >= MAX_PER_BOOK {
        return Err(AppError::Conflict(format!(
            "не больше {MAX_PER_BOOK} пометок в книге"
        )));
    }

    let row: Annotation = sqlx::query_as(&format!(
        "INSERT INTO book_annotations
             (user_id, entity_id, kind, cfi, text, color, note, chapter, fraction)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (user_id, entity_id, cfi) WHERE kind = 'bookmark'
             DO UPDATE SET chapter = COALESCE(EXCLUDED.chapter, book_annotations.chapter)
         RETURNING {COLUMNS}"
    ))
    .bind(user.id)
    .bind(entity.id)
    .bind(req.kind)
    .bind(&cfi)
    .bind(&text)
    .bind(color)
    .bind(&note)
    .bind(&chapter)
    .bind(fraction)
    .fetch_one(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// Изменить цвет или заметку своей пометки.
#[utoipa::path(
    patch, operation_id = "update_annotation", path = "/annotations/{id}", tag = "annotations",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id пометки")),
    request_body = UpdateAnnotation,
    responses(
        (status = 200, description = "Изменена", body = Annotation),
        (status = 400, description = "Неверный цвет или длина заметки", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой своей пометки", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateAnnotation>,
) -> AppResult<Json<Annotation>> {
    let color = color(req.color)?;
    let (set_note, note) = match req.note {
        None => (false, None),
        Some(note) => (true, validate::optional("note", note, MAX_NOTE)?),
    };
    // Цвет — только у выделения: у закладки он остаётся NULL.
    let row: Option<Annotation> = sqlx::query_as(&format!(
        "UPDATE book_annotations SET
             color = CASE WHEN kind = 'highlight' THEN COALESCE($3, color) ELSE color END,
             note = CASE WHEN $4 THEN $5 ELSE note END
         WHERE id = $1 AND user_id = $2
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(user.id)
    .bind(color)
    .bind(set_note)
    .bind(&note)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(row.ok_or(AppError::NotFound)?))
}

/// Удалить свою пометку.
#[utoipa::path(
    delete, operation_id = "delete_annotation", path = "/annotations/{id}", tag = "annotations",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id пометки")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой своей пометки", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let deleted: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM book_annotations WHERE id = $1 AND user_id = $2 RETURNING id",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?;
    deleted.ok_or(AppError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Свои пометки во всех книгах, новые сверху: раздел профиля.
#[utoipa::path(
    get, operation_id = "list_own_annotations", path = "/users/me/annotations", tag = "annotations",
    security(("bearer" = [])),
    params(AnnotationsQuery),
    responses(
        (status = 200, description = "Страница пометок с книгами", body = Page<AnnotationWithEntity>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<AnnotationsQuery>,
) -> AppResult<Json<Page<AnnotationWithEntity>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM book_annotations WHERE user_id = $1 AND ($2::text IS NULL OR kind = $2)",
    )
    .bind(user.id)
    .bind(query.kind)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<AnnotationRow> = sqlx::query_as(&format!(
        "SELECT entity_id, {COLUMNS} FROM book_annotations
         WHERE user_id = $1 AND ($2::text IS NULL OR kind = $2)
         ORDER BY created_at DESC, id LIMIT $3 OFFSET $4"
    ))
    .bind(user.id)
    .bind(query.kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let entities = refs::entities(&state, rows.iter().map(|r| r.entity_id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|row| {
            Some(AnnotationWithEntity {
                entity: entities.get(&row.entity_id)?.clone(),
                annotation: row.annotation,
            })
        })
        .collect();
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}
