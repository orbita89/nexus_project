//! Модерация: admin удаляет любую рецензию, комментарий к ней, коллекцию, тему и сообщение,
//! помечает коллекции особыми (с читаемым адресом), меняет название и описание любой коллекции,
//! закрывает темы.

use crate::collection_comments;
use crate::collections::{self, Editor};
use crate::comments;
use crate::events;
use crate::models::{
    AdminCollectionsQuery, AdminUpdateCollection, Collection, CollectionItem, CollectionRow,
    FeatureCollection, PutCollectionItem, COLLECTION_COLUMNS,
};
use crate::posts;
use crate::refs;
use crate::slug;
use crate::validate::{self, MAX_COLLECTION_DESCRIPTION, MAX_COLLECTION_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::PgConnection;
use uuid::Uuid;

/// Длина URL обоев и обложки особой коллекции.
const MAX_STYLE_URL: usize = 2048;

/// Удалить любую рецензию.
#[utoipa::path(
    delete, operation_id = "admin_delete_review", path = "/admin/reviews/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id рецензии")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete_review(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let review: Option<(Uuid, Uuid)> =
        sqlx::query_as("DELETE FROM reviews WHERE id = $1 RETURNING user_id, entity_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let (author, entity) = review.ok_or(AppError::NotFound)?;
    tracing::info!(admin_id = %admin.id, review_id = %id, author_id = %author, "review deleted by admin");
    events::publish(&state, events::review("review.deleted", id, entity, author));
    crate::ratings::sync(&state, entity).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любой комментарий к рецензии.
#[utoipa::path(
    delete, operation_id = "admin_delete_review_comment", path = "/admin/review-comments/{id}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id комментария")),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет или уже удалён", body = ErrorBody),
    )
)]
pub async fn delete_review_comment(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let author = comments::remove(&state, id, None).await?;
    tracing::info!(admin_id = %admin.id, comment_id = %id, author_id = %author, "review comment deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любой комментарий в обсуждении коллекции (с ответами, если он корневой).
#[utoipa::path(
    delete, operation_id = "admin_delete_collection_comment", path = "/admin/collection-comments/{id}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id комментария")),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет или уже удалён", body = ErrorBody),
    )
)]
pub async fn delete_collection_comment(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let author = collection_comments::remove(&state, id, None).await?;
    tracing::info!(admin_id = %admin.id, comment_id = %id, author_id = %author, "collection comment deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любую коллекцию, в том числе приватную.
#[utoipa::path(
    delete, operation_id = "admin_delete_collection", path = "/admin/collections/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let owner: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM collections WHERE id = $1 RETURNING user_id")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let owner = owner.ok_or(AppError::NotFound)?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, owner_id = %owner, "collection deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Занят ли slug другой коллекцией: текущий адрес или бывший (история, в том числе удалённых).
async fn slug_taken(tx: &mut PgConnection, slug: &str, id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM collections WHERE slug = $1 AND id <> $2)
             OR EXISTS (SELECT 1 FROM collection_slug_history
                        WHERE slug = $1 AND collection_id IS DISTINCT FROM $2)",
    )
    .bind(slug)
    .bind(id)
    .fetch_one(tx)
    .await?)
}

