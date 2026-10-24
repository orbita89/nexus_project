//! «Мои комментарии»: всё, что пользователь написал — к произведениям, людям и студиям
//! (`comments`), к рецензиям (`review_comments`) и в обсуждениях коллекций
//! (`collection_comments`), одной лентой, новые сверху.
//!
//! Три таблицы собираются `UNION ALL` — так страница и `total` считаются один раз, а не
//! склеиваются на клиенте из трёх списков с разными курсорами. Цель каждой строки раскрывается
//! пакетно: сущности, люди и студии — через справочники, рецензии и коллекции — свои таблицы.
//! Строка, для которой цель уже удалена, в ответ не попадает (справочник не вернёт ссылку).
//!
//! Видимость — как у остальной аналитики профиля (`stats::visible`): владелец и admin всегда,
//! остальные — если включён `stats_public`.

use crate::models::{PageQuery, UserComment, UserCommentRow, UserCommentTarget};
use crate::refs;
use crate::stats;
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppResult, AppState, AuthUser};
use std::collections::HashMap;
use uuid::Uuid;

/// Три источника одной лентой. `source` различает таблицу, `target_id` — строку, на которую
/// комментарий указывает (цель, рецензия или коллекция).
const FEED: &str = "
    SELECT id, target_kind AS source, target_id, body, created_at, updated_at
      FROM comments WHERE author_id = $1 AND deleted_at IS NULL
    UNION ALL
    SELECT id, 'review', review_id, body, created_at, updated_at
      FROM review_comments WHERE author_id = $1 AND deleted_at IS NULL
    UNION ALL
    SELECT id, 'collection', collection_id, body, created_at, updated_at
      FROM collection_comments WHERE author_id = $1 AND deleted_at IS NULL";

/// Комментарии пользователя, новые сверху.
#[utoipa::path(
    get, operation_id = "list_user_comments", path = "/users/{username}/comments", tag = "users",
    security((), ("bearer" = [])),
    params(
        ("username" = String, Path, description = "username", example = "user"),
        PageQuery,
    ),
    responses(
        (status = 200, description = "Страница комментариев", body = Page<UserComment>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Токен передан, но недействителен", body = ErrorBody),
        (status = 403, description = "Аналитика закрыта владельцем (`stats_private`)", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<UserComment>>> {
    let user = stats::visible(&state, &username, viewer.as_ref()).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM ({FEED}) AS feed"))
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<UserCommentRow> = sqlx::query_as(&format!(
        "SELECT * FROM ({FEED}) AS feed ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3"
    ))
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items: with_targets(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Раскрывает цели пакетно: по одному запросу на вид, без N+1.
async fn with_targets(state: &AppState, rows: Vec<UserCommentRow>) -> AppResult<Vec<UserComment>> {
    let ids = |source: &str| -> Vec<Uuid> {
        rows.iter()
            .filter(|row| row.source == source)
            .map(|row| row.target_id)
            .collect()
    };

    // Рецензия и коллекция — свои таблицы; у рецензии нужна ещё и сущность, о которой она.
    let review_ids = ids("review");
    let review_entities: HashMap<Uuid, Uuid> = if review_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as("SELECT id, entity_id FROM reviews WHERE id = ANY($1)")
            .bind(&review_ids)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect()
    };
    let collection_ids = ids("collection");
    let collections: HashMap<Uuid, String> = if collection_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as("SELECT id, title FROM collections WHERE id = ANY($1)")
            .bind(&collection_ids)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect()
    };

    let mut entity_ids = ids("entity");
    entity_ids.extend(review_entities.values().copied());
    let entities = refs::entities(state, entity_ids).await?;
    let people = refs::people(state, ids("person")).await?;
    let studios = refs::studios(state, ids("studio")).await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let target = match row.source.as_str() {
                "entity" => UserCommentTarget::Entity {
                    entity: entities.get(&row.target_id)?.clone(),
                },
                "person" => UserCommentTarget::Person {
                    person: people.get(&row.target_id)?.clone(),
                },
                "studio" => UserCommentTarget::Studio {
                    studio: studios.get(&row.target_id)?.clone(),
                },
                "review" => UserCommentTarget::Review {
                    review_id: row.target_id,
                    entity: entities.get(review_entities.get(&row.target_id)?)?.clone(),
                },
                "collection" => UserCommentTarget::Collection {
                    collection_id: row.target_id,
                    title: collections.get(&row.target_id)?.clone(),
                },
                _ => return None,
            };
            Some(UserComment {
                id: row.id,
                target,
                body: row.body,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}
