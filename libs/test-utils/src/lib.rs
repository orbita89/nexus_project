//! Хелперы для тестов: подключаются только как dev-dependency.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use shared::cache::{Cache, CacheSettings};
use shared::directory::Directories;
use shared::isr::Isr;
use shared::mail::{Mailer, Outbox};
use shared::{AppState, Config};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

/// Состояние приложения поверх тестовой БД (например, из `#[sqlx::test]`).
/// Письма не отправляются, а складываются в возвращаемый [`Outbox`].
pub fn state(pool: PgPool) -> (AppState, Outbox) {
    state_with_config(pool, Config::from_env())
}

/// То же, но с изменённой конфигурацией (включить dev login, добавить OAuth-провайдера, ...).
/// Индексы поиска (`SEARCH_INDEX_DIR`) и ревалидация статики выключены: тесты не пишут на диск и
/// не ходят во фронтенд. Включить поиск — [`with_index`].
pub fn state_with_config(pool: PgPool, config: Config) -> (AppState, Outbox) {
    let (mailer, outbox) = Mailer::memory();
    let directories = directories(pool.clone());
    let mut state = AppState::new(config, pool, mailer, directories);
    state.isr = Isr::disabled();
    (state, outbox)
}

/// Настоящие справочники, как в `nexus::directories`. Подменить в тесте:
/// `state.entities = Arc::new(Fake)`.
pub fn directories(db: PgPool) -> Directories {
    Directories {
        entities: Arc::new(catalog::directory::PgEntityDirectory::new(db.clone())),
        users: Arc::new(auth::directory::PgUserDirectory::new(db.clone())),
        interests: Arc::new(social::directory::PgInterestDirectory::new(db.clone())),
        ratings: Arc::new(catalog::directory::PgEntityRatings::new(db.clone())),
        chat: Arc::new(social::chat::PgChat::default()),
        people: Arc::new(catalog::directory::PgPersonDirectory::new(db.clone())),
        tags: Arc::new(catalog::directory::PgTagDirectory::new(db.clone())),
        awards: Arc::new(catalog::directory::PgAwardDirectory::new(db.clone())),
        studios: Arc::new(catalog::directory::PgStudioDirectory::new(db.clone())),
        person_likes: Arc::new(social::directory::PgPersonLikes::new(db.clone())),
        user_ratings: Arc::new(social::directory::PgUserRatings::new(db.clone())),
    }
}

/// Каталог индексов Tantivy теста; удаляется при drop.
pub struct IndexDir(pub std::path::PathBuf);

impl Drop for IndexDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Включает поиск: `SEARCH_INDEX_DIR` во временном каталоге (свой у каждого теста, чтобы
/// параллельные тесты не мешали друг другу). Индексы строятся вручную:
/// `catalog::index::sync::rebuild`.
pub fn with_index(mut state: AppState) -> (AppState, IndexDir) {
    let dir = std::env::temp_dir().join(format!("nexus-test-{}", uuid::Uuid::new_v4()));
    let mut config = (*state.config).clone();
    config.search_index_dir = Some(dir.to_string_lossy().into_owned());
    state.config = Arc::new(config);
    (state, IndexDir(dir))
}

/// Включает кэш (в памяти процесса) со своим префиксом ключей.
pub fn with_cache(mut state: AppState) -> AppState {
    let mut settings = CacheSettings::from_config(&state.config);
    settings.prefix = format!("test_{}:", uuid::Uuid::new_v4().simple());
    // Повторное удаление при инвалидации не должно задерживать тесты.
    settings.redelete_after = Duration::from_millis(20);
    state.cache = Cache::local(settings);
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

/// Загрузка файла (`multipart/form-data`) с одним файловым полем — для вложений чата.
pub async fn upload(
    app: Router,
    uri: &str,
    filename: &str,
    bytes: Vec<u8>,
    token: Option<&str>,
) -> TestResponse {
    const BOUNDARY: &str = "nexus-test-boundary";
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let mut builder = Request::builder().method(Method::POST).uri(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    send(app, builder.body(Body::from(body)).expect("valid request")).await
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
