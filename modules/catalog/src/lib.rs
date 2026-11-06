//! catalog — контентное ядро: фильмы, сериалы, книги, игры (таблица `entities`), люди, теги
//! и поиск через Meilisearch; холодный архив фильмов — Tantivy (`archive`). Монтируется под `/api/v1/catalog`.
//!
//! Чтение — без авторизации, запись — `/admin/...`, только роль admin.

pub mod admin;
pub mod archive;
pub mod awards;
pub mod banners;
pub mod books;
pub mod directory;
pub mod entities;
pub mod jobs;
pub mod media;
pub mod metadata;
pub mod models;
pub mod people;
pub mod people_search;
pub mod publish;
pub mod ranking;
pub mod search;
pub mod studios;
pub mod tags;
pub mod tmdb;
mod validate;

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(entities::list))
        .routes(routes!(entities::get))
        .routes(routes!(entities::similar))
        .routes(routes!(search::neighbors))
        .routes(routes!(people::list))
        .routes(routes!(people::get))
        .routes(routes!(studios::get))
        .routes(routes!(awards::list))
        .routes(routes!(awards::get))
        .routes(routes!(awards::edition))
        .routes(routes!(tags::list))
        .routes(routes!(tags::get))
        .routes(routes!(search::search))
        .routes(routes!(people_search::search))
        .routes(routes!(search::studios))
        .routes(routes!(search::publishers))
        .routes(routes!(search::tags))
        .routes(routes!(search::museums))
        .routes(routes!(search::countries))
        .routes(routes!(archive::search))
        .routes(routes!(archive::stats))
        .routes(routes!(archive::get))
        .routes(routes!(archive::restore))
        .routes(routes!(admin::entities::list, admin::entities::create))
        .routes(routes!(admin::entities::update, admin::entities::delete))
        .routes(routes!(admin::entities::update_stream))
        .routes(routes!(admin::entities::set_tags))
        .routes(routes!(admin::entities::add_credit))
        .routes(routes!(admin::entities::delete_credit))
        .routes(routes!(admin::relations::put))
        .routes(routes!(admin::relations::delete))
        .routes(routes!(admin::people::create))
        .routes(routes!(admin::people::update, admin::people::delete))
        .routes(routes!(admin::tags::create))
        .routes(routes!(admin::tags::update, admin::tags::delete))
        .routes(routes!(search::index_entities))
        .routes(routes!(search::reindex_handler))
        .routes(routes!(search::reindex_stream))
        .routes(routes!(tmdb::stats_stream))
        .routes(routes!(tmdb::active_stream))
        .routes(routes!(ranking::get))
        .routes(routes!(ranking::preview))
        .routes(routes!(ranking::save_stream))
        .routes(routes!(archive::promote))
        .routes(routes!(archive::demote))
        .routes(routes!(banners::list))
        .routes(routes!(admin::banners::list, admin::banners::create))
        .routes(routes!(admin::banners::update, admin::banners::delete))
        .routes(routes!(admin::awards::create))
        .routes(routes!(admin::awards::update, admin::awards::delete))
        .routes(routes!(admin::awards::create_edition))
        .routes(routes!(
            admin::awards::update_edition,
            admin::awards::delete_edition
        ))
        .routes(routes!(admin::awards::create_category))
        .routes(routes!(
            admin::awards::update_category,
            admin::awards::delete_category
        ))
        .routes(routes!(
            admin::awards::put_nominations,
            admin::awards::create_nomination
        ))
        .routes(routes!(admin::awards::delete_nomination))
}
