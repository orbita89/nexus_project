//! Холодный архив каталога: фильмы вне горячего слоя в индексе Tantivy (`ARCHIVE_DIR`).
//!
//! Горячий слой — `entities` в PostgreSQL (полные карточки, сайт, Meilisearch): все фильмы с
//! русским названием и 500 самых популярных иностранных. Остальные ~1 млн хранятся только
//! «скелетом» в архиве (название, дата, постер): поиск по названию, страница фильма ([`get`]) и
//! возврат в каталог ([`promote`]). Раскладку делает
//! [`pipeline`] (`nexus archive dump|build`).

pub mod index;
pub mod pipeline;

pub use index::{Archive, ArchiveDoc, ArchiveError};

use crate::entities::detail_by_id;
use crate::jobs::Reporter;
use crate::models::EntityDetail;
use crate::publish::{self, Touched};
use crate::validate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::pagination::Page;
use shared::{AdminUser, AppError, AppResult, AppState};
use std::sync::Arc;
use utoipa::IntoParams;
use uuid::Uuid;

/// Результатов поиска по умолчанию и максимум.
pub const SEARCH_LIMIT: usize = 20;
/// Глубже в выдачу не листаем: Tantivy собирает `offset + limit` лучших.
pub const MAX_OFFSET: usize = 1_000;
const MAX_QUERY: usize = 200;

/// Архив из `ARCHIVE_DIR`. Не задан — 503.
pub fn open(state: &AppState) -> AppResult<Arc<Archive>> {
    let dir = state
        .config
        .archive_dir
        .as_deref()
        .ok_or_else(unavailable)?;
    Archive::open(std::path::Path::new(dir)).map_err(|error| {
        tracing::warn!(%error, dir, "archive index cannot be opened");
        unavailable()
    })
}

fn unavailable() -> AppError {
    AppError::Unavailable("archive is not available".into())
}

impl From<ArchiveError> for AppError {
    fn from(error: ArchiveError) -> Self {
        match error {
            ArchiveError::Busy => {
                AppError::Conflict("archive is being rebuilt, try again later".into())
            }
            ArchiveError::Index(error) => {
                tracing::warn!(%error, "archive index error");
                unavailable()
            }
        }
    }
}

/// Блокирующая работа с индексом (mmap может уйти в page fault) — вне потоков async.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ArchiveError> + Send + 'static,
) -> AppResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(format!("archive task failed: {e}")))?
        .map_err(AppError::from)
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArchiveSearchQuery {
    /// Слова из названия (русского или оригинального), 1–200 символов. Должны найтись все.
    #[param(example = "night visitor")]
    pub q: String,
    /// 1–20, по умолчанию 20.
    pub limit: Option<i64>,
    /// 0–1000, по умолчанию 0.
    pub offset: Option<i64>,
}

