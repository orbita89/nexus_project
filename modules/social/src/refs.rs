//! Чужие данные через справочники `shared::directory`: social не читает `entities` и `users`.
//!
//! Схема для списков: выбрать свои строки, собрать id, одним вызовом `by_ids` получить ссылки
//! на сущности и пользователей. Строка, для которой ссылки не нашлось (сущность или пользователь
//! удалены прямо сейчас, каскад ещё не дошёл), пропускается.

use crate::models::{
    Collection, CollectionEntity, CollectionItem, CollectionItemRow, CollectionRow, Interest,
    InterestRow, MarkRow, MarkedEntity, Post, PostRow, Review, ReviewComment, ReviewCommentRow,
    ReviewRow, Thread, ThreadRow,
};
use shared::directory::{EntityRef, PersonRef, TagRef, UserRef};
use shared::{AppError, AppResult, AppState};
use std::collections::HashMap;
use uuid::Uuid;

/// Сущность по slug из URL, иначе 404.
pub async fn entity(state: &AppState, slug: &str) -> AppResult<EntityRef> {
    state
        .entities
        .by_slug(slug)
        .await?
        .ok_or(AppError::NotFound)
}

/// Человек каталога по slug из URL (только публичный), иначе 404.
pub async fn person(state: &AppState, slug: &str) -> AppResult<PersonRef> {
    state.people.by_slug(slug).await?.ok_or(AppError::NotFound)
}

/// Тег каталога по slug из URL, иначе 404.
pub async fn tag(state: &AppState, slug: &str) -> AppResult<TagRef> {
    state.tags.by_slug(slug).await?.ok_or(AppError::NotFound)
}

pub async fn people(state: &AppState, ids: Vec<Uuid>) -> AppResult<HashMap<Uuid, PersonRef>> {
    state.people.by_ids(&dedup(ids)).await
}

pub async fn tags(state: &AppState, ids: Vec<Uuid>) -> AppResult<HashMap<Uuid, TagRef>> {
    state.tags.by_ids(&dedup(ids)).await
}

/// Оценки, рецензии и реакции — только после выхода: дата выхода позже сегодняшней (UTC) → 409
/// `not_released`. Без даты — можно.
pub fn require_released(entity: &EntityRef) -> AppResult<()> {
    let today = chrono::Utc::now().date_naive();
    match entity.release_date {
        Some(date) if date > today => Err(AppError::Conflict(format!(
            "entity is not released yet (release date {date})"
        ))
        .with_code("not_released")),
        _ => Ok(()),
    }
}

