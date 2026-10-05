//! Хелперы для тестов: подключаются только как dev-dependency.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use shared::{AppState, Config};
use sqlx::PgPool;
use tower::ServiceExt;

/// Состояние приложения поверх тестовой БД (например, из `#[sqlx::test]`).
pub fn state(pool: PgPool) -> AppState {
    AppState::new(Config::from_env(), pool)
}

/// Пул, который никогда не подключается. Для тестов, которым БД не нужна.
pub fn state_without_db() -> AppState {
    let pool = PgPool::connect_lazy("postgres://unused@localhost/unused")
        .expect("lazy pool from a valid URL");
    state(pool)
}

/// Ответ, прочитанный целиком.
pub struct TestResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
}

impl TestResponse {
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
    let body = response
        .into_body()
        .collect()
        .await
        .expect("read response body")
        .to_bytes()
        .to_vec();
    TestResponse { status, body }
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
