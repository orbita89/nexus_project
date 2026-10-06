//! Лента вошедшего пользователя. Собирается при чтении из своих таблиц social, ярусами:
//!
//! 1. **Подписки** — рецензии (включая оценки без текста), темы и публичные коллекции тех, на кого
//!    подписан пользователь; новые сверху.
//! 2. **Интересы** — темы о сущностях из интересов и рецензии на них с текстом; новые сверху. Без
//!    записей из первого яруса.
//! 3. **Популярное** — остальные темы, больше сообщений сверху. Видно, когда личное закончилось
//!    или его нет (пустая лента новичка). Для начала так; позже — сложнее.
//!
//! Своих записей в ленте нет. Пагинация — курсором: он помнит ярус и место в нём, поэтому новые
//! записи сверху не сдвигают страницы и не дают дублей.

use crate::models::{
    CollectionRow, FeedItem, FeedItemType, FeedPage, FeedQuery, FeedReason, ReviewRow, ThreadRow,
    COLLECTION_COLUMNS, REVIEW_COLUMNS, THREAD_COLUMNS,
};
use crate::{refs, threads};
use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::pagination::page_bounds;
use shared::{AppError, AppResult, AppState, AuthUser};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Ярусы по порядку показа.
const FOLLOWS: u8 = 0;
const INTERESTS: u8 = 1;
const POPULAR: u8 = 2;

/// Лента: подписки, затем интересы, затем популярное.
#[utoipa::path(
    get, operation_id = "get_feed", path = "/feed", tag = "feed",
    security(("bearer" = [])),
    params(FeedQuery),
    responses(
        (status = 200, description = "Страница ленты", body = FeedPage),
        (status = 400, description = "Неверный параметр или курсор", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<FeedQuery>,
) -> AppResult<Json<FeedPage>> {
    let (limit, _) = page_bounds(query.limit, None);
    let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
    let kind = query.item_type.map(kind_name);

    let followees: Vec<Uuid> =
        sqlx::query_scalar("SELECT followee_id FROM follows WHERE follower_id = $1")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?;
    let interests: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM user_interests WHERE user_id = $1")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?;

    // На одну запись больше лимита: так видно, есть ли следующая страница (в том числе в
    // следующем ярусе).
    let wanted = usize::try_from(limit).unwrap_or(0) + 1;
    let mut rows: Vec<(u8, Row)> = Vec::new();
    for tier in cursor.as_ref().map_or(FOLLOWS, |c| c.tier)..=POPULAR {
        let source = match tier {
            FOLLOWS if followees.is_empty() => continue,
            INTERESTS if interests.is_empty() => continue,
            _ => tier_sql(tier),
        };
        let after = cursor.as_ref().filter(|c| c.tier == tier);
        let found: Vec<Row> = sqlx::query_as(source)
            .bind(user.id)
            .bind(&followees)
            .bind(&interests)
            .bind(kind)
            .bind(after.map(|c| c.count))
            .bind(after.map(|c| c.at))
            .bind(after.map(|c| c.id))
            .bind(i64::try_from(wanted - rows.len()).unwrap_or(i64::MAX))
            .fetch_all(&state.db)
            .await?;
        rows.extend(found.into_iter().map(|row| (tier, row)));
        if rows.len() >= wanted {
            break;
        }
    }

    let next_cursor = if rows.len() >= wanted {
        rows.truncate(wanted - 1);
        rows.last().map(|(tier, row)| {
            Cursor {
                tier: *tier,
                count: row.rank_count,
                at: row.rank_at,
                id: row.id,
            }
            .encode()
        })
    } else {
        None
    };
    let items = items(&state, rows, &followees, &interests).await?;
    Ok(Json(FeedPage {
        items,
        next_cursor,
        limit,
    }))
}

fn kind_name(kind: FeedItemType) -> &'static str {
    match kind {
        FeedItemType::Review => "review",
        FeedItemType::Thread => "thread",
        FeedItemType::Collection => "collection",
    }
}

