//! social — рецензии и оценки, подписки на пользователей, коллекции, социальный профиль, форум,
//! интересы, чат по интересам, лента. После изменений публикует события в `shared::events` (см. `events`) для `realtime`.
//! Монтируется под `/api/v1/social`.
//!
//! Чтение публичного — без авторизации, запись — любой вошедший (темы форума — author и выше),
//! модерация (`/admin/...`) — admin.
//! Названия сущностей и имена пользователей social берёт не из чужих таблиц, а через
//! справочники `shared::directory` (`AppState::entities`, `AppState::users`), см. `refs`.

pub mod admin;
pub mod attachments;
pub mod chat;
pub mod collection_comments;
pub mod collection_links;
pub mod collections;
pub mod comments;
pub mod directory;
pub mod dm;
mod events;
pub mod feed;
pub mod follows;
pub mod interests;
pub mod likes;
pub mod marks;
pub mod models;
pub mod posts;
pub mod profiles;
pub mod ratings;
pub mod reactions;
mod refs;
pub mod reputation;
pub mod reviews;
pub mod rooms;
mod slug;
pub mod stats;
pub mod target_comments;
pub mod threads;
pub mod user_comments;
mod validate;
pub mod votes;

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(profiles::get))
        .routes(routes!(profiles::search))
        .routes(routes!(target_comments::list, target_comments::create))
        .routes(routes!(target_comments::delete))
        .routes(routes!(reactions::collection_summary,))
        .routes(routes!(
            reactions::put_collection,
            reactions::delete_collection
        ))
        .routes(routes!(admin::delete_comment))
        .routes(routes!(likes::like, likes::unlike))
        .routes(routes!(likes::list))
        .routes(routes!(reputation::recalc))
        .routes(routes!(stats::get))
        .routes(routes!(user_comments::list))
        .routes(routes!(stats::ratings))
        .routes(routes!(stats::marks))
        .routes(routes!(reviews::list_for_entity))
        .routes(routes!(reviews::rating))
        .routes(routes!(reviews::ratings))
        .routes(routes!(
            reviews::get_own,
            reviews::put_own,
            reviews::delete_own
        ))
        .routes(routes!(reviews::list_for_user))
        .routes(routes!(votes::put, votes::delete))
        .routes(routes!(votes::mine))
        .routes(routes!(comments::list, comments::create))
        .routes(routes!(comments::delete))
        .routes(routes!(
            reactions::get_own,
            reactions::put,
            reactions::delete
        ))
        .routes(routes!(marks::get_own, marks::put, marks::delete))
        .routes(routes!(marks::list_own))
        .routes(routes!(follows::follow, follows::unfollow))
        .routes(routes!(follows::followers))
        .routes(routes!(follows::following))
        .routes(routes!(collections::list_public, collections::create))
        .routes(routes!(
            collections::get,
            collections::update,
            collections::delete
        ))
        .routes(routes!(collections::list_featured))
        .routes(routes!(collections::get_by_slug))
        .routes(routes!(
            collection_comments::list,
            collection_comments::create
        ))
        .routes(routes!(collection_comments::delete))
        .routes(routes!(collection_links::list))
        .routes(routes!(collection_links::set))
        .routes(routes!(admin::search_collections))
        .routes(routes!(collection_links::add, collection_links::remove))
        .routes(routes!(admin::delete_collection_comment))
        .routes(routes!(collections::list_for_user))
        .routes(routes!(collections::list_for_entity))
        .routes(routes!(collections::reorder))
        .routes(routes!(collections::put_item, collections::delete_item))
        .routes(routes!(feed::get))
        .routes(routes!(interests::list_own))
        .routes(routes!(interests::list_own_people))
        .routes(routes!(interests::list_own_tags))
        .routes(routes!(
            interests::get_own_person,
            interests::add_person,
            interests::remove_person
        ))
        .routes(routes!(
            interests::get_own_tag,
            interests::add_tag,
            interests::remove_tag
        ))
        .routes(routes!(chat::list_own))
        .routes(routes!(chat::search))
        .routes(routes!(attachments::get))
        // Лимит тела — только на загрузку: `layer` на общей цепочке поднял бы его всем
        // маршрутам выше. Чуть больше своего предела, чтобы на превышении отвечала наша
        // ошибка с понятным кодом, а не 413 от фреймворка.
        .merge(
            OpenApiRouter::new()
                .routes(routes!(attachments::upload))
                .layer(axum::extract::DefaultBodyLimit::max(
                    attachments::MAX_UPLOAD_BYTES + 64 * 1024,
                )),
        )
        .routes(routes!(dm::open))
        .routes(routes!(dm::get, dm::delete))
        .routes(routes!(dm::block, dm::unblock))
        .routes(routes!(dm::list_blocked))
        .routes(routes!(dm::settings, dm::update_settings))
        .routes(routes!(chat::info))
        .routes(routes!(chat::history))
        .routes(routes!(chat::get, chat::update, chat::delete))
        .routes(routes!(chat::mute, chat::unmute))
        .routes(routes!(rooms::list, rooms::create_own))
        .routes(routes!(rooms::get, rooms::update_own, rooms::delete_own))
        .routes(routes!(rooms::join, rooms::leave))
        .routes(routes!(rooms::hide, rooms::unhide))
        .routes(routes!(rooms::report))
        .routes(routes!(rooms::list_reported))
        .routes(routes!(rooms::create))
        .routes(routes!(rooms::update, rooms::delete))
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
        .routes(routes!(admin::delete_review_comment))
        .routes(routes!(admin::update_collection, admin::delete_collection))
        .routes(routes!(
            admin::feature_collection,
            admin::unfeature_collection
        ))
        .routes(routes!(
            admin::put_collection_item,
            admin::delete_collection_item
        ))
        .routes(routes!(admin::delete_thread))
        .routes(routes!(admin::delete_post))
        .routes(routes!(admin::lock_thread, admin::unlock_thread))
}
