//! Холодный архив каталога: фильмы вне горячего слоя в индексе Tantivy (`ARCHIVE_DIR`).
//!
//! Горячий слой — `entities` в PostgreSQL (полные карточки, сайт, Meilisearch): все фильмы с
//! русским названием и 500 самых популярных иностранных. Остальные ~1 млн хранятся только
//! «скелетом» в архиве (название, дата, постер): поиск по названию, страница фильма ([`get`]),
//! возврат в каталог ([`promote`]) и ручной перенос ненужных фильмов в архив ([`demote`]). Раскладку делает
//! [`pipeline`] (`nexus archive dump|build`).

pub mod index;
pub mod pipeline;

pub use index::{Archive, ArchiveDoc, ArchiveError};

use crate::entities::detail_by_id;
use crate::jobs::Reporter;
use crate::models::EntityDetail;
use crate::models::EntityKind;
use crate::publish::{self, Touched};
use crate::validate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::Page;
use shared::{AdminUser, AppError, AppResult, AppState};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
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

/// Сколько фильмов переносить за запрос.
pub const MAX_DEMOTE: usize = 100;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoteRequest {
    /// slug'и фильмов на сайте, 1–100, без повторов.
    #[schema(example = json!(["the-night-visitor-1971"]))]
    pub slugs: Vec<String>,
}

