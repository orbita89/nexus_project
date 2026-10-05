//! social — подписки, рецензии, оценки, коллекции.
//! Монтируется под `/api/social`. Пока роутов нет.

use axum::Router;
use shared::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
}
