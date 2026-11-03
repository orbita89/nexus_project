//! Теги и жанры. Чтение без авторизации.

use crate::models::{EntityKind, ListTagsQuery, TagWithCount};
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AppError, AppResult, AppState};

/// Колонки [`TagWithCount`]: счётчики держат триггеры, разбивка по типу работы — в
/// `tag_kind_counts` (типов у тега единицы, подзапрос идёт по PK).
const TAG_COUNT_COLUMNS: &str = "t.id, t.slug, t.name, t.kind, t.description,
     t.entities_count::int8 AS entities_count,
     COALESCE((SELECT jsonb_object_agg(k.kind, k.entities_count) FROM tag_kind_counts k
               WHERE k.tag_id = t.id AND k.entities_count > 0), '{}'::jsonb) AS by_kind";

/// Все теги по алфавиту с числом активных сущностей (счётчик держат триггеры); `?kind=genre` —
/// только жанры, `?entity_kind=painting` — только теги с активными картинами. Тегов немного,
/// поэтому без пагинации.
#[utoipa::path(
    get, operation_id = "list_tags", path = "/tags", tag = "catalog",
    params(ListTagsQuery),
    responses(
        (status = 200, description = "Теги", body = Vec<TagWithCount>),
        (status = 400, description = "Неверный `kind` или `entity_kind`", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListTagsQuery>,
) -> AppResult<Json<Vec<TagWithCount>>> {
    let tags = sqlx::query_as(&format!(
        "SELECT {TAG_COUNT_COLUMNS}
         FROM tags t
         WHERE ($1::text IS NULL OR t.kind = $1)
           AND ($2::text IS NULL OR EXISTS (
                   SELECT 1 FROM tag_kind_counts k
                   WHERE k.tag_id = t.id AND k.kind = $2 AND k.entities_count > 0))
         ORDER BY t.name"
    ))
    .bind(query.kind)
    .bind(query.entity_kind.map(EntityKind::as_str))
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
    let tag: Option<TagWithCount> = sqlx::query_as(&format!(
        "SELECT {TAG_COUNT_COLUMNS} FROM tags t WHERE t.slug = $1"
    ))
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(tag.ok_or(AppError::NotFound)?))
}