/// Пометить публичную коллекцию особой, поменять её порядок, адрес или оформление (обои, обложка,
/// тема).
#[utoipa::path(
    put, operation_id = "admin_feature_collection", path = "/admin/collections/{id}/featured",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body(content = FeatureCollection, description = "Можно не передавать тело: в конец, slug — транслит названия"),
    responses(
        (status = 200, description = "Особая", body = Collection),
        (status = 400, description = "Коллекция приватная (`collection_private`); неверные поля (`validation` с `field`): slug — формат, UUID, служебное слово; `wallpaper_url`, `cover_url` — не https-URL или длиннее 2048; `theme` — не `^[a-z0-9-]{1,32}$`; `wallpaper_mode` — не `header`/`page`; `wallpaper_dim` — не целое 0–100; `wallpaper_blur` — не целое 0–20; `effects` — не boolean; `accent_color` — не `#rrggbb`; `header_mode` — не `styled`/`plain`/`none`", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
        (status = 409, description = "slug занят другой коллекцией (сейчас или раньше)", body = ErrorBody),
    )
)]
pub async fn feature_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    body: Option<JsonBody<FeatureCollection>>,
) -> AppResult<Json<Collection>> {
    let req = body.map(|JsonBody(req)| req).unwrap_or_default();
    let wanted = req
        .slug
        .as_deref()
        .map(slug::validate)
        .transpose()
        .map_err(|e| e.on_field("slug"))?;
    let wallpaper = req
        .wallpaper_url
        .map(|value| style_url("wallpaper_url", value))
        .transpose()?;
    let cover = req
        .cover_url
        .map(|value| style_url("cover_url", value))
        .transpose()?;
    let theme = req.theme.map(style_theme).transpose()?;
    // Оформление страницы: `null` — значение по умолчанию (у `wallpaper_dim`, `accent_color` — NULL).
    let mode = req.wallpaper_mode.map(style_mode).transpose()?;
    let dim = req
        .wallpaper_dim
        .map(|value| style_int("wallpaper_dim", value, 100))
        .transpose()?;
    let blur = req
        .wallpaper_blur
        .map(|value| style_int("wallpaper_blur", value, 20).map(|blur| blur.unwrap_or(0)))
        .transpose()?;
    let effects = req.effects.map(style_effects).transpose()?;
    let accent = req.accent_color.map(style_accent).transpose()?;
    let header = req.header_mode.map(style_header).transpose()?;

    let mut tx = state.db.begin().await?;
    // Выбор и запись slug — по очереди: две коллекции не займут один адрес.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('collection-slug', 0))")
        .execute(&mut *tx)
        .await?;
    let found: Option<(bool, Option<String>, String)> =
        sqlx::query_as("SELECT is_public, slug, title FROM collections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (is_public, current, title) = found.ok_or(AppError::NotFound)?;
    if !is_public {
        return Err(
            AppError::BadRequest("featured collection must be public".into())
                .with_code("collection_private"),
        );
    }
    let new_slug = match (wanted, current.clone()) {
        (Some(wanted), _) => {
            if slug_taken(&mut tx, &wanted, id).await? {
                return Err(AppError::Conflict(format!(
                    "slug {wanted} is already taken"
                )));
            }
            wanted
        }
        (None, Some(current)) => current,
        (None, None) => {
            let base = slug::from_title(&title);
            let mut candidate = base.clone();
            let mut n = 2;
            while slug_taken(&mut tx, &candidate, id).await? {
                candidate = format!("{base}-{n}");
                n += 1;
            }
            candidate
        }
    };
    if let Some(old) = current.filter(|old| *old != new_slug) {
        sqlx::query(
            "INSERT INTO collection_slug_history (slug, collection_id) VALUES ($1, $2)
             ON CONFLICT (slug) DO UPDATE SET collection_id = EXCLUDED.collection_id",
        )
        .bind(&old)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    // Свой бывший адрес можно вернуть: из истории он уходит.
    sqlx::query("DELETE FROM collection_slug_history WHERE slug = $1 AND collection_id = $2")
        .bind(&new_slug)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Оформление: не передано — не меняется (`$4`/`$6`/`$8` = false), `null` — сбрасывается.
    sqlx::query(
        "UPDATE collections SET slug = $3,
             featured_position = COALESCE($2, featured_position,
                 (SELECT COALESCE(max(featured_position) + 1, 0) FROM collections
                  WHERE featured_position IS NOT NULL)),
             wallpaper_url = CASE WHEN $4 THEN $5 ELSE wallpaper_url END,
             cover_url = CASE WHEN $6 THEN $7 ELSE cover_url END,
             theme = CASE WHEN $8 THEN $9 ELSE theme END,
             wallpaper_mode = COALESCE($10, wallpaper_mode),
             wallpaper_dim = CASE WHEN $11 THEN $12 ELSE wallpaper_dim END,
             wallpaper_blur = COALESCE($13, wallpaper_blur),
             effects = COALESCE($14, effects),
             accent_color = CASE WHEN $15 THEN $16 ELSE accent_color END,
             header_mode = COALESCE($17, header_mode)
         WHERE id = $1",
    )
    .bind(id)
    .bind(req.position)
    .bind(&new_slug)
    .bind(wallpaper.is_some())
    .bind(wallpaper.flatten())
    .bind(cover.is_some())
    .bind(cover.flatten())
    .bind(theme.is_some())
    .bind(theme.flatten())
    .bind(mode)
    .bind(dim.is_some())
    .bind(dim.flatten())
    .bind(blur)
    .bind(effects)
    .bind(accent.is_some())
    .bind(accent.flatten())
    .bind(header)
    .execute(&mut *tx)
    .await
    .map_err(collections::featured_must_be_public)?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, slug = %new_slug, "collection featured by admin");
    let row = collections::load(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(refs::collection(&state, row).await?))
}

/// Снять с коллекции отметку «особая». slug уходит в историю: старый адрес ведёт на
/// `/collections/{id}`, другой коллекции он не достанется. Оформление сбрасывается.
#[utoipa::path(
    delete, operation_id = "admin_unfeature_collection", path = "/admin/collections/{id}/featured",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    responses(
        (status = 204, description = "Больше не особая (повторно — тоже)"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn unfeature_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let current: Option<Option<String>> =
        sqlx::query_scalar("SELECT slug FROM collections WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let current = current.ok_or(AppError::NotFound)?;
    if let Some(old) = &current {
        sqlx::query(
            "INSERT INTO collection_slug_history (slug, collection_id) VALUES ($1, $2)
             ON CONFLICT (slug) DO UPDATE SET collection_id = EXCLUDED.collection_id",
        )
        .bind(old)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE collections SET featured_position = NULL, slug = NULL,
             wallpaper_url = NULL, cover_url = NULL, theme = NULL,
             wallpaper_mode = 'header', wallpaper_dim = NULL, wallpaper_blur = 0,
             effects = true, accent_color = NULL, header_mode = 'styled'
         WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, "collection unfeatured by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Длина запроса поиска коллекций.
const MAX_COLLECTION_QUERY: usize = 200;

/// Поиск любых коллекций (в том числе приватных) по подстроке названия: редактор связанных
/// подборок, «Подборки». Сверху — точное совпадение названия, затем начинающиеся с запроса, затем
/// больше произведений, новее.
#[utoipa::path(
    get, operation_id = "admin_search_collections", path = "/admin/collections", tag = "social-admin",
    security(("bearer" = [])),
    params(AdminCollectionsQuery),
    responses(
        (status = 200, description = "Страница коллекций", body = Page<Collection>),
        (status = 400, description = "`q` длиннее 200 (`validation`, `field: q`), неверный параметр", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn search_collections(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Query(query): Query<AdminCollectionsQuery>,
) -> AppResult<Json<Page<Collection>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let q = query
        .q
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty());
    if q.as_ref()
        .is_some_and(|q| q.chars().count() > MAX_COLLECTION_QUERY)
    {
        return Err(AppError::Invalid {
            field: "q".into(),
            message: format!("q must be at most {MAX_COLLECTION_QUERY} chars"),
        });
    }
    // Подстрока для ILIKE: `%`, `_` и `\` в запросе — буквально.
    let pattern = q.as_ref().map(|q| {
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{escaped}%")
    });
    let filter = "($1::text IS NULL OR c.title ILIKE $1)
         AND ($2::bool IS NULL OR c.is_public = $2)
         AND ($3::bool IS NULL OR (c.featured_position IS NOT NULL) = $3)";
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM collections c WHERE {filter}"
    ))
    .bind(&pattern)
    .bind(query.public)
    .bind(query.featured)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<CollectionRow> = sqlx::query_as(&format!(
        "SELECT {COLLECTION_COLUMNS} FROM collections c WHERE {filter}
         ORDER BY lower(c.title) = lower($4) DESC,
                  starts_with(lower(c.title), lower($4)) DESC,
                  items_count DESC, c.created_at DESC, c.id
         LIMIT $5 OFFSET $6"
    ))
    .bind(&pattern)
    .bind(query.public)
    .bind(query.featured)
    .bind(q.as_deref().unwrap_or(""))
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: refs::collections(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Изменить название и описание любой коллекции (например, чужой особой), закрыть или открыть
/// её обсуждение. Видимость здесь не
/// меняется: `is_public` в теле — 400.
#[utoipa::path(
    patch, operation_id = "admin_update_collection", path = "/admin/collections/{id}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id коллекции")),
    request_body = AdminUpdateCollection,
    responses(
        (status = 200, description = "Изменена", body = Collection),
        (status = 400, description = "Неверные поля (`validation` с `field`: `title` 1–200, `description` до 2000) или лишнее поле (`is_public`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn update_collection(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<AdminUpdateCollection>,
) -> AppResult<Json<Collection>> {
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_COLLECTION_TITLE))
        .transpose()
        .map_err(|e| e.on_field("title"))?;
    let description = req
        .description
        .map(|value| validate::optional("description", value, MAX_COLLECTION_DESCRIPTION))
        .transpose()
        .map_err(|e| e.on_field("description"))?;
    let updated = sqlx::query(
        "UPDATE collections SET
            title = COALESCE($2, title),
            description = CASE WHEN $3 THEN $4 ELSE description END,
            comments_locked = COALESCE($5, comments_locked)
         WHERE id = $1",
    )
    .bind(id)
    .bind(title)
    .bind(description.is_some())
    .bind(description.flatten())
    .bind(req.comments_locked)
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(admin_id = %admin.id, collection_id = %id, "collection updated by admin");
    let row = collections::load(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(refs::collection(&state, row).await?))
}

/// URL оформления (обои, обложка): https, до 2048 символов; `null` — убрать.
fn style_url(field: &str, value: Option<String>) -> AppResult<Option<String>> {
    let value = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let Some(url) = &value {
        let ok = url.len() <= MAX_STYLE_URL
            && url
                .strip_prefix("https://")
                .is_some_and(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace));
        if !ok {
            return Err(AppError::Invalid {
                field: field.into(),
                message: format!("{field} must be an https URL up to {MAX_STYLE_URL} chars"),
            });
        }
    }
    Ok(value)
}

fn invalid(field: &str, message: &str) -> AppError {
    AppError::Invalid {
        field: field.into(),
        message: format!("{field} {message}"),
    }
}

/// Где обои: `header` или `page`; `null` — `header`.
fn style_mode(value: Option<Value>) -> AppResult<String> {
    match value {
        None => Ok("header".into()),
        Some(Value::String(mode)) if mode == "header" || mode == "page" => Ok(mode),
        Some(_) => Err(invalid("wallpaper_mode", "must be \"header\" or \"page\"")),
    }
}

/// Режим шапки: `styled`, `plain` или `none`; `null` — `styled`.
fn style_header(value: Option<Value>) -> AppResult<String> {
    match value {
        None => Ok("styled".into()),
        Some(Value::String(mode)) if ["styled", "plain", "none"].contains(&mode.as_str()) => {
            Ok(mode)
        }
        Some(_) => Err(invalid(
            "header_mode",
            "must be \"styled\", \"plain\" or \"none\"",
        )),
    }
}

/// Целое 0..=max; `null` — `None`.
fn style_int(field: &str, value: Option<Value>, max: i16) -> AppResult<Option<i16>> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|n| (0..=i64::from(max)).contains(n))
            .map(|n| Some(n as i16))
            .ok_or_else(|| invalid(field, &format!("must be an integer 0-{max}"))),
    }
}

