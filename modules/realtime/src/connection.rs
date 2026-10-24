//! Одно WebSocket-соединение: сообщения клиента, события шины, heartbeat и срок токена.
//!
//! Соединение открывается гостем: можно подписаться на публичные каналы (`thread:`, `entity:`,
//! `person:`, `tag:`, `room:`). `auth` с access-токеном добавляет личный канал `user:me` и каналы
//! интересов (сущности, люди, теги, свои комнаты, в которые вступил).
//! Токен живёт 15 минут: за минуту до конца сервер присылает `auth_expiring`, клиент обновляет
//! токен через REST и присылает новый `auth` — соединение и подписки не прерываются. Не прислал —
//! соединение становится гостевым (`auth_expired`): личные каналы отключаются, ручные остаются.

use crate::channels::{self, USER_ME};
use crate::registry::{Registry, Slot};
use crate::{EXPIRY_WARNING, HEARTBEAT, IDLE_TIMEOUT, MAX_CHANNELS, MAX_CONNECTIONS_PER_USER};
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket};
use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use shared::directory::ChatRoom;
use shared::events::{Channel, Event};
use shared::{AppError, AppState};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{interval_at, sleep_until, Instant, MissedTickBehavior};
use uuid::Uuid;

/// Сообщения клиента.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Войти или продлить вход новым access-токеном.
    Auth {
        token: String,
    },
    Subscribe {
        channels: Vec<String>,
    },
    Unsubscribe {
        channels: Vec<String>,
    },
    Ping,
    /// Сообщение в чат комнаты (`entity:<slug>`); `client_id` возвращается в ответе.
    #[serde(rename = "chat.send")]
    ChatSend {
        room: String,
        body: String,
        #[serde(default)]
        reply_to: Option<Uuid>,
        #[serde(default)]
        client_id: Option<String>,
    },
    /// Прочитано в комнате до `message_id` включительно.
    #[serde(rename = "chat.read")]
    ChatRead {
        room: String,
        message_id: Uuid,
    },
}

/// Вошедший пользователь соединения.
struct Session {
    user_id: Uuid,
    /// Срок токена (unix-время, секунды).
    expires_at: i64,
    /// `auth_expiring` уже отправлен.
    warned: bool,
    _slot: Slot,
}

struct Conn {
    state: AppState,
    registry: Arc<Registry>,
    session: Option<Session>,
    /// Подписки клиента: канал → имя, как его видит клиент.
    manual: HashMap<Channel, String>,
    /// Каналы интересов вошедшего: сущности, люди, теги.
    interests: HashMap<Channel, String>,
}

