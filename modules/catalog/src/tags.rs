//! Теги и жанры. Чтение без авторизации.

use crate::models::{ListTagsQuery, TagWithCount};
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::{AppResult, AppState};

/// Все теги по алфавиту с числом сущностей; `?kind=genre` — только жанры. Тегов немного, поэтому
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
        "SELECT t.id, t.slug, t.name, t.kind, count(et.entity_id) AS entities_count
         FROM tags t LEFT JOIN entity_tags et ON et.tag_id = t.id
         WHERE $1::text IS NULL OR t.kind = $1
         GROUP BY t.id ORDER BY t.name",
    )
    .bind(query.kind)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(tags))
}
