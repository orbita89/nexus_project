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
    /// Секрет подписи JWT (HS256). Не короче [`MIN_JWT_SECRET_LEN`] байт.
    pub jwt_secret: String,
}

/// Секрет для локальной разработки. В проде обязательно задать свой `JWT_SECRET`.
pub const DEV_JWT_SECRET: &str = "nexus-dev-jwt-secret-change-me-in-production";

pub const MIN_JWT_SECRET_LEN: usize = 32;

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
            jwt_secret: jwt_secret_from_env(),
        }
    }

    pub fn uses_dev_jwt_secret(&self) -> bool {
        self.jwt_secret == DEV_JWT_SECRET
    }
}

fn var_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Короткий секрет подбирается перебором — лучше не стартовать вовсе.
fn jwt_secret_from_env() -> String {
    let secret = var_or("JWT_SECRET", DEV_JWT_SECRET);
    assert!(
        secret.len() >= MIN_JWT_SECRET_LEN,
        "JWT_SECRET must be at least {MIN_JWT_SECRET_LEN} bytes"
    );
    secret
}
