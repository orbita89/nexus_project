//! Экстракторы запросов с ошибками в общем формате `{"error": "..."}`.

use crate::AppError;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;

/// Как `axum::Json`, но неразобранное тело (не JSON, неверный тип, неизвестное поле) — это
/// `400 {"error": "..."}`, а не 415/422 с текстом. В `#[utoipa::path]` тело описывается через
/// `request_body = T`.
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|rejection| AppError::BadRequest(rejection.body_text()))
    }
}
