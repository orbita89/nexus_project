//! catalog — контентное ядро: фильмы, сериалы, книги, игры (таблица `entities`)
//! и их индексация в Meilisearch. Монтируется под `/api/v1/catalog`. Пока роутов нет.

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
}