/// Эффекты темы: boolean; `null` — `true`.
fn style_effects(value: Option<Value>) -> AppResult<bool> {
    match value {
        None => Ok(true),
        Some(Value::Bool(on)) => Ok(on),
        Some(_) => Err(invalid("effects", "must be a boolean")),
    }
}

/// Акцентный цвет `#rrggbb` (в нижнем регистре); `null` — убрать.
fn style_accent(value: Option<Value>) -> AppResult<Option<String>> {
    match value {
        None => Ok(None),
        Some(Value::String(color)) => {
            let color = color.trim().to_ascii_lowercase();
            let ok = color.len() == 7
                && color.starts_with('#')
                && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
            if ok {
                Ok(Some(color))
            } else {
                Err(invalid("accent_color", "must be #rrggbb"))
            }
        }
        Some(_) => Err(invalid("accent_color", "must be #rrggbb")),
    }
}

/// Тема оформления: `^[a-z0-9-]{1,32}$`; `null` — убрать.
fn style_theme(value: Option<String>) -> AppResult<Option<String>> {
    let value = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let Some(theme) = &value {
        let ok = theme.len() <= 32
            && theme
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(AppError::Invalid {
                field: "theme".into(),
                message: "theme must be 1-32 chars of a-z, 0-9 and -".into(),
            });
        }
    }
    Ok(value)
}

