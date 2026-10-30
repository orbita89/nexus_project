//! Клиент Meilisearch: тонкая обёртка над HTTP API, без SDK.
//!
//! Что и как индексировать, решает модуль (`catalog` — произведения и люди, `social` —
//! коллекции); здесь транспорт, ожидание асинхронных задач Meilisearch, префикс имён индексов
//! (у каждого теста свой) и общие операции: страница выдачи с точным счётом ([`search_exact`])
//! и переотправка изменённых документов ([`upsert_and_delete`]).

use crate::AppError;
use axum::http::Method;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

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

/// Поиск с точным числом совпадений. Точный `totalHits` Meilisearch отдаёт только в режиме
/// `page` + `hitsPerPage` (`estimatedTotalHits` при `offset`/`limit` — оценка). `offset`, кратный
/// `limit`, — одна страница; иначе выдача по `offset`/`limit` и параллельно счёт с
/// `hitsPerPage: 0`. Возвращает ответ Meilisearch (`hits`) и точное число.
pub async fn search_exact(
    search: &Search,
    path: &str,
    mut body: Value,
    limit: i64,
    offset: i64,
) -> Result<(Value, i64), SearchError> {
    if offset % limit == 0 {
        body["page"] = json!(offset / limit + 1);
        body["hitsPerPage"] = json!(limit);
        let response = search.call(Method::POST, path, Some(&body)).await?;
        let total = response["totalHits"].as_i64().unwrap_or(0);
        return Ok((response, total));
    }
    let mut count = body.clone();
    count["page"] = json!(1);
    count["hitsPerPage"] = json!(0);
    count["attributesToRetrieve"] = json!([]);
    if let Some(map) = count.as_object_mut() {
        map.remove("sort");
    }
    body["offset"] = json!(offset);
    body["limit"] = json!(limit);
    let (response, count) = tokio::try_join!(
        search.call(Method::POST, path, Some(&body)),
        search.call(Method::POST, path, Some(&count)),
    )?;
    Ok((response, count["totalHits"].as_i64().unwrap_or(0)))
}

/// Обновляет документы `docs` в индексе `index` и удаляет те из `ids`, которых нет среди
/// `present`. Ждёт применения.
pub async fn upsert_and_delete(
    search: &Search,
    index: &str,
    docs: &Value,
    ids: &[Uuid],
    present: &[Uuid],
) -> Result<(), SearchError> {
    if docs.as_array().is_some_and(|docs| !docs.is_empty()) {
        search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents?primaryKey=id"),
                Some(docs),
            )
            .await?;
    }
    let deleted: Vec<Uuid> = ids
        .iter()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    if !deleted.is_empty() {
        let result = search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents/delete-batch"),
                Some(&json!(deleted)),
            )
            .await;
        // Удалять из ещё не созданного индекса нечего.
        ignore(result, "index_not_found")?;
    }
    Ok(())
}

/// Поиск временно недоступен: Meilisearch не ответил или выключен. Отдаётся как 503 — данные
/// целы, повторить запрос есть смысл.
pub fn unavailable() -> AppError {
    AppError::Unavailable("search is temporarily unavailable".into())
}

/// Пропускает ожидаемую ошибку Meilisearch (`index_not_found` — удалять нечего).
pub fn ignore(result: Result<(), SearchError>, code: &str) -> Result<(), SearchError> {
    match result {
        Err(error) if error.code() == Some(code) => Ok(()),
        other => other,
    }
}
