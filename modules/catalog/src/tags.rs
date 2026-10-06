//! Теги. Чтение без авторизации.

use crate::models::TagWithCount;
use axum::extract::State;
use axum::Json;
use shared::{AppResult, AppState};

/// Все теги по алфавиту с числом сущностей. Тегов немного, поэтому без пагинации.
#[utoipa::path(
    get, operation_id = "list_tags", path = "/tags", tag = "catalog",
    responses((status = 200, description = "Теги", body = Vec<TagWithCount>))
)]
pub async fn list(State(state): State<AppState>) -> AppResult<Json<Vec<TagWithCount>>> {
    let tags = sqlx::query_as(
        "SELECT t.id, t.slug, t.name, count(et.entity_id) AS entities_count
         FROM tags t LEFT JOIN entity_tags et ON et.tag_id = t.id
         GROUP BY t.id ORDER BY t.name",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(tags))
}