/// Добавить произведение в любую особую коллекцию или изменить его позицию и заметку.
#[utoipa::path(
    put, operation_id = "admin_put_collection_item", path = "/admin/collections/{id}/items/{slug}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id особой коллекции"),
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
    ),
    request_body(content = PutCollectionItem, description = "Можно не передавать тело: добавить в конец без заметки"),
    responses(
        (status = 201, description = "Добавлено", body = CollectionItem),
        (status = 200, description = "Изменено", body = CollectionItem),
        (status = 400, description = "Коллекция не особая (`not_featured`), неверные поля или уже 500 произведений", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет коллекции или сущности", body = ErrorBody),
    )
)]
pub async fn put_collection_item(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((id, slug)): Path<(Uuid, String)>,
    body: Option<JsonBody<PutCollectionItem>>,
) -> AppResult<(StatusCode, Json<CollectionItem>)> {
    let req = body.map(|JsonBody(req)| req).unwrap_or_default();
    let result = collections::put_item_as(&state, Editor::Admin, id, &slug, req).await?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, %slug, "featured collection item put by admin");
    Ok(result)
}

/// Убрать произведение из любой особой коллекции.
#[utoipa::path(
    delete, operation_id = "admin_delete_collection_item", path = "/admin/collections/{id}/items/{slug}",
    tag = "social-admin",
    security(("bearer" = [])),
    params(
        ("id" = Uuid, Path, description = "id особой коллекции"),
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
    ),
    responses(
        (status = 204, description = "Убрано"),
        (status = 400, description = "Коллекция не особая (`not_featured`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет коллекции, сущности или её нет в коллекции", body = ErrorBody),
    )
)]
pub async fn delete_collection_item(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((id, slug)): Path<(Uuid, String)>,
) -> AppResult<StatusCode> {
    let result = collections::delete_item_as(&state, Editor::Admin, id, &slug).await?;
    tracing::info!(admin_id = %admin.id, collection_id = %id, %slug, "featured collection item removed by admin");
    Ok(result)
}

