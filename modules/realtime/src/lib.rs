//! realtime — WebSocket: живые уведомления, присутствие, чат.
//! Пока заглушка: echo-сокет на `/ws`, чтобы проверить проксирование в nginx.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::{routing::get, Router};
use shared::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/ws", get(ws_handler))
}

async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(echo)
}

/// Временная заглушка: возвращает клиенту его же сообщения.
async fn echo(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        if let Message::Close(_) = msg {
            break;
        }
        if socket.send(msg).await.is_err() {
            break;
        }
    }
}
