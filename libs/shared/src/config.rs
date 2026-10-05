//! Конфигурация приложения из переменных окружения.
//!
//! Значения задаются в `infra/docker-compose.yml`; дефолты подобраны так, чтобы
//! приложение поднималось и при локальном `cargo run` без Docker.

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    /// Адрес, который слушает HTTP-сервер.
    pub bind_addr: String,
    pub database_url: String,
    pub redis_url: String,
    pub meili_url: String,
    pub meili_master_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            bind_addr: var_or("BIND_ADDR", "0.0.0.0:8080"),
            database_url: var_or(
                "DATABASE_URL",
                "postgres://nexus_user:nexus_password@localhost:5432/nexus_db",
            ),
            redis_url: var_or("REDIS_URL", "redis://localhost:6379"),
            meili_url: var_or("MEILI_URL", "http://localhost:7700"),
            meili_master_key: env::var("MEILI_MASTER_KEY").ok(),
        }
    }
}

fn var_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}
