//! auth — регистрация, вход, выдача и обновление токенов, управление ролями.
//! Монтируется в приложении под `/api/auth`.
//!
//! Схема токенов:
//! - access — JWT на 15 минут (`shared::auth`), передаётся в `Authorization: Bearer`;
//! - refresh — случайная строка на 30 дней, в БД хранится только её хеш (`refresh_tokens`).
//!   При каждом обновлении выдаётся новая, старая отзывается.

mod admin;
mod handlers;
mod models;
pub mod password;
mod session;

use axum::routing::{get, patch, post};
use axum::Router;
use shared::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(handlers::register))
        .route("/login", post(handlers::login))
        .route("/refresh", post(handlers::refresh))
        .route("/logout", post(handlers::logout))
        .route("/me", get(handlers::me))
        .route("/admin/users", get(admin::list_users))
        .route("/admin/users/{id}/role", patch(admin::set_role))
}
