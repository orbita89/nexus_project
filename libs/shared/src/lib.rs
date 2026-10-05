//! Общий код модулей Nexus: конфигурация, подключение к БД, состояние приложения,
//! проверка доступа (JWT, роли), единый тип ошибки.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod state;
pub mod telemetry;

pub use auth::{AdminUser, AuthUser, Role};
pub use config::Config;
pub use error::{AppError, AppResult};
pub use state::AppState;
