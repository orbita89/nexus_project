//! Клиент Meilisearch: тонкая обёртка над HTTP API, без SDK.
//!
//! Что и как индексировать, решает модуль (сейчас только `catalog`); здесь только транспорт,
//! ожидание асинхронных задач Meilisearch и префикс имён индексов (у каждого теста свой).

use axum::http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Сколько ждать один HTTP-запрос к Meilisearch.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько ждать завершения задачи (индексация, настройки, swap).
const TASK_TIMEOUT: Duration = Duration::from_secs(60);
const TASK_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("search is disabled")]
    Disabled,
    #[error("meilisearch request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Ответ Meilisearch с ошибкой: `code` — машинный код (`index_not_found`, ...).
    #[error("meilisearch error {status} {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("meilisearch task {uid} {status}: {error}")]
    Task {
        uid: u64,
        status: String,
        error: Value,
    },
}

impl SearchError {
    /// Код ошибки Meilisearch (`index_not_found`, `index_already_exists`, ...).
    pub fn code(&self) -> Option<&str> {
        match self {
            SearchError::Api { code, .. } => Some(code),
            SearchError::Task { error, .. } => error["code"].as_str(),
            _ => None,
        }
    }
}

/// Хэндл клиента: дешёво клонируется. Выключенный (`disabled`) отвечает [`SearchError::Disabled`].
#[derive(Clone)]
pub struct Search {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    http: reqwest::Client,
    url: String,
    key: Option<String>,
    index_prefix: String,
}

impl Search {
    pub fn new(url: &str, key: Option<String>, index_prefix: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with default TLS settings");
        Self {
            inner: Some(Arc::new(Inner {
                http,
                url: url.trim_end_matches('/').to_string(),
                key: key.filter(|key| !key.is_empty()),
                index_prefix: index_prefix.to_string(),
            })),
        }
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Полное имя индекса: префикс + имя (`entities` → `test_1f2e..._entities`).
    pub fn index(&self, name: &str) -> String {
        match &self.inner {
            Some(inner) => format!("{}{name}", inner.index_prefix),
            None => name.to_string(),
        }
    }

    /// Запрос к API. `path` — от корня: `/indexes/entities/search`.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, SearchError> {
        let inner = self.inner.as_ref().ok_or(SearchError::Disabled)?;
        let mut request = inner.http.request(method, format!("{}{path}", inner.url));
        if let Some(key) = &inner.key {
            request = request.bearer_auth(key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        let json: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if !status.is_success() {
            return Err(SearchError::Api {
                status: status.as_u16(),
                code: json["code"].as_str().unwrap_or("unknown").to_string(),
                message: json["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        Ok(json)
    }

    /// Запрос, который ставит задачу в очередь Meilisearch, и ожидание её завершения.
    pub async fn call_and_wait(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(), SearchError> {
        let task = self.call(method, path, body).await?;
        self.wait(&task).await
    }

    /// Ждёт задачу из ответа Meilisearch (`{"taskUid": 42, ...}`). Ошибка, если задача упала.
    pub async fn wait(&self, task: &Value) -> Result<(), SearchError> {
        let uid = task["taskUid"].as_u64().ok_or_else(|| SearchError::Api {
            status: 0,
            code: "no_task_uid".into(),
            message: task.to_string(),
        })?;
        let deadline = tokio::time::Instant::now() + TASK_TIMEOUT;
        loop {
            let task = self
                .call(Method::GET, &format!("/tasks/{uid}"), None)
                .await?;
            let status = task["status"].as_str().unwrap_or_default();
            match status {
                "succeeded" => return Ok(()),
                "failed" | "canceled" => {
                    return Err(SearchError::Task {
                        uid,
                        status: status.to_string(),
                        error: task["error"].clone(),
                    })
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(SearchError::Task {
                        uid,
                        status: "timeout".into(),
                        error: Value::Null,
                    })
                }
                _ => tokio::time::sleep(TASK_POLL_INTERVAL).await,
            }
        }
    }
}