/// Строка яруса: что за запись и где она в порядке яруса.
#[derive(Debug, sqlx::FromRow)]
struct Row {
    kind: String,
    id: Uuid,
    created_at: DateTime<Utc>,
    /// Ключ сортировки: у подписок и интересов — `0` и время создания, у популярного — число
    /// сообщений и время последнего.
    rank_count: i32,
    rank_at: DateTime<Utc>,
}

/// SQL яруса. Параметры у всех одни: `$1` — пользователь, `$2` — на кого подписан, `$3` —
/// интересы, `$4` — тип записи или NULL, `$5`–`$7` — позиция курсора в этом ярусе или NULL,
/// `$8` — сколько строк.
fn tier_sql(tier: u8) -> &'static str {
    match tier {
        FOLLOWS => {
            "SELECT kind, id, created_at, 0 AS rank_count, created_at AS rank_at FROM (
                SELECT 'review' AS kind, r.id, r.created_at FROM reviews r
                WHERE r.user_id = ANY($2)
                UNION ALL
                SELECT 'thread', t.id, t.created_at FROM forum_threads t
                WHERE t.author_id = ANY($2)
                UNION ALL
                SELECT 'collection', c.id, c.created_at FROM collections c
                WHERE c.user_id = ANY($2) AND c.is_public
             ) f
             WHERE ($4::text IS NULL OR kind = $4)
               AND ($6::timestamptz IS NULL OR (created_at, id) < ($6, $7::uuid))
             ORDER BY created_at DESC, id DESC LIMIT $8"
        }
        INTERESTS => {
            "SELECT kind, id, created_at, 0 AS rank_count, created_at AS rank_at FROM (
                SELECT 'review' AS kind, r.id, r.created_at FROM reviews r
                WHERE r.entity_id = ANY($3) AND r.body IS NOT NULL
                  AND r.user_id <> $1 AND r.user_id <> ALL($2)
                UNION ALL
                SELECT 'thread', t.id, t.created_at FROM forum_threads t
                WHERE t.author_id <> $1 AND t.author_id <> ALL($2)
                  AND EXISTS (SELECT 1 FROM forum_thread_entities te
                              WHERE te.thread_id = t.id AND te.entity_id = ANY($3))
             ) f
             WHERE ($4::text IS NULL OR kind = $4)
               AND ($6::timestamptz IS NULL OR (created_at, id) < ($6, $7::uuid))
             ORDER BY created_at DESC, id DESC LIMIT $8"
        }
        _ => {
            "SELECT 'thread' AS kind, t.id, t.created_at,
                    t.posts_count AS rank_count, t.last_post_at AS rank_at
             FROM forum_threads t
             WHERE t.author_id <> $1 AND t.author_id <> ALL($2)
               AND NOT EXISTS (SELECT 1 FROM forum_thread_entities te
                               WHERE te.thread_id = t.id AND te.entity_id = ANY($3))
               AND ($4::text IS NULL OR $4 = 'thread')
               AND ($5::int IS NULL
                    OR (t.posts_count, t.last_post_at, t.id) < ($5, $6::timestamptz, $7::uuid))
             ORDER BY t.posts_count DESC, t.last_post_at DESC, t.id DESC LIMIT $8"
        }
    }
}

