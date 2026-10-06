//! realtime — живые события по WebSocket на `/ws`: новое в темах форума, по сущностям из
//! интересов, ответы на свои сообщения.
//!
//! Данные создают другие модули и публикуют события в `shared::events::EventBus`; realtime
//! только доставляет их соединениям, подписанным на канал. В чужие таблицы не ходит: slug
//! сущностей — через `state.entities`, интересы — через `state.interests`.
//!
//! Протокол (JSON-сообщения с полем `type`) — `documents/modules/realtime.md`.

mod channels;
mod connection;
mod registry;

use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use axum::{routing::get, Extension, Router};
use registry::Registry;
use shared::AppState;
use std::sync::Arc;
use std::time::Duration;

/// Как часто сервер пингует клиента.
pub const HEARTBEAT: Duration = Duration::from_secs(30);
/// Клиент молчит дольше (ни сообщений, ни pong) — соединение закрывается.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// За сколько до истечения токена сервер присылает `auth_expiring`.
pub const EXPIRY_WARNING: Duration = Duration::from_secs(60);
/// Каналов, подписанных вручную, на соединение (интересы и `user:me` не считаются).
pub const MAX_CHANNELS: usize = 200;
/// Одновременных соединений одного пользователя (гостевые не ограничены).
pub const MAX_CONNECTIONS_PER_USER: usize = 5;
/// Максимальный размер сообщения от клиента.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ws", get(ws_handler))
        .layer(Extension(Arc::new(Registry::default())))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(registry): Extension<Arc<Registry>>,
) -> Response {
    ws.max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| connection::run(socket, state, registry))
}
