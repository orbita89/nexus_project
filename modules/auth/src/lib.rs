//! auth — регистрация и вход (по паролю, по ссылке из письма, через OAuth-провайдеров),
//! токены, сессии, роли.
//! Монтируется в приложении под `/api/v1/auth`.
//!
//! Схема токенов:
//! - access — JWT на 15 минут (`shared::auth`), передаётся в `Authorization: Bearer`;
//! - refresh — случайная строка на 30 дней, в БД хранится только её хеш (`refresh_tokens`).
//!   При каждом обновлении выдаётся новая, старая отзывается.
//!
//! Подробно — `documents/modules/auth.md`.

mod admin;
pub mod cleanup;
pub mod crypto;
mod email_tokens;
mod emails;
mod handlers;
mod models;
mod oauth;
mod rate_limit;
mod session;
mod users;
mod validate;

use axum::Extension;
use handlers::{account, dev, email, password, sessions};
use oauth::OAuthHttp;
use rate_limit::RateLimits;
use shared::AppState;
use std::sync::Arc;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(account::register))
        .routes(routes!(account::login))
        .routes(routes!(account::refresh))
        .routes(routes!(account::logout))
        .routes(routes!(account::me))
        .routes(routes!(email::verify_email))
        .routes(routes!(email::resend_verification))
        .routes(routes!(email::email_login_start))
        .routes(routes!(email::email_login_confirm))
        .routes(routes!(password::forgot))
        .routes(routes!(password::reset))
        .routes(routes!(password::change))
        .routes(routes!(sessions::list))
        .routes(routes!(sessions::revoke))
        .routes(routes!(sessions::logout_all))
        .routes(routes!(oauth::providers))
        .routes(routes!(oauth::start))
        .routes(routes!(oauth::callback))
        .routes(routes!(oauth::exchange))
        .routes(routes!(dev::login))
        .routes(routes!(admin::list_users))
        .routes(routes!(admin::set_role))
        .routes(routes!(admin::set_status))
        .layer(Extension(Arc::new(RateLimits::default())))
        .layer(Extension(OAuthHttp::default()))
}
