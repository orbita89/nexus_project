//! Админка баннеров страниц сайта (таблица `banners`, публичный список — `crate::banners`).

use super::nullable_param;
use crate::banners::{self, Banner, COLUMNS};
use crate::models::nullable;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// `(starts_at, ends_at)` баннера.
type Window = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

#[derive(Debug, Deserialize, IntoParams)]
pub struct AdminBannersQuery {
    /// Только эта страница; без него — все.
    pub page: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateBanner {
    pub page: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub image_url: String,
    pub link_url: Option<String>,
    /// По умолчанию 0.
    pub position: Option<i32>,
    /// По умолчанию `true`.
    pub is_active: Option<bool>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
}

/// Переданные поля заменяются, `null` очищает необязательное.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateBanner {
    pub page: Option<String>,
    pub title: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub subtitle: Option<Option<String>>,
    pub image_url: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub link_url: Option<Option<String>>,
    pub position: Option<i32>,
    pub is_active: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<DateTime<Utc>>)]
    pub starts_at: Option<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<DateTime<Utc>>)]
    pub ends_at: Option<Option<DateTime<Utc>>>,
}

/// Все баннеры (и неактивные, и вне окна показа) по странице, затем `position`.
#[utoipa::path(
    get, operation_id = "admin_list_banners", path = "/admin/banners", tag = "catalog-admin",
    security(("bearer" = [])),
    params(AdminBannersQuery),
    responses(
        (status = 200, description = "Баннеры", body = Page<Banner>),
        (status = 400, description = "Неизвестная страница", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Query(query): Query<AdminBannersQuery>,
) -> AppResult<Json<Page<Banner>>> {
    let page = query.page.as_deref().map(banners::page).transpose()?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let items: Vec<Banner> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM banners WHERE $1::text IS NULL OR page = $1
         ORDER BY page, position, created_at, id LIMIT $2 OFFSET $3"
    ))
    .bind(&page)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM banners WHERE $1::text IS NULL OR page = $1")
            .bind(&page)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Создать баннер.
#[utoipa::path(
    post, operation_id = "create_banner", path = "/admin/banners", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreateBanner,
    responses(
        (status = 201, description = "Создан", body = Banner),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateBanner>,
) -> AppResult<(StatusCode, Json<Banner>)> {
    let page = banners::page(&req.page)?;
    let title = banners::title(&req.title)?;
    let subtitle = banners::subtitle(req.subtitle)?;
    let image_url = banners::image(&req.image_url)?;
    let link_url = banners::link(req.link_url)?;
    banners::window(req.starts_at, req.ends_at)?;

    let banner: Banner = sqlx::query_as(&format!(
        "INSERT INTO banners (page, title, subtitle, image_url, link_url, position, is_active,
                              starts_at, ends_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {COLUMNS}"
    ))
    .bind(page)
    .bind(title)
    .bind(subtitle)
    .bind(image_url)
    .bind(link_url)
    .bind(req.position.unwrap_or(0))
    .bind(req.is_active.unwrap_or(true))
    .bind(req.starts_at)
    .bind(req.ends_at)
    .fetch_one(&state.db)
    .await?;
    tracing::info!(admin_id = %admin.id, banner_id = %banner.id, page = %banner.page, "banner created");
    Ok((StatusCode::CREATED, Json(banner)))
}

/// Изменить баннер: переданные поля заменяются, `null` очищает необязательное.
#[utoipa::path(
    patch, operation_id = "update_banner", path = "/admin/banners/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id баннера")),
    request_body = UpdateBanner,
    responses(
        (status = 200, description = "Изменён", body = Banner),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найден", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateBanner>,
) -> AppResult<Json<Banner>> {
    let page = req.page.as_deref().map(banners::page).transpose()?;
    let title = req.title.as_deref().map(banners::title).transpose()?;
    let (set_subtitle, subtitle) = nullable_param(req.subtitle);
    let subtitle = banners::subtitle(subtitle)?;
    let image_url = req.image_url.as_deref().map(banners::image).transpose()?;
    let (set_link, link_url) = nullable_param(req.link_url);
    let link_url = banners::link(link_url)?;
    let (set_starts, starts_at) = nullable_param(req.starts_at);
    let (set_ends, ends_at) = nullable_param(req.ends_at);

    let mut tx = state.db.begin().await?;
    let current: Option<Window> =
        sqlx::query_as("SELECT starts_at, ends_at FROM banners WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (old_starts, old_ends) = current.ok_or(AppError::NotFound)?;
    banners::window(
        if set_starts { starts_at } else { old_starts },
        if set_ends { ends_at } else { old_ends },
    )?;

    let banner: Banner = sqlx::query_as(&format!(
        "UPDATE banners SET
            page = COALESCE($2, page),
            title = COALESCE($3, title),
            subtitle = CASE WHEN $4 THEN $5 ELSE subtitle END,
            image_url = COALESCE($6, image_url),
            link_url = CASE WHEN $7 THEN $8 ELSE link_url END,
            position = COALESCE($9, position),
            is_active = COALESCE($10, is_active),
            starts_at = CASE WHEN $11 THEN $12 ELSE starts_at END,
            ends_at = CASE WHEN $13 THEN $14 ELSE ends_at END
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(page)
    .bind(title)
    .bind(set_subtitle)
    .bind(subtitle)
    .bind(image_url)
    .bind(set_link)
    .bind(link_url)
    .bind(req.position)
    .bind(req.is_active)
    .bind(set_starts)
    .bind(starts_at)
    .bind(set_ends)
    .bind(ends_at)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, banner_id = %id, "banner updated");
    Ok(Json(banner))
}

/// Удалить баннер.
#[utoipa::path(
    delete, operation_id = "delete_banner", path = "/admin/banners/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id баннера")),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найден", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM banners WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(admin_id = %admin.id, banner_id = %id, "banner deleted");
    Ok(StatusCode::NO_CONTENT)
}
