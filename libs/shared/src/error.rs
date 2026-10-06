//! Единый тип ошибки для HTTP-хендлеров всех сервисов.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

pub type AppResult<T> = Result<T, AppError>;

/// Тело любого ответа с ошибкой. Нужно для документации API.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    /// Описание ошибки.
    #[schema(example = "unauthorized")]
    pub error: String,
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            AppError::Forbidden | AppError::EmailNotVerified => {
                (StatusCode::FORBIDDEN, self.to_string())
            }
            AppError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            AppError::TooManyRequests { retry_after_secs } => {
                let body = ErrorBody {
                    error: self.to_string(),
                    retry_after: Some(*retry_after_secs),
                };
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, retry_after_secs.to_string())],
                    Json(body),
                )
                    .into_response();
            }
            AppError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m.clone()),
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

    #[test]
    fn database_error_is_internal() {
        let error = AppError::Database(sqlx::Error::RowNotFound);
        assert_eq!(status_of(error), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
