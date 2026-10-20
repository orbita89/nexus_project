//! Баннеры (промо и новости) по страницам сайта: `GET /banners?page=` — публичный список,
//! правка — `admin::banners`. Таблица `banners`.

use crate::validate::{self, MAX_TITLE};
use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::{AppError, AppResult, AppState};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Страницы сайта, на которых бывают баннеры (CHECK в миграции `catalog_banners`).
pub const PAGES: [&str; 6] = ["home", "films", "series", "books", "games", "people"];
/// Сколько баннеров отдаёт публичный список.
pub const PUBLIC_LIMIT: i64 = 20;

pub(crate) const COLUMNS: &str = "id, page, title, subtitle, image_url, link_url, position, \
     is_active, starts_at, ends_at, created_at, updated_at";

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Banner {
    pub id: Uuid,
    #[schema(example = "films")]
    pub page: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub image_url: String,
    /// `http(s)://...` или путь сайта `/films/dune-2021`.
    pub link_url: Option<String>,
    pub position: i32,
    pub is_active: bool,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct BannersQuery {
    /// Страница: `home`, `films`, `series`, `books`, `games`, `people`.
    pub page: String,
}

/// `page` из списка [`PAGES`], иначе 400.
pub(crate) fn page(value: &str) -> Result<String, AppError> {
    if PAGES.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(AppError::BadRequest(format!(
            "page must be one of: {}",
            PAGES.join(", ")
        )))
    }
}

/// Ссылка баннера: `http(s)://` или путь сайта с `/` (не `//`).
pub(crate) fn link(value: Option<String>) -> Result<Option<String>, AppError> {
    match value.as_deref().map(str::trim) {
        Some(path) if path.starts_with('/') && !path.starts_with("//") => {
            let path = validate::required("link_url", path, validate::MAX_URL)?;
            if path.contains(char::is_whitespace) {
                return Err(AppError::BadRequest(
                    "link_url must not contain spaces".into(),
                ));
            }
            Ok(Some(path))
        }
        _ => validate::url("link_url", value),
    }
}

pub(crate) fn title(value: &str) -> Result<String, AppError> {
    validate::required("title", value, MAX_TITLE)
}

pub(crate) fn subtitle(value: Option<String>) -> Result<Option<String>, AppError> {
    validate::optional("subtitle", value, MAX_TITLE)
}

pub(crate) fn image(value: &str) -> Result<String, AppError> {
    validate::url("image_url", Some(value.to_string()))?
        .ok_or_else(|| AppError::BadRequest("image_url must not be empty".into()))
}

pub(crate) fn window(
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
) -> Result<(), AppError> {
    match (starts_at, ends_at) {
        (Some(start), Some(end)) if end <= start => Err(AppError::BadRequest(
            "ends_at must be later than starts_at".into(),
        )),
        _ => Ok(()),
    }
}

/// Баннеры страницы сайта: активные, сейчас в окне показа (`starts_at`/`ends_at`, пустые — без
/// границы), по `position`, затем старые сверху; до 20.
#[utoipa::path(
    get, operation_id = "list_banners", path = "/banners", tag = "catalog",
    params(BannersQuery),
    responses(
        (status = 200, description = "Баннеры страницы", body = [Banner]),
        (status = 400, description = "Неизвестная страница", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<BannersQuery>,
) -> AppResult<Json<Vec<Banner>>> {
    let page = page(&query.page)?;
    let banners: Vec<Banner> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM banners
         WHERE page = $1 AND is_active
           AND (starts_at IS NULL OR starts_at <= now())
           AND (ends_at IS NULL OR ends_at > now())
         ORDER BY position, created_at, id
         LIMIT $2"
    ))
    .bind(page)
    .bind(PUBLIC_LIMIT)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(banners))
}