pub async fn run(mut socket: WebSocket, state: AppState, registry: Arc<Registry>) {
    let mut events = state.events.subscribe();
    let mut conn = Conn {
        state,
        registry,
        session: None,
        manual: HashMap::new(),
        interests: HashMap::new(),
    };
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();

    loop {
        let deadline = conn.deadline();
        let replies = tokio::select! {
            incoming = socket.recv() => {
                last_seen = Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => conn.handle(text.as_str()).await,
                    Some(Ok(Message::Binary(_))) => vec![error("binary messages are not supported")],
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => Vec::new(),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
            event = events.recv() => match event {
                Ok(event) => conn.deliver(&event).await,
                // Клиент не успевает читать: события пропущены, пусть перечитает данные по API.
                Err(RecvError::Lagged(missed)) => vec![json!({ "type": "lagged", "missed": missed })],
                Err(RecvError::Closed) => break,
            },
            _ = heartbeat.tick() => {
                if last_seen.elapsed() > IDLE_TIMEOUT {
                    let close = CloseFrame { code: close_code::AWAY, reason: "idle timeout".into() };
                    let _ = socket.send(Message::Close(Some(close))).await;
                    break;
                }
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
                Vec::new()
            }
            _ = sleep_until(deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))),
                if deadline.is_some() => conn.on_deadline(),
        };
        for reply in replies {
            if socket
                .send(Message::Text(reply.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

fn error(message: impl Into<String>) -> Value {
    json!({ "type": "error", "error": message.into() })
}

/// Ошибка чата → `error`; у лимита частоты — `retry_after` (секунды).
fn chat_error(e: AppError) -> Value {
    match e {
        AppError::BadRequest(message) => error(message),
        AppError::NotFound => error("not found"),
        AppError::Forbidden => error("forbidden"),
        AppError::TooManyRequests { retry_after_secs } => json!({
            "type": "error",
            "error": "too many messages",
            "retry_after": retry_after_secs,
        }),
        e => internal(e),
    }
}

fn internal(e: AppError) -> Value {
    tracing::error!(error = %e, "realtime");
    error("internal error")
}

/// Unix-время (секунды) → момент `tokio::time`.
fn instant_at(unix: i64) -> Instant {
    let left = unix - Utc::now().timestamp();
    Instant::now() + Duration::from_secs(u64::try_from(left).unwrap_or(0))
}

fn rfc3339(unix: i64) -> String {
    Utc.timestamp_opt(unix, 0)
        .single()
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

impl Conn {
    async fn handle(&mut self, text: &str) -> Vec<Value> {
        let message: ClientMessage = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(e) => return vec![error(format!("invalid message: {e}"))],
        };
        match message {
            ClientMessage::Auth { token } => vec![self.auth(&token).await],
            ClientMessage::Subscribe { channels } => vec![self.subscribe(&channels).await],
            ClientMessage::Unsubscribe { channels } => vec![self.unsubscribe(channels)],
            ClientMessage::Ping => vec![json!({ "type": "pong" })],
            ClientMessage::ChatSend {
                room,
                body,
                reply_to,
                client_id,
            } => vec![self.chat_send(&room, &body, reply_to, client_id).await],
            ClientMessage::ChatRead { room, message_id } => {
                vec![self.chat_read(&room, message_id).await]
            }
        }
    }

    /// `chat.send`: сообщение пишет `social` (`state.chat`), событие рассылает шина. Ответ
    /// отправителю — `chat.sent` с сообщением или `error`; в обоих — его `client_id`.
    async fn chat_send(
        &self,
        room: &str,
        body: &str,
        reply_to: Option<Uuid>,
        client_id: Option<String>,
    ) -> Value {
        let mut reply = match self.chat_room(room).await {
            Err(reply) => reply,
            Ok((user_id, chat_room, name)) => {
                match self
                    .state
                    .chat
                    .send(&self.state, user_id, chat_room, body, reply_to)
                    .await
                {
                    Ok(message) => json!({ "type": "chat.sent", "room": name, "message": message }),
                    Err(e) => chat_error(e),
                }
            }
        };
        reply["client_id"] = json!(client_id);
        reply
    }

    /// `chat.read`: прочитано до сообщения → `chat.read_saved`; другие устройства получат
    /// событие `chat.read` в `user:me`.
    async fn chat_read(&self, room: &str, message_id: Uuid) -> Value {
        let (user_id, chat_room, name) = match self.chat_room(room).await {
            Ok(found) => found,
            Err(reply) => return reply,
        };
        match self
            .state
            .chat
            .read(&self.state, user_id, chat_room, message_id)
            .await
        {
            Ok(()) => json!({ "type": "chat.read_saved", "room": name, "message_id": message_id }),
            Err(e) => chat_error(e),
        }
    }

    /// Вошедший и комната чата по имени; иначе — готовый ответ `error`.
    async fn chat_room(&self, room: &str) -> Result<(Uuid, ChatRoom, String), Value> {
        let Some(session) = &self.session else {
            return Err(error("not authenticated"));
        };
        match channels::chat_room(&self.state, room, Some(session.user_id)).await {
            Ok((chat_room, name)) => Ok((session.user_id, chat_room, name)),
            Err(AppError::BadRequest(message)) => Err(error(message)),
            Err(e) => Err(internal(e)),
        }
    }

    /// Вход или продление. Ошибка не меняет состояние соединения.
    async fn auth(&mut self, token: &str) -> Value {
        let claims = match self.state.jwt.verify(token) {
            Ok(claims) if claims.exp > Utc::now().timestamp() => claims,
            _ => return error("invalid or expired token"),
        };
        // Тот же пользователь — только новый срок, подписки остаются.
        if let Some(session) = self.session.as_mut().filter(|s| s.user_id == claims.sub) {
            session.expires_at = claims.exp;
            session.warned = false;
            return self.authenticated();
        }
        let Some(slot) = self.registry.acquire(claims.sub, MAX_CONNECTIONS_PER_USER) else {
            return error(format!(
                "no more than {MAX_CONNECTIONS_PER_USER} connections per user"
            ));
        };
        let interests = match channels::interests(&self.state, claims.sub).await {
            Ok(interests) => interests,
            Err(e) => return internal(e),
        };
        self.session = Some(Session {
            user_id: claims.sub,
            expires_at: claims.exp,
            warned: false,
            _slot: slot,
        });
        self.interests = interests.into_iter().collect();
        self.authenticated()
    }

    fn authenticated(&self) -> Value {
        let Some(session) = &self.session else {
            return error("not authenticated");
        };
        json!({
            "type": "authenticated",
            "user_id": session.user_id,
            "expires_at": rfc3339(session.expires_at),
            "channels": self.personal_channels(),
        })
    }

    /// `user:me` и каналы интересов по алфавиту.
    fn personal_channels(&self) -> Vec<String> {
        let mut names: Vec<String> = self.interests.values().cloned().collect();
        names.sort();
        names.insert(0, USER_ME.to_string());
        names
    }

    /// Подписаться на все каналы или ни на один (при любой ошибке).
    async fn subscribe(&mut self, names: &[String]) -> Value {
        let mut parsed = Vec::with_capacity(names.len());
        for name in names {
            let viewer = self.session.as_ref().map(|s| s.user_id);
            match channels::parse(&self.state, name, viewer).await {
                Ok(channel) => parsed.push(channel),
                Err(AppError::BadRequest(message)) => return error(message),
                Err(e) => return internal(e),
            }
        }
        let new: HashSet<Channel> = parsed
            .iter()
            .map(|(channel, _)| *channel)
            .filter(|channel| !self.manual.contains_key(channel))
            .collect();
        if self.manual.len() + new.len() > MAX_CHANNELS {
            return error(format!(
                "no more than {MAX_CHANNELS} channels per connection"
            ));
        }
        let names: Vec<String> = parsed.iter().map(|(_, name)| name.clone()).collect();
        self.manual.extend(parsed);
        json!({ "type": "subscribed", "channels": names })
    }

    /// Отписаться от каналов, подписанных вручную. Каналы интересов меняются через API интересов.
    fn unsubscribe(&mut self, names: Vec<String>) -> Value {
        for name in &names {
            let thread = name
                .strip_prefix("thread:")
                .and_then(|id| id.parse::<Uuid>().ok());
            match thread {
                Some(id) => {
                    self.manual.remove(&Channel::Thread(id));
                }
                None => self.manual.retain(|_, subscribed| subscribed != name),
            }
        }
        json!({ "type": "unsubscribed", "channels": names })
    }

    /// Событие шины → сообщение клиенту, если он подписан хотя бы на один из каналов события.
    async fn deliver(&mut self, event: &Event) -> Vec<Value> {
        let mut names: Vec<String> = Vec::new();
        let mut personal = false;
        for channel in &event.channels {
            let name = match channel {
                Channel::User(id) if self.session.as_ref().is_some_and(|s| s.user_id == *id) => {
                    personal = true;
                    Some(USER_ME.to_string())
                }
                _ => self
                    .manual
                    .get(channel)
                    .or_else(|| self.interests.get(channel))
                    .cloned(),
            };
            if let Some(name) = name.filter(|name| !names.contains(name)) {
                names.push(name);
            }
        }
        if names.is_empty() {
            return Vec::new();
        }
        if personal {
            if let Err(e) = self.follow_interests(event).await {
                return vec![internal(e)];
            }
        }
        vec![json!({
            "type": "event",
            "event": event.kind,
            "channels": names,
            "data": event.data,
        })]
    }

    /// Интерес добавлен или убран (в том числе с другого устройства) — меняем подписки.
    /// В данных — `entity_id`, `person_id`, `tag_id` или `room_id` (вступил в свою комнату / вышел).
    async fn follow_interests(&mut self, event: &Event) -> Result<(), AppError> {
        let id_of = |key: &str| {
            event
                .data
                .get(key)
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<Uuid>().ok())
        };
        let added = match event.kind {
            "interest.added" => true,
            "interest.removed" => false,
            _ => return Ok(()),
        };
        let found = if let Some(id) = id_of("entity_id") {
            Some((Channel::Entity(id), id))
        } else if let Some(id) = id_of("person_id") {
            Some((Channel::Person(id), id))
        } else if let Some(id) = id_of("tag_id") {
            Some((Channel::Tag(id), id))
        } else {
            id_of("room_id").map(|id| (Channel::Room(id), id))
        };
        let Some((channel, id)) = found else {
            return Ok(());
        };
        if !added {
            self.interests.remove(&channel);
            return Ok(());
        }
        let named = match channel {
            Channel::Entity(_) => channels::entities(&self.state, &[id]).await?,
            Channel::Person(_) => channels::people(&self.state, &[id]).await?,
            Channel::Tag(_) => channels::tags(&self.state, &[id]).await?,
            _ => channels::rooms(&self.state, &[id]).await?,
        };
        self.interests.extend(named);
        Ok(())
    }

    /// Когда сработает таймер токена: предупреждение, потом истечение.
    fn deadline(&self) -> Option<Instant> {
        let session = self.session.as_ref()?;
        let warning = i64::try_from(EXPIRY_WARNING.as_secs()).unwrap_or(0);
        let at = if session.warned {
            session.expires_at
        } else {
            session.expires_at - warning
        };
        Some(instant_at(at))
    }

    fn on_deadline(&mut self) -> Vec<Value> {
        let Some(session) = self.session.as_mut() else {
            return Vec::new();
        };
        if !session.warned {
            session.warned = true;
            let expires_at = rfc3339(session.expires_at);
            return vec![json!({ "type": "auth_expiring", "expires_at": expires_at })];
        }
        let channels = self.personal_channels();
        self.session = None;
        self.interests.clear();
        vec![json!({ "type": "auth_expired", "channels": channels })]
    }
}
