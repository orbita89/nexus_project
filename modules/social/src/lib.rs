//! social — рецензии и оценки, подписки на пользователей, коллекции, социальный профиль, форум,
//! интересы, лента. После изменений публикует события в `shared::events` (см. `events`) для `realtime`.
//! Монтируется под `/api/v1/social`.
//!
//! Чтение публичного — без авторизации, запись — любой вошедший (темы форума — author и выше),
//! модерация (`/admin/...`) — admin.
//! Названия сущностей и имена пользователей social берёт не из чужих таблиц, а через
//! справочники `shared::directory` (`AppState::entities`, `AppState::users`), см. `refs`.

pub mod admin;
pub mod collections;
pub mod directory;
mod events;
pub mod feed;
pub mod follows;
pub mod interests;
pub mod models;
pub mod posts;
pub mod profiles;
mod refs;
pub mod reviews;
pub mod threads;
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
        .routes(routes!(feed::get))
        .routes(routes!(interests::list_own))
        .routes(routes!(
            interests::get_own,
            interests::add,
            interests::remove
        ))
        .routes(routes!(threads::list, threads::create))
        .routes(routes!(threads::get, threads::update, threads::delete))
        .routes(routes!(threads::list_for_entity))
        .routes(routes!(threads::list_for_user))
        .routes(routes!(posts::create))
        .routes(routes!(posts::update, posts::delete))
        .routes(routes!(admin::delete_review))
        .routes(routes!(admin::delete_collection))
        .routes(routes!(admin::delete_thread))
        .routes(routes!(admin::delete_post))
        .routes(routes!(admin::lock_thread, admin::unlock_thread))
}