/// Удалить любую тему вместе с сообщениями.
#[utoipa::path(
    delete, operation_id = "admin_delete_thread", path = "/admin/threads/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn delete_thread(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let entities = events::thread_entities(&mut *tx, id).await?;
    let author: Option<Uuid> =
        sqlx::query_scalar("DELETE FROM forum_threads WHERE id = $1 RETURNING author_id")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let author = author.ok_or(AppError::NotFound)?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, author_id = %author, "thread deleted by admin");
    events::publish(
        &state,
        events::thread_changed("thread.deleted", id, &entities),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить любое сообщение. Если на него есть ответы, остаётся заглушка «сообщение удалено».
#[utoipa::path(
    delete, operation_id = "admin_delete_post", path = "/admin/posts/{id}", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сообщения")),
    responses(
        (status = 204, description = "Удалено"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет или уже удалено", body = ErrorBody),
    )
)]
pub async fn delete_post(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let removed = posts::remove(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(admin_id = %admin.id, post_id = %id, author_id = %removed.author_id, "post deleted by admin");
    events::publish(
        &state,
        events::post_changed("post.deleted", removed.thread_id, id),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Закрыть тему для ответов (повторно — тоже 204). Править и удалять свои сообщения можно.
#[utoipa::path(
    put, operation_id = "admin_lock_thread", path = "/admin/threads/{id}/lock", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Закрыта"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn lock_thread(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    set_locked(&state, id, true).await?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, "thread locked by admin");
    Ok(StatusCode::NO_CONTENT)
}

/// Открыть тему для ответов (повторно — тоже 204).
#[utoipa::path(
    delete, operation_id = "admin_unlock_thread", path = "/admin/threads/{id}/lock", tag = "social-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id темы")),
    responses(
        (status = 204, description = "Открыта"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn unlock_thread(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    set_locked(&state, id, false).await?;
    tracing::info!(admin_id = %admin.id, thread_id = %id, "thread unlocked by admin");
    Ok(StatusCode::NO_CONTENT)
}

async fn set_locked(state: &AppState, id: Uuid, locked: bool) -> AppResult<()> {
    let updated = sqlx::query("UPDATE forum_threads SET is_locked = $2 WHERE id = $1")
        .bind(id)
        .bind(locked)
        .execute(&state.db)
        .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    let entities = events::thread_entities(&state.db, id).await?;
    events::publish(
        state,
        events::thread_changed("thread.updated", id, &entities),
    );
    Ok(())
}
