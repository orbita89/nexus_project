//! catalog — контентное ядро: фильмы, сериалы, книги, игры (таблица `entities`), люди, теги
//! и поиск на Tantivy (`works`, `people_idx`, `books`); холодный архив фильмов — Tantivy (`archive`). Монтируется под `/api/v1/catalog`.
//!
//! Чтение — без авторизации, запись — `/admin/...`, только роль admin.

pub mod admin;
pub mod ai;
pub mod archive;
pub mod awards;
pub mod banners;
pub mod books;
pub mod directory;
pub mod entities;
pub mod fragments;
pub mod jobs;
pub mod media;
pub mod metadata;
pub mod models;
pub mod people;
pub mod people_idx;
pub mod people_search;
pub mod publish;
pub mod ranking;
pub mod scripts;
pub mod search;
pub mod seasons;
pub mod studios;
pub mod tags;
pub mod tmdb;
pub mod tutor;
mod validate;
pub mod works;

use shared::AppState;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(entities::list))
        .routes(routes!(entities::get))
        .routes(routes!(entities::similar))
        .routes(routes!(fragments::list))
        .routes(routes!(seasons::list))
        .routes(routes!(seasons::get))
        .routes(routes!(seasons::episodes))
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
        .routes(routes!(admin::episodes::put))
        .routes(routes!(admin::fragments::put))
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
        .routes(routes!(books::reader::epub))
        .routes(routes!(admin::books::list, admin::books::put))
        .routes(routes!(admin::books::reload_sidecar))
        .routes(routes!(admin::books::index_status))
        .routes(routes!(admin::books::rebuild_index))
        .routes(routes!(admin::books::merge_index))
        .routes(routes!(admin::books::clear_cache))
        .routes(routes!(admin::indexes::list))
        .routes(routes!(admin::indexes::rebuild))
        .routes(routes!(admin::indexes::merge))
        .routes(routes!(admin::static_pages::regenerate))
        .routes(routes!(admin::static_pages::regenerate_pages))
        .routes(routes!(admin::static_pages::status))
        .routes(routes!(tutor::providers::list, tutor::providers::create))
        .routes(routes!(tutor::providers::update, tutor::providers::delete))
        .routes(routes!(tutor::providers::test))
        .routes(routes!(tutor::threads::list_book, tutor::threads::create))
        .routes(routes!(tutor::threads::list_own))
        .routes(routes!(tutor::threads::get, tutor::threads::delete))
        .routes(routes!(tutor::threads::send))
        .routes(routes!(admin::ai::list, admin::ai::create))
        .routes(routes!(admin::ai::update, admin::ai::delete))
        .routes(routes!(admin::ai::test))
        .routes(routes!(admin::prompts::list))
        .routes(routes!(admin::prompts::put, admin::prompts::delete))
        .routes(routes!(admin::prompts::preview))
        .routes(routes!(admin::scripts::list))
        .routes(routes!(admin::scripts::run))
        .routes(routes!(admin::scripts::report))
        .routes(routes!(admin::scripts::progress))
        .routes(routes!(admin::scripts::jobs))
        .routes(routes!(admin::scripts::job))
        .routes(routes!(admin::scripts::cancel))
        .merge(
            OpenApiRouter::new()
                .routes(routes!(admin::covers::upload))
                .layer(axum::extract::DefaultBodyLimit::max(
                    admin::covers::MAX_COVER_BYTES + 64 * 1024,
                )),
        )
}
