//! Шина событий: модули сообщают о том, что произошло, а `realtime` рассылает это клиентам.
//!
//! Модуль не вызывает `realtime` напрямую (см. «Правила границ» в `documents/architecture.md`):
//! он публикует [`Event`] в [`EventBus`] из [`AppState`](crate::AppState), а `realtime`
//! подписан на шину. Публиковать — **после** `commit`, чтобы клиент, получивший событие и
//! перечитавший данные по API, их увидел.
//!
//! В событии только id (`data`): тексты и имена клиент берёт по API. Так событие не
//! устаревает и не раскрывает удалённое.
//!
//! Сейчас шина — `tokio::sync::broadcast` внутри процесса (один экземпляр приложения). Когда
//! экземпляров станет несколько, за тем же API появится мост через Redis pub/sub.

use serde_json::Value;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Сколько событий шина держит для отстающего подписчика. Отставший больше — получает
/// `RecvError::Lagged` и сообщает клиенту, что тот пропустил события.
pub const BUS_CAPACITY: usize = 1024;

/// Куда адресовано событие.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Всё о теме форума: новые, изменённые и удалённые сообщения, изменения темы.
    Thread(Uuid),
    /// Всё о сущности каталога: новые темы и сообщения о ней, рецензии.
    Entity(Uuid),
    /// Личное: ответы на сообщения пользователя, изменения его интересов.
    User(Uuid),
}

/// Событие: тип (`post.created`, ...), каналы и данные (только id).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: &'static str,
    pub channels: Vec<Channel>,
    pub data: Value,
}

impl Event {
    pub fn new(kind: &'static str, channels: Vec<Channel>, data: Value) -> Self {
        Self {
            kind,
            channels,
            data,
        }
    }
}

/// Шина событий. Клонируется дёшево (это хэндл).
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Arc<Event>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(BUS_CAPACITY)
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            sender: broadcast::channel(capacity).0,
        }
    }

    /// Опубликовать событие. Без подписчиков оно просто теряется: ошибкой это не считается,
    /// публикация не должна валить запрос.
    pub fn publish(&self, event: Event) {
        tracing::debug!(kind = event.kind, channels = ?event.channels, "event published");
        let _ = self.sender.send(Arc::new(event));
    }

    /// Получать все события, опубликованные после вызова.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn subscriber_gets_events_published_after_subscribe() {
        let bus = EventBus::default();
        bus.publish(Event::new("lost", vec![], json!({})));
        let mut rx = bus.subscribe();
        let thread = Uuid::new_v4();
        bus.publish(Event::new(
            "post.created",
            vec![Channel::Thread(thread)],
            json!({ "thread_id": thread }),
        ));
        let event = rx.recv().await.unwrap();
        assert_eq!(event.kind, "post.created");
        assert_eq!(event.channels, [Channel::Thread(thread)]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn publish_without_subscribers_is_fine() {
        EventBus::new(1).publish(Event::new("x", vec![], json!(null)));
    }
}