/// Активный пользователь по username из URL, иначе 404.
pub async fn user(state: &AppState, username: &str) -> AppResult<UserRef> {
    state
        .users
        .by_username(username)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn users(state: &AppState, ids: Vec<Uuid>) -> AppResult<HashMap<Uuid, UserRef>> {
    state.users.by_ids(&dedup(ids)).await
}

pub async fn entities(state: &AppState, ids: Vec<Uuid>) -> AppResult<HashMap<Uuid, EntityRef>> {
    state.entities.by_ids(&dedup(ids)).await
}

fn dedup(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

pub async fn reviews(state: &AppState, rows: Vec<ReviewRow>) -> AppResult<Vec<Review>> {
    let users = users(state, rows.iter().map(|r| r.user_id).collect()).await?;
    let entities = entities(state, rows.iter().map(|r| r.entity_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Review {
                author: users.get(&row.user_id)?.clone(),
                entity: entities.get(&row.entity_id)?.clone(),
                id: row.id,
                rating: row.rating,
                body: row.body,
                is_spoiler: row.is_spoiler,
                likes_count: row.likes_count,
                dislikes_count: row.dislikes_count,
                comments_count: row.comments_count,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

pub async fn review(state: &AppState, row: ReviewRow) -> AppResult<Review> {
    reviews(state, vec![row])
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

pub async fn comments(
    state: &AppState,
    rows: Vec<ReviewCommentRow>,
) -> AppResult<Vec<ReviewComment>> {
    let users = users(state, rows.iter().map(|r| r.author_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(ReviewComment {
                author: users.get(&row.author_id)?.clone(),
                id: row.id,
                review_id: row.review_id,
                body: row.body,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

/// Сколько произведений коллекции показывать обложками в списке.
const PREVIEW_ITEMS: i64 = 4;

pub async fn collections(state: &AppState, rows: Vec<CollectionRow>) -> AppResult<Vec<Collection>> {
    let users = users(state, rows.iter().map(|r| r.user_id).collect()).await?;
    // Первые пункты всех коллекций страницы — одним запросом.
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let firsts: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT collection_id, entity_id FROM (
             SELECT collection_id, entity_id,
                    row_number() OVER (PARTITION BY collection_id
                                       ORDER BY position, added_at, entity_id) AS n
             FROM collection_items WHERE collection_id = ANY($1)
         ) i WHERE n <= $2 ORDER BY collection_id, n",
    )
    .bind(&ids)
    .bind(PREVIEW_ITEMS)
    .fetch_all(&state.db)
    .await?;
    let entities = entities(state, firsts.iter().map(|&(_, e)| e).collect()).await?;
    let mut previews: HashMap<Uuid, Vec<EntityRef>> = HashMap::new();
    for (collection, entity) in firsts {
        if let Some(entity) = entities.get(&entity) {
            previews.entry(collection).or_default().push(entity.clone());
        }
    }
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Collection {
                owner: users.get(&row.user_id)?.clone(),
                preview: previews.remove(&row.id).unwrap_or_default(),
                contains: None,
                id: row.id,
                title: row.title,
                description: row.description,
                is_public: row.is_public,
                is_featured: row.featured_position.is_some(),
                featured_position: row.featured_position,
                slug: row.slug,
                wallpaper_url: row.wallpaper_url,
                cover_url: row.cover_url,
                theme: row.theme,
                items_count: row.items_count,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect())
}

pub async fn collection(state: &AppState, row: CollectionRow) -> AppResult<Collection> {
    collections(state, vec![row])
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

pub async fn items(
    state: &AppState,
    rows: Vec<CollectionItemRow>,
) -> AppResult<Vec<CollectionItem>> {
    let ids: Vec<Uuid> = rows.iter().map(|r| r.entity_id).collect();
    let entities = entities(state, ids.clone()).await?;
    // Описания — одним запросом на всю коллекцию.
    let mut overviews = state.entities.overviews(&dedup(ids)).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(CollectionItem {
                entity: CollectionEntity {
                    entity: entities.get(&row.entity_id)?.clone(),
                    overview: overviews.remove(&row.entity_id),
                },
                position: row.position,
                note: row.note,
                added_at: row.added_at,
            })
        })
        .collect())
}

/// Темы. `links` — пары (тема, сущность) по порядку показа. Сущность, которой нет в справочнике,
/// из темы пропадает, а сама тема остаётся.
pub async fn threads(
    state: &AppState,
    rows: Vec<ThreadRow>,
    links: Vec<(Uuid, Uuid)>,
) -> AppResult<Vec<Thread>> {
    let users = users(state, rows.iter().map(|r| r.author_id).collect()).await?;
    let entities = entities(state, links.iter().map(|&(_, e)| e).collect()).await?;
    let mut by_thread: HashMap<Uuid, Vec<EntityRef>> = HashMap::new();
    for (thread, entity) in links {
        if let Some(entity) = entities.get(&entity) {
            by_thread.entry(thread).or_default().push(entity.clone());
        }
    }
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Thread {
                author: users.get(&row.author_id)?.clone(),
                entities: by_thread.remove(&row.id).unwrap_or_default(),
                id: row.id,
                title: row.title,
                posts_count: row.posts_count,
                last_post_at: row.last_post_at,
                is_locked: row.is_locked,
                edited_at: row.edited_at,
                created_at: row.created_at,
            })
        })
        .collect())
}

/// Сообщения. У заглушки удалённого (`body` = `NULL`) автор не показывается.
pub async fn posts(state: &AppState, rows: Vec<PostRow>) -> AppResult<Vec<Post>> {
    let ids = rows
        .iter()
        .filter(|r| r.body.is_some())
        .map(|r| r.author_id)
        .chain(rows.iter().filter_map(|r| r.parent_author_id))
        .collect();
    let users = users(state, ids).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let author = match row.body {
                Some(_) => Some(users.get(&row.author_id)?.clone()),
                None => None,
            };
            Some(Post {
                deleted: row.body.is_none(),
                reply_to: row.parent_author_id.and_then(|id| users.get(&id).cloned()),
                author,
                id: row.id,
                thread_id: row.thread_id,
                parent_id: row.parent_id,
                body: row.body,
                edited_at: row.edited_at,
                created_at: row.created_at,
            })
        })
        .collect())
}

pub async fn post(state: &AppState, row: PostRow) -> AppResult<Post> {
    posts(state, vec![row])
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

pub async fn interests(state: &AppState, rows: Vec<InterestRow>) -> AppResult<Vec<Interest>> {
    let entities = entities(state, rows.iter().map(|r| r.entity_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Interest {
                entity: entities.get(&row.entity_id)?.clone(),
                since: row.created_at,
            })
        })
        .collect())
}

pub async fn marks(state: &AppState, rows: Vec<MarkRow>) -> AppResult<Vec<MarkedEntity>> {
    let entities = entities(state, rows.iter().map(|r| r.entity_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(MarkedEntity {
                entity: entities.get(&row.entity_id)?.clone(),
                kind: row.kind,
                created_at: row.created_at,
            })
        })
        .collect())
}
