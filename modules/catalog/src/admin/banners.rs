//! Админка баннеров сайта (таблица `banners`, публичный список — `crate::banners`).

use crate::banners::{
    self, Banner, BannerButton, BannerKind, BannerSlide, BannerTarget, Draft, COLUMNS,
};
use crate::entities::like_pattern;
use crate::models::nullable;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::types::Json as SqlJson;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AdminBannersQuery {
    /// Цель: `all`, страница (`home`, `films`, …) или `path` — баннеры с такой целью.
    pub target: Option<String>,
    /// Совместимость: то же, что `target=<page>`.
    pub page: Option<String>,
    /// Подстрока заголовка (без учёта регистра).
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateBanner {
    /// По умолчанию `image`.
    pub kind: Option<BannerKind>,
    pub title: String,
    pub subtitle: Option<String>,
    /// Обязателен при `kind=image`.
    pub image_url: Option<String>,
    pub link_url: Option<String>,
    pub content_html: Option<String>,
    /// 1..10 при `kind=slides`.
    #[serde(default)]
    pub slides: Vec<BannerSlide>,
    /// 0..3.
    #[serde(default)]
    pub buttons: Vec<BannerButton>,
    pub theme: Option<String>,
    /// 1..20. Можно не передавать, если передан устаревший `page`.
    pub targets: Option<Vec<BannerTarget>>,
    /// Устаревшее: то же, что `targets: [{"type":"page","page":...}]`.
    pub page: Option<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// По умолчанию 0.
    pub priority: Option<i32>,
    /// По умолчанию 0.
    pub position: Option<i32>,
    /// По умолчанию `true`.
    pub is_active: Option<bool>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
}

/// Переданные поля заменяются (массивы `slides`, `buttons`, `targets`, `exclude_paths` —
/// целиком), `null` очищает необязательное.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateBanner {
    pub kind: Option<BannerKind>,
    pub title: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub subtitle: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub image_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub link_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub content_html: Option<Option<String>>,
    pub slides: Option<Vec<BannerSlide>>,
    pub buttons: Option<Vec<BannerButton>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub theme: Option<Option<String>>,
    pub targets: Option<Vec<BannerTarget>>,
    /// Устаревшее: заменяет `targets` на `[{"type":"page","page":...}]` (если `targets` не передан).
    pub page: Option<String>,
    pub exclude_paths: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub position: Option<i32>,
    pub is_active: Option<bool>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<DateTime<Utc>>)]
    pub starts_at: Option<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<DateTime<Utc>>)]
    pub ends_at: Option<Option<DateTime<Utc>>>,
}

/// Фильтр `targets @> ...` для `?target=` / `?page=`.
fn target_filter(target: Option<&str>) -> Result<Option<serde_json::Value>, AppError> {
    Ok(match target.map(str::trim) {
        None | Some("") => None,
        Some("all") => Some(json!([{ "type": "all" }])),
        Some("path") => Some(json!([{ "type": "path" }])),
        Some(page) => {
            let page = banners::page(page).map_err(|_| AppError::Invalid {
                field: "target".into(),
                message: format!(
                    "target must be all, path or one of: {}",
                    banners::PAGES.join(", ")
                ),
            })?;
            Some(json!([{ "type": "page", "page": page }]))
        }
    })
}

const LIST_WHERE: &str =
    "($1::jsonb IS NULL OR targets @> $1) AND ($2::text IS NULL OR title ILIKE $2)";

/// Все баннеры (и неактивные, и вне окна показа): `priority desc`, затем `position`.
#[utoipa::path(
    get, operation_id = "admin_list_banners", path = "/admin/banners", tag = "catalog-admin",
    security(("bearer" = [])),
    params(AdminBannersQuery),
    responses(
        (status = 200, description = "Баннеры", body = Page<Banner>),
        (status = 400, description = "Неизвестный target/page", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Query(query): Query<AdminBannersQuery>,
) -> AppResult<Json<Page<Banner>>> {
    let target = target_filter(query.target.as_deref().or(query.page.as_deref()))?;
    let pattern = query.q.as_deref().and_then(like_pattern);
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let items: Vec<Banner> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM banners WHERE {LIST_WHERE}
         ORDER BY priority DESC, position, created_at, id LIMIT $3 OFFSET $4"
    ))
    .bind(&target)
    .bind(&pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let total: i64 =
        sqlx::query_scalar(&format!("SELECT count(*) FROM banners WHERE {LIST_WHERE}"))
            .bind(&target)
            .bind(&pattern)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Записать проверенные поля: `id = None` — INSERT, иначе UPDATE всех колонок.
async fn save(
    executor: impl sqlx::PgExecutor<'_>,
    id: Option<Uuid>,
    d: Draft,
) -> Result<Option<Banner>, sqlx::Error> {
    let sql = match id {
        None => format!(
            "INSERT INTO banners (kind, title, subtitle, image_url, link_url, content_html, slides,
                                  buttons, theme, targets, exclude_paths, priority, position,
                                  is_active, starts_at, ends_at)
             SELECT $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17
             WHERE $1::uuid IS NULL
             RETURNING {COLUMNS}"
        ),
        Some(_) => format!(
            "UPDATE banners SET kind = $2, title = $3, subtitle = $4, image_url = $5,
                link_url = $6, content_html = $7, slides = $8, buttons = $9, theme = $10,
                targets = $11, exclude_paths = $12, priority = $13, position = $14,
                is_active = $15, starts_at = $16, ends_at = $17
             WHERE id = $1 RETURNING {COLUMNS}"
        ),
    };
    sqlx::query_as(&sql)
        .bind(id)
        .bind(d.kind.as_str())
        .bind(d.title)
        .bind(d.subtitle)
        .bind(d.image_url)
        .bind(d.link_url)
        .bind(d.content_html)
        .bind(SqlJson(d.slides))
        .bind(SqlJson(d.buttons))
        .bind(d.theme)
        .bind(SqlJson(d.targets))
        .bind(d.exclude_paths)
        .bind(d.priority)
        .bind(d.position)
        .bind(d.is_active)
        .bind(d.starts_at)
        .bind(d.ends_at)
        .fetch_optional(executor)
        .await
}

