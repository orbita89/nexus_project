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
//! Шина внутри процесса (экземпляр один, Redis нет). Два вида подписки:
//! - [`EventBus::subscribe_channel`] — хаб: свой `broadcast` на каждый канал, создаётся при первой
//!   подписке и удаляется, когда подписчиков не осталось. Так подписчик (WS-соединение) получает
//!   только события своих каналов, а не весь поток;
//! - [`EventBus::subscribe`] — все события (тесты, диагностика); без подписчиков ничего не стоит.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Сколько событий шина держит для отстающего подписчика. Отставший больше — получает
/// `RecvError::Lagged` и сообщает клиенту, что тот пропустил события.
pub const BUS_CAPACITY: usize = 1024;
/// Очередь одного канала хаба: подписчик, отставший больше, получает `Lagged`.
pub const CHANNEL_CAPACITY: usize = 64;

/// Куда адресовано событие.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Всё о теме форума: новые, изменённые и удалённые сообщения, изменения темы.
    Thread(Uuid),
    /// Всё о сущности каталога: новые темы и сообщения о ней, рецензии.
    Entity(Uuid),
    /// Человек каталога: чат его комнаты.
    Person(Uuid),
    /// Тег каталога: чат его комнаты.
    Tag(Uuid),
    /// Премия каталога: чат её комнаты (`award:<slug>`).
    Award(Uuid),
    /// Церемония премии: чат её комнаты (`award:<slug>-<год>`).
    AwardEdition(Uuid),
    /// Своя комната чата (админа): сообщения, изменение и удаление комнаты.
    Room(Uuid),
    /// Личное: ответы на сообщения пользователя, изменения его интересов.
    User(Uuid),
    /// Личная переписка: сообщения и изменения в ней. Имя канала — `dm:<id>`.
    Dm(Uuid),
    /// Каталог комнат чата целиком: появилась новая комната. Общий на всех, подписка — вручную
    /// (`subscribe` на `rooms`), автоматически его никому не выдают: он нужен только открытой
    /// странице со списком комнат.
    Rooms,
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
    /// Хаб: канал → его `broadcast`. Есть только у каналов с подписчиками.
    channels: Arc<Mutex<HashMap<Channel, broadcast::Sender<Arc<Event>>>>>,
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
            channels: Arc::default(),
        }
    }

    /// Опубликовать событие. Без подписчиков оно просто теряется: ошибкой это не считается,
    /// публикация не должна валить запрос. В каждый канал события — один и тот же `Arc`:
    /// подписчик нескольких каналов узнаёт повтор по `Arc::ptr_eq`.
    pub fn publish(&self, event: Event) {
        tracing::debug!(kind = event.kind, channels = ?event.channels, "event published");
        let event = Arc::new(event);
        {
            let mut channels = self.lock();
            for channel in &event.channels {
                if let Some(sender) = channels.get(channel) {
                    if sender.send(event.clone()).is_err() {
                        // Все подписчики ушли, а release не дошёл — убираем канал здесь.
                        channels.remove(channel);
                    }
                }
            }
        }
        let _ = self.sender.send(event);
    }

    /// Получать события одного канала, опубликованные после вызова.
    pub fn subscribe_channel(&self, channel: Channel) -> broadcast::Receiver<Arc<Event>> {
        self.lock()
            .entry(channel)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Подписчик канала ушёл (его `Receiver` уже удалён): канал без подписчиков удаляется.
    pub fn release(&self, channel: Channel) {
        let mut channels = self.lock();
        if channels
            .get(&channel)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            channels.remove(&channel);
        }
    }

    /// Сколько каналов в хабе. Для тестов и метрик.
    pub fn channel_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Channel, broadcast::Sender<Arc<Event>>>> {
        self.channels.lock().unwrap_or_else(|e| e.into_inner())
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

    #[tokio::test]
    async fn channel_subscriber_gets_only_its_channel_and_hub_shrinks() {
        let bus = EventBus::default();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut rx = bus.subscribe_channel(Channel::Room(a));
        assert_eq!(bus.channel_count(), 1);
        bus.publish(Event::new("x", vec![Channel::Room(b)], json!({})));
        bus.publish(Event::new(
            "y",
            vec![Channel::Room(a), Channel::Room(b)],
            json!({}),
        ));
        assert_eq!(rx.recv().await.unwrap().kind, "y");
        assert!(rx.try_recv().is_err());
        drop(rx);
        bus.release(Channel::Room(a));
        assert_eq!(bus.channel_count(), 0);
    }

    #[tokio::test]
    async fn release_keeps_channel_with_other_subscribers() {
        let bus = EventBus::default();
        let room = Channel::Room(Uuid::new_v4());
        let first = bus.subscribe_channel(room);
        let mut second = bus.subscribe_channel(room);
        drop(first);
        bus.release(room);
        assert_eq!(bus.channel_count(), 1);
        bus.publish(Event::new("z", vec![room], json!({})));
        assert_eq!(second.recv().await.unwrap().kind, "z");
    }

    #[test]
    fn publish_without_subscribers_is_fine() {
        EventBus::new(1).publish(Event::new("x", vec![], json!(null)));
    }
}