/// Почему фильм не перенесён.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DemoteSkipReason {
    /// Нет на сайте (неверный slug или уже в архиве).
    NotFound,
    /// Не связан с TMDB: архив хранит фильмы TMDB (по TMDB id сборка и импорт его не вернут).
    NoTmdb,
    /// Нужен пользователям (интересы, рецензии, коллекции, отметки, реакции, темы форума):
    /// удаление строки стёрло бы их данные.
    InUse,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DemoteSkip {
    pub slug: String,
    pub reason: DemoteSkipReason,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DemoteResult {
    /// Перенесены в архив: удалены из каталога, поиска и статики.
    pub archived: Vec<String>,
    pub skipped: Vec<DemoteSkip>,
}

#[derive(sqlx::FromRow)]
struct DemoteRow {
    id: Uuid,
    slug: String,
    kind: EntityKind,
    title: String,
    original_title: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    tmdb_id: Option<i32>,
    popularity: Option<f64>,
}

/// Перенести ненужные фильмы с сайта в холодный архив: документ в Tantivy (название,
/// оригинальное название, дата, постер, TMDB id, popularity), затем строка удаляется из
/// PostgreSQL (каскадом титры, теги, статистика, полные данные TMDB; люди без других работ — тоже),
/// TMDB id — в `archived_tmdb` с `manual`: сборка горячего слоя такой фильм не вернёт, импорт
/// пропустит. Поиск сайта и статика обновляются. Вернуть — `POST /admin/archive/promote/{slug}`
/// («скелетом»: без описания, титров и жанров).
///
/// Не переносятся (`skipped`): нет на сайте, без TMDB, с данными пользователей.
#[utoipa::path(
    post, operation_id = "archive_demote", path = "/admin/archive/demote", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = DemoteRequest,
    responses(
        (status = 200, description = "Перенесённые и пропущенные с причиной", body = DemoteResult),
        (status = 400, description = "Пусто, больше 100, повтор или неверный slug", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Архив занят сборкой (ничего не перенесено)", body = ErrorBody),
        (status = 503, description = "Архив не настроен или недоступен", body = ErrorBody),
    )
)]
pub async fn demote(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<DemoteRequest>,
) -> AppResult<Json<DemoteResult>> {
    if req.slugs.is_empty() || req.slugs.len() > MAX_DEMOTE {
        return Err(AppError::BadRequest(format!(
            "slugs must contain 1-{MAX_DEMOTE} items"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for slug in &req.slugs {
        validate::slug("slugs", slug)?;
        if !seen.insert(slug.as_str()) {
            return Err(AppError::BadRequest(format!("duplicate slug {slug:?}")));
        }
    }
    let archive = open(&state)?;

    let rows: Vec<DemoteRow> = sqlx::query_as(
        "SELECT e.id, e.slug, e.kind, e.title, e.original_title, e.release_date, e.cover_url,
                t.tmdb_id, s.tmdb_popularity AS popularity
         FROM entities e
         LEFT JOIN entity_tmdb t ON t.entity_id = e.id
         LEFT JOIN entity_stats s ON s.entity_id = e.id
         WHERE e.slug = ANY($1)",
    )
    .bind(&req.slugs)
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let in_use = state.interests.entities_in_use(&ids).await?;

    let mut skipped = Vec::new();
    let mut by_slug: std::collections::HashMap<&str, &DemoteRow> =
        rows.iter().map(|r| (r.slug.as_str(), r)).collect();
    let mut chosen: Vec<&DemoteRow> = Vec::new();
    for slug in &req.slugs {
        let reason = match by_slug.remove(slug.as_str()) {
            None => DemoteSkipReason::NotFound,
            Some(row) if row.tmdb_id.is_none() => DemoteSkipReason::NoTmdb,
            Some(row) if in_use.contains(&row.id) => DemoteSkipReason::InUse,
            Some(row) => {
                chosen.push(row);
                continue;
            }
        };
        skipped.push(DemoteSkip {
            slug: slug.clone(),
            reason,
        });
    }
    if chosen.is_empty() {
        return Ok(Json(DemoteResult {
            archived: Vec::new(),
            skipped,
        }));
    }

    // До удаления: адреса страниц и участники (их фильмографии и известность меняются).
    let chosen_ids: Vec<Uuid> = chosen.iter().map(|r| r.id).collect();
    let touched = Touched::collect(&state.db, &chosen_ids, &[]).await?;
    let docs: Vec<ArchiveDoc> = chosen
        .iter()
        .map(|row| ArchiveDoc {
            slug: row.slug.clone(),
            title: row.title.clone(),
            original_title: row.original_title.clone(),
            kind: row.kind,
            tmdb_id: row.tmdb_id,
            release_date: row.release_date,
            popularity: row.popularity.unwrap_or(0.0),
            cover_url: row.cover_url.clone(),
        })
        .collect();

    // Сначала архив (коммит), потом удаление из БД: упадёт БД — документы убираются обратно.
    // Писатель держится до конца: промоушен и сборка в это время получают 409.
    let slugs: Vec<String> = docs.iter().map(|d| d.slug.clone()).collect();
    let writer = {
        let archive = archive.clone();
        blocking(move || {
            let mut writer = archive.writer()?;
            for doc in &docs {
                archive.upsert(&writer, doc)?;
            }
            writer.commit()?;
            Ok(writer)
        })
        .await?
    };

    let tmdb_ids: Vec<i32> = chosen.iter().filter_map(|r| r.tmdb_id).collect();
    let deleted = async {
        let mut tx = state.db.begin().await?;
        sqlx::query(
            "INSERT INTO archived_tmdb (tmdb_id, manual) SELECT unnest($1::int[]), true
             ON CONFLICT (tmdb_id) DO UPDATE SET manual = true",
        )
        .bind(&tmdb_ids)
        .execute(&mut *tx)
        .await?;
        let counts = pipeline::delete_entities(&mut tx, &chosen_ids, true).await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(counts)
    }
    .await;
    let (archive_result, deleted) = {
        let archive = archive.clone();
        let failed = deleted.is_err();
        let slugs = slugs.clone();
        let result = blocking(move || {
            let mut writer = writer;
            if failed {
                for slug in &slugs {
                    archive.delete(&writer, slug);
                }
                writer.commit()?;
            }
            drop(writer);
            archive.reload()
        })
        .await;
        (result, deleted?)
    };
    if let Err(error) = archive_result {
        tracing::warn!(%error, "archive reload after demote failed");
    }

    publish::run(&state, touched, &Reporter::silent()).await;
    tracing::info!(
        admin_id = %admin.id,
        archived = slugs.len(),
        entities_deleted = deleted.0,
        people_deleted = deleted.1,
        "entities moved to archive"
    );
    Ok(Json(DemoteResult {
        archived: slugs,
        skipped,
    }))
}
