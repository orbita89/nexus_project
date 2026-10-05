//! Общий код модулей Nexus: конфигурация, подключение к БД, состояние приложения,
//! единый тип ошибки.

pub mod config;
pub mod db;
pub mod error;
pub mod state;
pub mod telemetry;

pub use config::Config;
pub use error::{AppError, AppResult};
pub use state::AppState;
