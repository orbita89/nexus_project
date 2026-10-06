//! Хелперы для тестов: подключаются только как dev-dependency.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use shared::mail::{Mailer, Outbox};
use shared::search::Search;
use shared::{AppState, Config};
use sqlx::PgPool;
use tower::ServiceExt;

/// Состояние приложения поверх тестовой БД (например, из `#[sqlx::test]`).
/// Письма не отправляются, а складываются в возвращаемый [`Outbox`].
pub fn state(pool: PgPool) -> (AppState, Outbox) {
    state_with_config(pool, Config::from_env())
}

/// То же, но с изменённой конфигурацией (включить dev login, добавить OAuth-провайдера, ...).
/// Поиск выключен: тесты не пишут в Meilisearch. Включить — [`with_search`].
pub fn state_with_config(pool: PgPool, config: Config) -> (AppState, Outbox) {
    let (mailer, outbox) = Mailer::memory();
    let mut state = AppState::new(config, pool, mailer);
    state.search = Search::disabled();
    (state, outbox)
}

/// Включает Meilisearch (`MEILI_URL`, `MEILI_MASTER_KEY`) со своим префиксом индексов,
/// чтобы параллельные тесты не мешали друг другу и dev-индексу.
pub fn with_search(mut state: AppState) -> AppState {
    let prefix = format!("test_{}_", uuid::Uuid::new_v4().simple());
    state.search = Search::new(
        &state.config.meili_url,
        state.config.meili_master_key.clone(),
        &prefix,
    );
    state
}

/// Пул, который никогда не подключается. Для тестов, которым БД не нужна.
pub fn state_without_db() -> AppState {
    let pool = PgPool::connect_lazy("postgres://unused@localhost/unused")
        .expect("lazy pool from a valid URL");
    state(pool).0
}

/// Значение параметра `token=` из ссылки в письме.
pub fn token_from_email(text: &str) -> String {
    text.split("token=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("email contains a link with token=")
        .to_string()
}

/// Ответ, прочитанный целиком.
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl TestResponse {
    /// Заголовок `Location` у редиректа.
    pub fn location(&self) -> String {
        self.headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("response has Location header")
            .to_string()
    }

    /// Тело как JSON. Паникует с текстом тела, если это не JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "response body is not JSON ({e}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// Прогоняет запрос через роутер без сети.
pub async fn send(app: Router, request: Request<Body>) -> TestResponse {
    let response = app.oneshot(request).await.expect("router is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("read response body")
        .to_bytes()
        .to_vec();
    TestResponse {
        status,
        headers,
        body,
    }
}

pub async fn get(app: Router, uri: &str) -> TestResponse {
    request(app, Method::GET, uri, None, None).await
}

/// Запрос с необязательным JSON-телом и access-токеном (`Authorization: Bearer`).
pub async fn request(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
    token: Option<&str>,
) -> TestResponse {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    send(app, builder.body(body).expect("valid request")).await
}

/// Значение параметра из query-строки URL (без URL-декодирования: для токенов и кодов его не нужно).
pub fn query_param(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?
        .1
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}
