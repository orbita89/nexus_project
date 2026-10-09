//! Единый тип ошибки для HTTP-хендлеров всех сервисов.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

pub type AppResult<T> = Result<T, AppError>;

/// Тело любого ответа с ошибкой. Нужно для документации API.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    /// Описание ошибки (английский текст для логов; формулировка может меняться).
    #[schema(example = "unauthorized")]
    pub error: String,
    /// Машинный код: по нему клиент выбирает сообщение. Список — `documents/for-other-modules.md`.
    #[schema(example = "unauthorized")]
    pub code: Option<String>,
    /// Только для 429: через сколько секунд можно повторить (то же, что заголовок `Retry-After`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 80)]
    pub retry_after: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,

    #[error("{0}")]
    BadRequest(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    /// Вход по паролю до подтверждения email.
    #[error("email not verified")]
    EmailNotVerified,

    #[error("conflict: {0}")]
    Conflict(String),

    /// Сработал лимит; `retry_after_secs` — когда можно повторить (заголовок `Retry-After`).
    #[error("too many requests, try again later")]
    TooManyRequests { retry_after_secs: u64 },

    /// Внешний сервис (например, Meilisearch) недоступен. Сообщение уходит клиенту.
    #[error("{0}")]
    Unavailable(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// Непредвиденная ошибка (хеширование, подпись токена, ...). Детали — только в лог.
    #[error("internal error: {0}")]
    Internal(String),

    /// Ошибка `source` с уточнённым машинным кодом (`username_taken` вместо `conflict`).
    /// Статус и текст — от `source`. Создаётся через [`AppError::with_code`].
    #[error("{source}")]
    Coded {
        code: &'static str,
        source: Box<AppError>,
    },
}

impl AppError {
    /// Та же ошибка с конкретным кодом: `AppError::Conflict(..).with_code("username_taken")`.
    pub fn with_code(self, code: &'static str) -> Self {
        match self {
            AppError::Coded { source, .. } => AppError::Coded { code, source },
            source => AppError::Coded {
                code,
                source: Box::new(source),
            },
        }
    }

    /// Машинный код ответа: уточнённый через [`with_code`](Self::with_code) или общий для варианта.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::NotFound => "not_found",
            AppError::BadRequest(_) => "validation",
            AppError::Unauthorized => "unauthorized",
            AppError::Forbidden => "forbidden",
            AppError::EmailNotVerified => "email_not_verified",
            AppError::Conflict(_) => "conflict",
            AppError::TooManyRequests { .. } => "rate_limited",
            AppError::Unavailable(_) => "unavailable",
            AppError::Database(_) | AppError::Internal(_) => "internal",
            AppError::Coded { code, source } => match **source {
                // Детали внутренних ошибок наружу не отдаём, и код тоже.
                AppError::Database(_) | AppError::Internal(_) => "internal",
                _ => code,
            },
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let code = self.code();
        self.respond(code)
    }
}

impl AppError {
    /// Ответ с кодом `code`; у [`AppError::Coded`] статус и текст — от исходной ошибки.
    fn respond(self, code: &'static str) -> Response {
        let text = self.to_string();
        let (status, message) = match self {
            AppError::Coded { source, .. } => return source.respond(code),
            AppError::NotFound => (StatusCode::NOT_FOUND, text),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, text),
            AppError::Forbidden | AppError::EmailNotVerified => (StatusCode::FORBIDDEN, text),
            AppError::Conflict(m) => (StatusCode::CONFLICT, m),
            AppError::TooManyRequests { retry_after_secs } => {
                let body = ErrorBody {
                    error: text,
                    code: Some(code.to_string()),
                    retry_after: Some(retry_after_secs),
                };
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, retry_after_secs.to_string())],
                    Json(body),
                )
                    .into_response();
            }
            AppError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            // Детали внутренних ошибок наружу не отдаём — только в лог.
            AppError::Database(e) => {
                tracing::error!(error = %e, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
            AppError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };

        let body = ErrorBody {
            error: message,
            code: Some(code.to_string()),
            retry_after: None,
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(error: AppError) -> StatusCode {
        error.into_response().status()
    }

    #[test]
    fn maps_errors_to_status_codes() {
        assert_eq!(status_of(AppError::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(
            status_of(AppError::BadRequest("x".into())),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(status_of(AppError::Unauthorized), StatusCode::UNAUTHORIZED);
        assert_eq!(status_of(AppError::Forbidden), StatusCode::FORBIDDEN);
        assert_eq!(
            status_of(AppError::Conflict("x".into())),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status_of(AppError::Unavailable("x".into())),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn too_many_requests_tells_when_to_retry() {
        let response = AppError::TooManyRequests {
            retry_after_secs: 80,
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "80");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["retry_after"], 80);
        assert!(body["error"].is_string());
    }

    async fn body_of(error: AppError) -> (StatusCode, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn every_error_has_a_code() {
        let (_, body) = body_of(AppError::NotFound).await;
        assert_eq!(body["code"], "not_found");
        assert_eq!(body["error"], "not found");
        let (_, body) = body_of(AppError::BadRequest("x is bad".into())).await;
        assert_eq!(body["code"], "validation");
        let (_, body) = body_of(AppError::EmailNotVerified).await;
        assert_eq!(body["code"], "email_not_verified");
        let (_, body) = body_of(AppError::TooManyRequests {
            retry_after_secs: 5,
        })
        .await;
        assert_eq!(body["code"], "rate_limited");
        let (_, body) = body_of(AppError::Internal("secret".into())).await;
        assert_eq!(body["code"], "internal");
        assert_eq!(body["error"], "internal server error");
    }

    #[tokio::test]
    async fn with_code_keeps_status_and_message() {
        let error = AppError::Conflict("username already taken".into()).with_code("username_taken");
        assert_eq!(error.code(), "username_taken");
        let (status, body) = body_of(error).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "username_taken");
        assert_eq!(body["error"], "username already taken");

        // Повторный with_code заменяет код, а не вкладывает ошибку.
        let error = AppError::Forbidden.with_code("a").with_code("own_review");
        let (status, body) = body_of(error).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "own_review");

        // Внутренняя ошибка не раскрывает и код.
        let error = AppError::Internal("x".into()).with_code("leak");
        assert_eq!(body_of(error).await.1["code"], "internal");
    }

    #[test]
    fn database_error_is_internal() {
        let error = AppError::Database(sqlx::Error::RowNotFound);
        assert_eq!(status_of(error), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