/// Устаревший `page` → `targets: [{"type":"page","page":...}]`.
fn legacy_page(page: Option<String>) -> Result<Option<Vec<BannerTarget>>, AppError> {
    page.map(|p| {
        banners::page(&p)
            .map(|page| vec![BannerTarget::Page { page }])
            .map_err(|e| e.on_field("page"))
    })
    .transpose()
}

/// Создать баннер.
#[utoipa::path(
    post, operation_id = "create_banner", path = "/admin/banners", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreateBanner,
    responses(
        (status = 201, description = "Создан", body = Banner),
        (status = 400, description = "Неверные поля (`field` — какое)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateBanner>,
) -> AppResult<(StatusCode, Json<Banner>)> {
    let targets = match req.targets {
        Some(targets) => targets,
        None => legacy_page(req.page)?.unwrap_or_default(),
    };
    let draft = Draft {
        kind: req.kind.unwrap_or(BannerKind::Image),
        title: req.title,
        subtitle: req.subtitle,
        image_url: req.image_url,
        link_url: req.link_url,
        content_html: req.content_html,
        slides: req.slides,
        buttons: req.buttons,
        theme: req.theme,
        targets,
        exclude_paths: req.exclude_paths,
        priority: req.priority.unwrap_or(0),
        position: req.position.unwrap_or(0),
        is_active: req.is_active.unwrap_or(true),
        starts_at: req.starts_at,
        ends_at: req.ends_at,
    }
    .validate()?;
    let banner = save(&state.db, None, draft)
        .await?
        .ok_or_else(|| AppError::Internal("banner insert returned no row".into()))?;
    tracing::info!(admin_id = %admin.id, banner_id = %banner.id, kind = %banner.kind, "banner created");
    Ok((StatusCode::CREATED, Json(banner)))
}

/// Изменить баннер: переданные поля заменяются (массивы — целиком), `null` очищает
/// необязательное; итог проверяется целиком, как при создании.
#[utoipa::path(
    patch, operation_id = "update_banner", path = "/admin/banners/{id}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id баннера")),
    request_body = UpdateBanner,
    responses(
        (status = 200, description = "Изменён", body = Banner),
        (status = 400, description = "Неверные поля (`field` — какое)", body = ErrorBody),
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
    let legacy = legacy_page(req.page)?;
    let mut tx = state.db.begin().await?;
    let current: Banner = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM banners WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let kind = req.kind.unwrap_or(match current.kind.as_str() {
        "html" => BannerKind::Html,
        "slides" => BannerKind::Slides,
        _ => BannerKind::Image,
    });
    let draft = Draft {
        kind,
        title: req.title.unwrap_or(current.title),
        subtitle: req.subtitle.unwrap_or(current.subtitle),
        image_url: req.image_url.unwrap_or(current.image_url),
        link_url: req.link_url.unwrap_or(current.link_url),
        content_html: req.content_html.unwrap_or(current.content_html),
        slides: req.slides.unwrap_or(current.slides.0),
        buttons: req.buttons.unwrap_or(current.buttons.0),
        theme: req.theme.unwrap_or(current.theme),
        targets: req.targets.or(legacy).unwrap_or(current.targets.0),
        exclude_paths: req.exclude_paths.unwrap_or(current.exclude_paths),
        priority: req.priority.unwrap_or(current.priority),
        position: req.position.unwrap_or(current.position),
        is_active: req.is_active.unwrap_or(current.is_active),
        starts_at: req.starts_at.unwrap_or(current.starts_at),
        ends_at: req.ends_at.unwrap_or(current.ends_at),
    }
    .validate()?;
    let banner = save(&mut *tx, Some(id), draft)
        .await?
        .ok_or(AppError::NotFound)?;
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
