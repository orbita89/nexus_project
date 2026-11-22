//! Клиент ревалидации статики фронтенда (ISR): `POST {ISR_URL}/_isr/revalidate`.
//!
//! Публичные страницы карточек фронтенд отдаёт готовыми HTML/JSON. После правки в админке
//! бэкенд просит пересобрать затронутые страницы. Какие пути пересобирать, решает модуль
//! (сейчас только `catalog`); здесь только транспорт.

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Сколько ждать один запрос к фронтенду: он пересобирает страницы до ответа.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Сколько путей отправлять одним запросом (тег может затронуть тысячи карточек).
pub const PATHS_PER_REQUEST: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum IsrError {
    #[error("isr is not configured")]
    Disabled,
    #[error("isr request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("isr responded {status}: {body}")]
    Status { status: u16, body: String },
}

/// Хэндл клиента: дешёво клонируется. Выключенный (`disabled`) отвечает [`IsrError::Disabled`].
#[derive(Clone)]
pub struct Isr {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    http: reqwest::Client,
    /// Полный адрес вебхука.
    url: String,
    /// Адрес хода пересборки: `GET {ISR_URL}/_isr/status`.
    status_url: String,
    secret: String,
}

impl Isr {
    /// `base_url` — адрес фронтенда (`http://web:3000`), `secret` — `Authorization: Bearer`.
    pub fn new(base_url: &str, secret: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with default TLS settings");
        Self {
            inner: Some(Arc::new(Inner {
                http,
                url: format!("{}/_isr/revalidate", base_url.trim_end_matches('/')),
                status_url: format!("{}/_isr/status", base_url.trim_end_matches('/')),
                secret: secret.to_string(),
            })),
        }
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Включён, если заданы и `ISR_URL`, и `ISR_SECRET`.
    pub fn from_config(config: &crate::Config) -> Self {
        match (&config.isr_url, &config.isr_secret) {
            (Some(url), Some(secret)) => Self::new(url, secret),
            _ => Self::disabled(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Запустить пересборку всей статики: `{"all": true}` (после перестройки индекса). Фронтенд
    /// отвечает `202` и пересобирает в фоне: успех значит «запущено», а не «готово».
    pub async fn revalidate_all(&self) -> Result<(), IsrError> {
        let inner = self.inner.as_ref().ok_or(IsrError::Disabled)?;
        inner.post(&json!({ "all": true })).await
    }

    /// Пересобрать в фоне статику одного раздела (`book`, `movie`, …) или всю (`None`):
    /// `{"all": true, "kind": ...}`. `Ok(false)` — пересборка уже идёт, вторая не запущена.
    pub async fn revalidate_kind(&self, kind: Option<&str>) -> Result<bool, IsrError> {
        let inner = self.inner.as_ref().ok_or(IsrError::Disabled)?;
        let body = match kind {
            Some(kind) => json!({ "all": true, "kind": kind }),
            None => json!({ "all": true }),
        };
        let reply = inner.post_json(&body).await?;
        Ok(reply
            .get("started")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Ход последней пересборки: тело `GET /_isr/status` как есть (`{"run": {...} | null}`).
    pub async fn status(&self) -> Result<serde_json::Value, IsrError> {
        let inner = self.inner.as_ref().ok_or(IsrError::Disabled)?;
        let response = inner
            .http
            .get(&inner.status_url)
            .bearer_auth(&inner.secret)
            .send()
            .await?;
        Inner::json_reply(response).await
    }

    /// Пересобрать страницы `paths` (`/films/dune-2021`). Пачками по [`PATHS_PER_REQUEST`];
    /// первая же ошибка прерывает отправку. Пустой список — без запросов.
    pub async fn revalidate(&self, paths: &[String]) -> Result<(), IsrError> {
        let inner = self.inner.as_ref().ok_or(IsrError::Disabled)?;
        for chunk in paths.chunks(PATHS_PER_REQUEST) {
            inner.post(&json!({ "paths": chunk })).await?;
        }
        Ok(())
    }
}

impl Inner {
    async fn post_json(&self, body: &serde_json::Value) -> Result<serde_json::Value, IsrError> {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.secret)
            .json(body)
            .send()
            .await?;
        Self::json_reply(response).await
    }

    async fn json_reply(response: reqwest::Response) -> Result<serde_json::Value, IsrError> {
        let status = response.status();
        if status.is_success() {
            // Пустое или не JSON-тело (старый фронтенд) — null: успех статусом, без подробностей.
            let text = response.text().await?;
            return Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null));
        }
        let body = response.text().await.unwrap_or_default();
        Err(IsrError::Status {
            status: status.as_u16(),
            body: body.chars().take(500).collect(),
        })
    }

    async fn post(&self, body: &serde_json::Value) -> Result<(), IsrError> {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.secret)
            .json(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(IsrError::Status {
            status: status.as_u16(),
            body: body.chars().take(500).collect(),
        })
    }
}
