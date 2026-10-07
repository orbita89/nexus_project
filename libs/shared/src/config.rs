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
    /// Кэш карточек каталога ([`crate::cache`]): L1 в памяти процесса, `CACHE_L1_TTL_SECS`.
    pub cache_l1_ttl_secs: u64,
    /// Сколько записей держит L1, `CACHE_L1_CAPACITY`.
    pub cache_l1_capacity: u64,
    /// L2 в Redis, `CACHE_L2_TTL_SECS`.
    pub cache_l2_ttl_secs: u64,
    /// Полная перестройка поискового индекса по расписанию, `SEARCH_REINDEX_INTERVAL_SECS`.
    /// `0` — только при старте и вручную.
    pub search_reindex_interval_secs: u64,
    /// Секрет подписи JWT (HS256). Не короче [`MIN_JWT_SECRET_LEN`] байт.
    pub jwt_secret: String,
    /// SMTP для писем, например `smtp://mailpit:1025` или `smtps://user:pass@smtp.example.com`.
    /// Не задан — письма пишутся в лог.
    pub smtp_url: Option<String>,
    /// Отправитель писем.
    pub mail_from: String,
    /// Адрес фронтенда: из него строятся ссылки в письмах.
    pub app_base_url: String,
    /// Swagger UI на `/docs` и схема на `/api-docs/openapi.json`. `API_DOCS=false` — выключить.
    pub api_docs: bool,
    /// Публичный адрес API (как его видит браузер): из него строится redirect_uri для OAuth.
    pub public_url: String,
    /// `POST /api/v1/auth/dev/login` — вход под любым пользователем без пароля.
    /// Только для разработки и тестирования: `DEV_LOGIN=true`. В проде не включать.
    pub dev_login: bool,
    /// OAuth-провайдеры, для которых заданы ключи в env (см. [`OAuthProviderConfig::presets`]).
    pub oauth_providers: Vec<OAuthProviderConfig>,
}

/// Как получить профиль пользователя у провайдера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthKind {
    /// OpenID Connect userinfo: `sub`, `email`, `email_verified`, `name`, `picture` (Google и др.).
    Oidc,
    /// GitHub API: профиль `/user` и подтверждённые адреса `/user/emails`.
    Github,
    /// Яндекс ID: `login.yandex.ru/info`, заголовок `Authorization: OAuth <token>`.
    Yandex,
}

#[derive(Debug, Clone)]
pub struct OAuthProviderConfig {
    /// Имя в URL: `/api/v1/auth/oauth/{name}/start`.
    pub name: String,
    pub kind: OAuthKind,
    pub client_id: String,
    pub client_secret: String,
    pub authorize_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    /// Через пробел.
    pub scopes: String,
}

impl OAuthProviderConfig {
    /// Известные провайдеры. Включается тот, для которого заданы
    /// `OAUTH_<NAME>_CLIENT_ID` и `OAUTH_<NAME>_CLIENT_SECRET`.
    pub fn presets() -> [(
        &'static str,
        OAuthKind,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ); 3] {
        [
            (
                "google",
                OAuthKind::Oidc,
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                "https://openidconnect.googleapis.com/v1/userinfo",
                "openid email profile",
            ),
            (
                "github",
                OAuthKind::Github,
                "https://github.com/login/oauth/authorize",
                "https://github.com/login/oauth/access_token",
                "https://api.github.com/user",
                "read:user user:email",
            ),
            (
                "yandex",
                OAuthKind::Yandex,
                "https://oauth.yandex.ru/authorize",
                "https://oauth.yandex.ru/token",
                "https://login.yandex.ru/info?format=json",
                "login:email login:info login:avatar",
            ),
        ]
    }

    fn from_env() -> Vec<Self> {
        Self::presets()
            .into_iter()
            .filter_map(
                |(name, kind, authorize_url, token_url, userinfo_url, scopes)| {
                    let key = name.to_uppercase();
                    let client_id = env::var(format!("OAUTH_{key}_CLIENT_ID")).ok()?;
                    let client_secret = env::var(format!("OAUTH_{key}_CLIENT_SECRET")).ok()?;
                    if client_id.is_empty() || client_secret.is_empty() {
                        return None;
                    }
                    Some(Self {
                        name: name.to_string(),
                        kind,
                        client_id,
                        client_secret,
                        authorize_url: authorize_url.to_string(),
                        token_url: token_url.to_string(),
                        userinfo_url: userinfo_url.to_string(),
                        scopes: scopes.to_string(),
                    })
                },
            )
            .collect()
    }
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
            cache_l1_ttl_secs: number_or("CACHE_L1_TTL_SECS", 30),
            cache_l1_capacity: number_or("CACHE_L1_CAPACITY", 10_000),
            cache_l2_ttl_secs: number_or("CACHE_L2_TTL_SECS", 86_400),
            search_reindex_interval_secs: number_or("SEARCH_REINDEX_INTERVAL_SECS", 86_400),
            jwt_secret: jwt_secret_from_env(),
            smtp_url: env::var("SMTP_URL").ok().filter(|url| !url.is_empty()),
            mail_from: var_or("MAIL_FROM", "Nexus <no-reply@nexus.local>"),
            app_base_url: var_or("APP_BASE_URL", "http://localhost")
                .trim_end_matches('/')
                .to_string(),
            api_docs: var_or("API_DOCS", "true") != "false",
            public_url: var_or("PUBLIC_URL", "http://localhost")
                .trim_end_matches('/')
                .to_string(),
            dev_login: var_or("DEV_LOGIN", "false") == "true",
            oauth_providers: OAuthProviderConfig::from_env(),
        }
    }

    pub fn uses_dev_jwt_secret(&self) -> bool {
        self.jwt_secret == DEV_JWT_SECRET
    }
}

fn var_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Число из env. Неверное значение — не стартовать, а не молча взять дефолт.
fn number_or(key: &str, default: u64) -> u64 {
    match env::var(key) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{key} must be a non-negative integer, got {value:?}")),
        Err(_) => default,
    }
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