/// Строки ярусов → записи ленты: DTO каждого типа одним запросом, причины показа.
async fn items(
    state: &AppState,
    rows: Vec<(u8, Row)>,
    followees: &[Uuid],
    interests: &[Uuid],
) -> AppResult<Vec<FeedItem>> {
    let ids = |kind: &str| -> Vec<Uuid> {
        rows.iter()
            .filter(|(_, row)| row.kind == kind)
            .map(|(_, row)| row.id)
            .collect()
    };

    let review_rows: Vec<ReviewRow> = sqlx::query_as(&format!(
        "SELECT {REVIEW_COLUMNS} FROM reviews WHERE id = ANY($1)"
    ))
    .bind(ids("review"))
    .fetch_all(&state.db)
    .await?;
    let mut reviews: HashMap<Uuid, _> = refs::reviews(state, review_rows)
        .await?
        .into_iter()
        .map(|review| (review.id, review))
        .collect();

    let thread_rows: Vec<ThreadRow> = sqlx::query_as(&format!(
        "SELECT {THREAD_COLUMNS} FROM forum_threads t WHERE t.id = ANY($1)"
    ))
    .bind(ids("thread"))
    .fetch_all(&state.db)
    .await?;
    let mut threads: HashMap<Uuid, _> = threads::cards(state, thread_rows)
        .await?
        .into_iter()
        .map(|thread| (thread.id, thread))
        .collect();

    let collection_rows: Vec<CollectionRow> = sqlx::query_as(&format!(
        "SELECT {COLLECTION_COLUMNS} FROM collections c WHERE c.id = ANY($1)"
    ))
    .bind(ids("collection"))
    .fetch_all(&state.db)
    .await?;
    let mut collections: HashMap<Uuid, _> = refs::collections(state, collection_rows)
        .await?
        .into_iter()
        .map(|collection| (collection.id, collection))
        .collect();

    let followees: HashSet<Uuid> = followees.iter().copied().collect();
    let interests: HashSet<Uuid> = interests.iter().copied().collect();
    // Запись, удалённая между запросами или без ссылки в справочнике, пропускается.
    Ok(rows
        .into_iter()
        .filter_map(|(tier, row)| {
            let mut item = FeedItem {
                item_type: FeedItemType::Review,
                created_at: row.created_at,
                review: None,
                thread: None,
                collection: None,
                reasons: Vec::new(),
            };
            let (author, entities) = match row.kind.as_str() {
                "review" => {
                    let review = reviews.remove(&row.id)?;
                    let found = (review.author.clone(), vec![review.entity.clone()]);
                    item.review = Some(review);
                    found
                }
                "thread" => {
                    let thread = threads.remove(&row.id)?;
                    let found = (thread.author.clone(), thread.entities.clone());
                    item.item_type = FeedItemType::Thread;
                    item.thread = Some(thread);
                    found
                }
                _ => {
                    let collection = collections.remove(&row.id)?;
                    let found = (collection.owner.clone(), Vec::new());
                    item.item_type = FeedItemType::Collection;
                    item.collection = Some(collection);
                    found
                }
            };
            item.reasons = if tier == POPULAR {
                vec![FeedReason::Popular]
            } else {
                let follow = followees
                    .contains(&author.id)
                    .then_some(FeedReason::Follow { user: author });
                follow
                    .into_iter()
                    .chain(
                        entities
                            .into_iter()
                            .filter(|entity| interests.contains(&entity.id))
                            .map(|entity| FeedReason::Interest { entity }),
                    )
                    .collect()
            };
            Some(item)
        })
        .collect())
}

/// Место в ленте после последней записи страницы. Для клиента — непрозрачная строка.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Cursor {
    tier: u8,
    count: i32,
    at: DateTime<Utc>,
    id: Uuid,
}

impl Cursor {
    fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    fn decode(value: &str) -> AppResult<Self> {
        URL_SAFE_NO_PAD
            .decode(value)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|cursor| cursor.tier <= POPULAR)
            .ok_or_else(|| AppError::BadRequest("invalid cursor".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip() {
        let cursor = Cursor {
            tier: INTERESTS,
            count: 3,
            at: Utc::now(),
            id: Uuid::new_v4(),
        };
        assert_eq!(Cursor::decode(&cursor.encode()).unwrap(), cursor);
        assert!(Cursor::decode("garbage").is_err());
        let bad_tier = Cursor { tier: 9, ..cursor };
        assert!(Cursor::decode(&bad_tier.encode()).is_err());
    }
}
