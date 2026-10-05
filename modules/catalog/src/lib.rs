//! catalog — контентное ядро: фильмы, сериалы, книги, игры (таблица `entities`)
//! и их индексация в Meilisearch. Монтируется под `/api/catalog`. Пока роутов нет.

use axum::Router;
use shared::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
}
