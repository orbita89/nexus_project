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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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
/// Одно соединение на пользователя: новый `auth` того же пользователя закрывает прежнее
/// соединение кодом [`CLOSE_REPLACED`] (зависшая вкладка не блокирует свежую). Гостевые не
/// ограничены (фронт гостям сокет не открывает: чат у гостя — только чтение по REST).
pub const MAX_CONNECTIONS_PER_USER: usize = 1;
/// Код закрытия «соединение заменено новым того же пользователя»; клиент не переподключается.
pub const CLOSE_REPLACED: u16 = 4000;
/// Максимальный размер сообщения от клиента (и кадра).
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024;
/// Буферы сокета. По умолчанию в axum — по 128 КиБ на чтение и запись у КАЖДОГО соединения
/// (тысяча клиентов — ~256 МБ); сообщения чата маленькие, хватает нескольких КиБ.
pub const READ_BUFFER_SIZE: usize = 4 * 1024;
pub const WRITE_BUFFER_SIZE: usize = 4 * 1024;
/// Предел неотправленного в буфере записи: дальше запись — ошибка, соединение закрывается.
pub const MAX_WRITE_BUFFER_SIZE: usize = 64 * 1024;
/// Всего соединений на процесс (гости и вошедшие) по умолчанию; сверх — 503 до апгрейда,
/// клиент переподключается позже. Переопределяется `WS_MAX_CONNECTIONS`: соединение стоит
/// ~8–10 КБ у простаивающего гостя с одним каналом (замер `scripts/ws-load.mjs`, 2026-11-28),
/// больше у вошедшего (каналы интересов): 5000 — ~50–100 МБ.
pub const MAX_CONNECTIONS: usize = 5_000;

fn max_connections() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("WS_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(MAX_CONNECTIONS)
    })
}

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
    let Some(guard) = registry.connect(max_connections()) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many connections").into_response();
    };
    ws.max_message_size(MAX_MESSAGE_SIZE)
        .max_frame_size(MAX_MESSAGE_SIZE)
        .read_buffer_size(READ_BUFFER_SIZE)
        .write_buffer_size(WRITE_BUFFER_SIZE)
        .max_write_buffer_size(MAX_WRITE_BUFFER_SIZE)
        .on_upgrade(move |socket| async move {
            connection::run(socket, state, registry).await;
            drop(guard);
        })
}
