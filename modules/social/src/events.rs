//! События social для `realtime` (`shared::events`). Публикуются после `commit`, в данных только id.
//!
//! | Событие | Каналы | Данные |
//! |---|---|---|
//! | `thread.created` | сущности темы | `thread_id`, `author_id` |
//! | `thread.updated` | тема, сущности (старые и новые) | `thread_id` |
//! | `thread.deleted` | тема, сущности | `thread_id` |
//! | `post.created` | тема, сущности темы | `thread_id`, `post_id`, `parent_id`, `author_id` |
//! | `post.updated`, `post.deleted` | тема | `thread_id`, `post_id` |
//! | `reply.created` | автор родителя (если отвечает не он сам) | `thread_id`, `post_id`, `parent_id`, `author_id` |
//! | `review.created`, `review.updated`, `review.deleted` | сущность | `review_id`, `entity_id`, `author_id` |
//! | `interest.added`, `interest.removed` | пользователь | `entity_id` |

use serde_json::json;
use shared::events::{Channel, Event};
use shared::AppState;
use sqlx::PgExecutor;
use uuid::Uuid;

pub fn publish(state: &AppState, event: Event) {
    state.events.publish(event);
}

/// Сущности темы: каналы для событий темы и её сообщений.
pub async fn thread_entities(db: impl PgExecutor<'_>, thread_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar("SELECT entity_id FROM forum_thread_entities WHERE thread_id = $1")
        .bind(thread_id)
        .fetch_all(db)
        .await
}

fn entity_channels(entities: &[Uuid]) -> impl Iterator<Item = Channel> + '_ {
    entities.iter().map(|&id| Channel::Entity(id))
}

pub fn thread_created(thread_id: Uuid, author_id: Uuid, entities: &[Uuid]) -> Event {
    Event::new(
        "thread.created",
        entity_channels(entities).collect(),
        json!({ "thread_id": thread_id, "author_id": author_id }),
    )
}

/// `thread.updated` или `thread.deleted`: тема и её сущности.
pub fn thread_changed(kind: &'static str, thread_id: Uuid, entities: &[Uuid]) -> Event {
    let channels = std::iter::once(Channel::Thread(thread_id))
        .chain(entity_channels(entities))
        .collect();
    Event::new(kind, channels, json!({ "thread_id": thread_id }))
}

pub struct NewPost {
    pub thread_id: Uuid,
    pub post_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub author_id: Uuid,
}

impl NewPost {
    fn data(&self) -> serde_json::Value {
        json!({
            "thread_id": self.thread_id,
            "post_id": self.post_id,
            "parent_id": self.parent_id,
            "author_id": self.author_id,
        })
    }

    pub fn created(&self, entities: &[Uuid]) -> Event {
        let channels = std::iter::once(Channel::Thread(self.thread_id))
            .chain(entity_channels(entities))
            .collect();
        Event::new("post.created", channels, self.data())
    }

    /// Уведомление автору сообщения, на которое ответили (себе не уведомляем).
    pub fn reply(&self, parent_author: Option<Uuid>) -> Option<Event> {
        let to = parent_author.filter(|&to| to != self.author_id)?;
        Some(Event::new(
            "reply.created",
            vec![Channel::User(to)],
            self.data(),
        ))
    }
}

/// `post.updated` или `post.deleted`.
pub fn post_changed(kind: &'static str, thread_id: Uuid, post_id: Uuid) -> Event {
    Event::new(
        kind,
        vec![Channel::Thread(thread_id)],
        json!({ "thread_id": thread_id, "post_id": post_id }),
    )
}

/// `review.created`, `review.updated` или `review.deleted`.
pub fn review(kind: &'static str, review_id: Uuid, entity_id: Uuid, author_id: Uuid) -> Event {
    Event::new(
        kind,
        vec![Channel::Entity(entity_id)],
        json!({ "review_id": review_id, "entity_id": entity_id, "author_id": author_id }),
    )
}

/// `interest.added` или `interest.removed`: realtime по нему меняет подписки соединений пользователя.
pub fn interest(kind: &'static str, user_id: Uuid, entity_id: Uuid) -> Event {
    Event::new(
        kind,
        vec![Channel::User(user_id)],
        json!({ "entity_id": entity_id }),
    )
}
