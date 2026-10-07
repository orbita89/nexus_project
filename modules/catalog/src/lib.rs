//! catalog — контентное ядро: фильмы, сериалы, книги, игры (таблица `entities`), люди, теги
//! и поиск через Meilisearch. Монтируется под `/api/v1/catalog`.
//!
//! Чтение — без авторизации, запись — `/admin/...`, только роль admin.

pub mod admin;
pub mod cards;
pub mod directory;
pub mod entities;
pub mod media;
pub mod metadata;
pub mod models;
pub mod people;
pub mod search;
pub mod tags;
mod validate;

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(entities::list))
        .routes(routes!(entities::get))
        .routes(routes!(people::list))
        .routes(routes!(people::get))
        .routes(routes!(tags::list))
        .routes(routes!(search::search))
        .routes(routes!(admin::entities::create))
        .routes(routes!(admin::entities::update, admin::entities::delete))
        .routes(routes!(admin::entities::set_tags))
        .routes(routes!(admin::entities::add_credit))
        .routes(routes!(admin::entities::delete_credit))
        .routes(routes!(admin::people::create))
        .routes(routes!(admin::people::update, admin::people::delete))
        .routes(routes!(admin::tags::create))
        .routes(routes!(admin::tags::update, admin::tags::delete))
        .routes(routes!(search::reindex_handler))
}
