//! Общий код модулей Nexus: конфигурация, подключение к БД, состояние приложения,
//! проверка доступа (JWT, роли), единый тип ошибки.

pub mod auth;
pub mod config;
pub mod db;
pub mod directory;
pub mod error;
pub mod extract;
pub mod mail;
pub mod pagination;
pub mod search;
pub mod state;
pub mod telemetry;

pub use auth::{AdminUser, AuthUser, Role};

/// Префикс всех эндпоинтов API. Несовместимые изменения пойдут в `/api/v2`.
pub const API_PREFIX: &str = "/api/v1";
pub use config::Config;
pub use error::{AppError, AppResult};
pub use state::AppState;
