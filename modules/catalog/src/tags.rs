//! Теги и жанры. Чтение без авторизации.

use crate::models::{ListTagsQuery, TagWithCount};
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AppError, AppResult, AppState};

/// Все теги по алфавиту с числом активных сущностей (счётчик держат триггеры); `?kind=genre` —
/// только жанры. Тегов немного, поэтому
/// без пагинации.
#[utoipa::path(
    get, operation_id = "list_tags", path = "/tags", tag = "catalog",
    params(ListTagsQuery),
    responses(
        (status = 200, description = "Теги", body = Vec<TagWithCount>),
        (status = 400, description = "Неверный `kind`", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListTagsQuery>,
) -> AppResult<Json<Vec<TagWithCount>>> {
    let tags = sqlx::query_as(
        "SELECT t.id, t.slug, t.name, t.kind, t.entities_count::int8 AS entities_count
         FROM tags t
         WHERE $1::text IS NULL OR t.kind = $1
         ORDER BY t.name",
    )
    .bind(query.kind)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(tags))
}

/// Тег по slug: страница тега.
#[utoipa::path(
    get, operation_id = "get_tag", path = "/tags/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug тега", example = "sci-fi")),
    responses(
        (status = 200, description = "Тег", body = TagWithCount),
        (status = 404, description = "Не найден", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<TagWithCount>> {
    let tag: Option<TagWithCount> = sqlx::query_as(
        "SELECT t.id, t.slug, t.name, t.kind, t.entities_count::int8 AS entities_count
         FROM tags t WHERE t.slug = $1",
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(tag.ok_or(AppError::NotFound)?))
}
