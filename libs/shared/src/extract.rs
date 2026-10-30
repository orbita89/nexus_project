//! Экстракторы запросов с ошибками в общем формате `{"error": "..."}`.

use crate::AppError;
use axum::extract::{FromRequest, FromRequestParts, OptionalFromRequest, Request};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
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
        <axum::Json<T> as FromRequest<S>>::from_request(req, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|rejection| AppError::BadRequest(rejection.body_text()))
    }
}

/// `Option<JsonBody<T>>` — тело необязательно: без `Content-Type` это `None`, иначе как `JsonBody`.
impl<S, T> OptionalFromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        if !req.headers().contains_key(header::CONTENT_TYPE) {
            return Ok(None);
        }
        <Self as FromRequest<S>>::from_request(req, state)
            .await
            .map(Some)
    }
}

/// Как `axum::extract::Path`, но ошибка разбора (`/collections/not-a-uuid`) — `400 {"error": "..."}`,
/// а не текст. Параметры в `#[utoipa::path]` описываются явно: `params(("id" = Uuid, Path))`.
pub struct Path<T>(pub T);

// Единственное место, где модулям можно трогать axum-версию (см. `clippy.toml`).
#[allow(clippy::disallowed_types)]
impl<S, T> FromRequestParts<S> for Path<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        <axum::extract::Path<T> as FromRequestParts<S>>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(|rejection| rejected(rejection.status(), rejection.body_text()))
    }
}

/// Как `axum::extract::Query`, но ошибка разбора (`?sort=best`, `?year=abc`) — `400 {"error": "..."}`.
/// В `#[utoipa::path]` — `params(T)`, где `T: IntoParams`.
pub struct Query<T>(pub T);

#[allow(clippy::disallowed_types)]
impl<S, T> FromRequestParts<S> for Query<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        <axum::extract::Query<T> as FromRequestParts<S>>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(|rejection| rejected(rejection.status(), rejection.body_text()))
    }
}

/// Ошибка клиента — 400 с текстом axum; 5xx (роут объявлен без нужного параметра) — баг, в лог.
fn rejected(status: StatusCode, text: String) -> AppError {
    if status.is_server_error() {
        AppError::Internal(text)
    } else {
        AppError::BadRequest(text)
    }
}

/// Повторяющиеся параметры запроса: `?kind=movie&kind=game`.
///
/// `serde_urlencoded` (на нём стоит [`Query`]) оставляет от повторов только последнее значение,
/// поэтому такие параметры читаются отдельно — экстрактором рядом с обычным `Query`. Значения,
/// перечисленные через запятую (`?kind=movie,game`), разбираются так же: клиенту удобнее то
/// одно, то другое, а для API это одно и то же.
pub struct Repeated(Vec<(String, String)>);

impl Repeated {
    /// Из строки запроса (`kind=movie&kind=game`) — для тестов и внутренних вызовов.
    pub fn from_query(query: &str) -> Self {
        Self(
            form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect(),
        )
    }

    /// Все значения параметра в порядке появления; пустые пропускаются.
    pub fn values(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(name, _)| name == key)
            .flat_map(|(_, value)| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Repeated {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_query(parts.uri.query().unwrap_or_default()))
    }
}

#[cfg(test)]
mod repeated_tests {
    use super::Repeated;

    #[test]
    fn repeats_and_commas_are_one_list() {
        let repeated = Repeated::from_query("kind=movie&kind=game&tag=sci-fi");
        assert_eq!(repeated.values("kind"), ["movie", "game"]);
        assert_eq!(repeated.values("tag"), ["sci-fi"]);
        assert_eq!(repeated.values("year"), Vec::<&str>::new());
        // Перечисление через запятую, пробелы и пустые значения.
        assert_eq!(
            Repeated::from_query("kind=movie,%20game&kind=&kind=,").values("kind"),
            ["movie", "game"]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[derive(serde::Deserialize)]
    struct Params {
        #[allow(dead_code)]
        n: i32,
    }

    async fn call(app: Router, uri: &str) -> (StatusCode, String) {
        let response = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn path_and_query_errors_are_json() {
        async fn by_id(Path(_): Path<uuid::Uuid>) -> StatusCode {
            StatusCode::OK
        }
        async fn listed(Query(_): Query<Params>) -> StatusCode {
            StatusCode::OK
        }
        let app = Router::new()
            .route("/items/{id}", get(by_id))
            .route("/items", get(listed));

        assert_eq!(
            call(app.clone(), "/items/not-a-uuid").await.0,
            StatusCode::BAD_REQUEST
        );
        let (status, body) = call(app.clone(), "/items?n=abc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(body["error"].as_str().unwrap().contains("n"), "{body}");

        let id = uuid::Uuid::new_v4();
        assert_eq!(
            call(app.clone(), &format!("/items/{id}")).await.0,
            StatusCode::OK
        );
        assert_eq!(call(app, "/items?n=1").await.0, StatusCode::OK);
    }

    #[test]
    fn server_errors_stay_internal() {
        let error = rejected(StatusCode::INTERNAL_SERVER_ERROR, "missing".into());
        assert_eq!(
            error.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
