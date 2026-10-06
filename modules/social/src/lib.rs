//! social — подписки, рецензии, оценки, коллекции.
//! Монтируется под `/api/v1/social`. Пока роутов нет.

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
}