/// Поиск по холодному архиву: фильмы, которых нет на сайте (вне горячего слоя). Ищет по
/// названию и оригинальному названию, выше — точнее совпавшие и популярные. Страница фильма —
/// `GET /archive/entities/{slug}`, вернуть фильм на сайт — `POST /admin/archive/promote/{slug}`.
#[utoipa::path(
    get, operation_id = "archive_search", path = "/archive/search", tag = "catalog",
    params(ArchiveSearchQuery),
    responses(
        (status = 200, description = "Страница документов архива; `total` — всего найдено", body = Page<ArchiveDoc>),
        (status = 400, description = "Пустой или длинный `q`, `limit` вне 1–20, `offset` вне 0–1000", body = ErrorBody),
        (status = 503, description = "Архив не настроен (`ARCHIVE_DIR`) или недоступен", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<ArchiveSearchQuery>,
) -> AppResult<Json<Page<ArchiveDoc>>> {
    let q = query.q.trim().to_string();
    if q.is_empty() || q.chars().count() > MAX_QUERY {
        return Err(AppError::BadRequest(format!(
            "q must be 1-{MAX_QUERY} characters"
        )));
    }
    let limit = match query.limit {
        None => SEARCH_LIMIT,
        Some(limit @ 1..=20) => limit as usize,
        Some(_) => return Err(AppError::BadRequest("limit must be 1-20".into())),
    };
    let offset = match query.offset {
        None => 0,
        Some(offset @ 0..=1_000) => offset as usize,
        Some(_) => {
            return Err(AppError::BadRequest(format!(
                "offset must be 0-{MAX_OFFSET}"
            )))
        }
    };
    let archive = open(&state)?;
    let (items, total) = blocking(move || archive.search(&q, limit, offset)).await?;
    Ok(Json(Page {
        items,
        total: total as i64,
        limit: limit as i64,
        offset: offset as i64,
    }))
}

/// Фильм из архива для его страницы на сайте: название, оригинальное название, дата выхода,
/// постер. Только чтение: на сайт фильм возвращает админ (`POST /admin/archive/promote/{slug}`).
#[utoipa::path(
    get, operation_id = "archive_get", path = "/archive/entities/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug фильма в архиве")),
    responses(
        (status = 200, description = "Документ архива", body = ArchiveDoc),
        (status = 404, description = "В архиве нет такого slug (фильм мог вернуться на сайт: `GET /entities/{slug}`)", body = ErrorBody),
        (status = 503, description = "Архив не настроен (`ARCHIVE_DIR`) или недоступен", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<ArchiveDoc>> {
    validate::slug("slug", &slug).map_err(|_| AppError::NotFound)?;
    let archive = open(&state)?;
    blocking(move || archive.get(&slug))
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

/// Вернуть фильм из архива на сайт: строка в `entities` (`active`, `active_manual` — сборка
/// горячего слоя её не уберёт), связь с TMDB и popularity; затем документ удаляется из архива
/// (`delete_term` + `commit`), поиск сайта и статика обновляются. Карточка — «скелет»: название,
/// оригинальное название, дата выхода, постер.
#[utoipa::path(
    post, operation_id = "archive_promote", path = "/admin/archive/promote/{slug}",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug фильма в архиве")),
    responses(
        (status = 201, description = "Фильм в каталоге", body = EntityDetail),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "В архиве нет такого slug", body = ErrorBody),
        (status = 409, description = "slug или TMDB id уже в каталоге; архив занят сборкой", body = ErrorBody),
        (status = 503, description = "Архив не настроен или недоступен", body = ErrorBody),
    )
)]
pub async fn promote(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
) -> AppResult<(StatusCode, Json<EntityDetail>)> {
    validate::slug("slug", &slug).map_err(|_| AppError::NotFound)?;
    let archive = open(&state)?;
    let doc = {
        let (archive, slug) = (archive.clone(), slug.clone());
        blocking(move || archive.get(&slug)).await?
    }
    .ok_or(AppError::NotFound)?;
    // Писатель — до записи в БД: идёт сборка архива — 409, в БД ничего не меняется.
    let writer = {
        let archive = archive.clone();
        blocking(move || archive.writer()).await?
    };

    // TODO: Fetch rich data from external API (TMDB: описание, титры, жанры).

    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO entities
             (kind, slug, title, original_title, release_date, cover_url, active, active_manual)
         VALUES ($1, $2, $3, $4, $5, $6, true, true) RETURNING id",
    )
    .bind(doc.kind)
    .bind(&doc.slug)
    .bind(&doc.title)
    .bind(&doc.original_title)
    .bind(doc.release_date)
    .bind(&doc.cover_url)
    .fetch_one(&mut *tx)
    .await
    .map_err(validate::conflict("slug is already taken"))?;
    if let Some(tmdb_id) = doc.tmdb_id {
        sqlx::query("INSERT INTO entity_tmdb (entity_id, tmdb_id) VALUES ($1, $2)")
            .bind(id)
            .bind(tmdb_id)
            .execute(&mut *tx)
            .await
            .map_err(validate::conflict("tmdb id is already in the catalog"))?;
        sqlx::query("DELETE FROM archived_tmdb WHERE tmdb_id = $1")
            .bind(tmdb_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO entity_stats (entity_id, tmdb_popularity) VALUES ($1, $2)")
        .bind(id)
        .bind(doc.popularity)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Ленивое удаление из архива: документ помечен удалённым, место освободит слияние.
    let deleted = {
        let (archive, slug) = (archive.clone(), slug.clone());
        blocking(move || {
            let mut writer = writer;
            archive.delete(&writer, &slug);
            writer.commit()?;
            archive.reload()
        })
        .await
    };
    if let Err(error) = deleted {
        // Фильм уже в каталоге; документ уберёт следующая сборка.
        tracing::warn!(%error, %slug, "promoted entity is still in the archive");
    }

    publish::run(&state, Touched::entity(id), &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, %slug, "entity promoted from archive");
    Ok((
        StatusCode::CREATED,
        Json(detail_by_id(&state.db, id).await?),
    ))
}
