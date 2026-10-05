//! auth — регистрация, вход, выдача и обновление токенов.
//! Монтируется в приложении под `/api/auth`. Пока роутов нет.

use axum::Router;
use shared::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
}
