//! Одно WebSocket-соединение: сообщения клиента, события шины, heartbeat и срок токена.
//!
//! Соединение открывается гостем: можно подписаться на публичные каналы (`thread:`, `entity:`).
//! `auth` с access-токеном добавляет личный канал `user:me` и каналы сущностей из интересов.
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
    /// Каналы сущностей из интересов вошедшего.
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
        let interests = match self.state.interests.entity_ids(claims.sub).await {
            Ok(ids) => ids,
            Err(e) => return internal(e),
        };
        let interests = match channels::entities(&self.state, &interests).await {
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
            match channels::parse(&self.state, name).await {
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
    async fn follow_interests(&mut self, event: &Event) -> Result<(), AppError> {
        let entity = event
            .data
            .get("entity_id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<Uuid>().ok());
        match (event.kind, entity) {
            ("interest.added", Some(id)) => {
                self.interests
                    .extend(channels::entities(&self.state, &[id]).await?);
            }
            ("interest.removed", Some(id)) => {
                self.interests.remove(&Channel::Entity(id));
            }
            _ => {}
        }
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
