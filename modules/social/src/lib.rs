//! social — рецензии и оценки, подписки на пользователей, коллекции, социальный профиль.
//! Монтируется под `/api/v1/social`.
//!
//! Чтение публичного — без авторизации, запись — любой вошедший, модерация (`/admin/...`) — admin.
//! Названия сущностей и имена пользователей social берёт не из чужих таблиц, а через
//! справочники `shared::directory` (`AppState::entities`, `AppState::users`), см. `refs`.

pub mod admin;
pub mod collections;
pub mod follows;
pub mod models;
pub mod profiles;
mod refs;
pub mod reviews;
mod validate;

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(profiles::get))
        .routes(routes!(reviews::list_for_entity))
        .routes(routes!(reviews::rating))
        .routes(routes!(
            reviews::get_own,
            reviews::put_own,
            reviews::delete_own
        ))
        .routes(routes!(reviews::list_for_user))
        .routes(routes!(follows::follow, follows::unfollow))
        .routes(routes!(follows::followers))
        .routes(routes!(follows::following))
        .routes(routes!(collections::list_public, collections::create))
        .routes(routes!(
            collections::get,
            collections::update,
            collections::delete
        ))
        .routes(routes!(collections::list_for_user))
        .routes(routes!(collections::list_for_entity))
        .routes(routes!(collections::reorder))
        .routes(routes!(collections::put_item, collections::delete_item))
        .routes(routes!(admin::delete_review))
        .routes(routes!(admin::delete_collection))
}
