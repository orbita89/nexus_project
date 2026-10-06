//! Чужие данные через справочники `shared::directory`: social не читает `entities` и `users`.
//!
//! Схема для списков: выбрать свои строки, собрать id, одним вызовом `by_ids` получить ссылки
//! на сущности и пользователей. Строка, для которой ссылки не нашлось (сущность или пользователь
//! удалены прямо сейчас, каскад ещё не дошёл), пропускается.

use crate::models::{
    Collection, CollectionItem, CollectionItemRow, CollectionRow, Interest, InterestRow, Post,
    PostRow, Review, ReviewRow, Thread, ThreadRow,
};
use shared::directory::{EntityRef, UserRef};
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

pub async fn collections(state: &AppState, rows: Vec<CollectionRow>) -> AppResult<Vec<Collection>> {
    let users = users(state, rows.iter().map(|r| r.user_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Collection {
                owner: users.get(&row.user_id)?.clone(),
                id: row.id,
                title: row.title,
                description: row.description,
                is_public: row.is_public,
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
    let entities = entities(state, rows.iter().map(|r| r.entity_id).collect()).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(CollectionItem {
                entity: entities.get(&row.entity_id)?.clone(),
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
