//! Холодный архив каталога: фильмы вне горячего слоя в индексе Tantivy (`ARCHIVE_DIR`).
//!
//! Горячий слой — `entities` в PostgreSQL (полные карточки, сайт, Meilisearch): все фильмы с
//! русским названием и 500 самых популярных иностранных. Остальные ~1 млн хранятся только
//! «скелетом» в архиве (название, дата, постер): поиск по названию, страница фильма ([`get`]),
//! возврат в каталог ([`promote`]) и ручной перенос ненужных фильмов в архив ([`demote`]). Раскладку делает
//! [`pipeline`] (`nexus archive dump|build`).

pub mod enrich;
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
use governor::clock::{Clock, DefaultClock};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::Page;
use shared::{AdminUser, AppError, AppResult, AppState};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
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

/// Вернуть фильм из архива на сайт. С `TMDB_API_KEY` — со всеми данными TMDB (описание,
/// постер, титры, жанры, трейлеры; [`enrich`]), TMDB недоступен или ключа нет — «скелетом»:
/// название, оригинальное название, дата выхода, постер. `active_manual` — сборка горячего слоя
/// фильм не уберёт; пометка ручного переноса в архив снимается вместе с документом. Документ удаляется из архива,
/// поиск сайта и статика обновляются.
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
    let data = match fetch(&state, &doc).await {
        Ok(data) => data,
        Err(enrich::TmdbError::Disabled) => None,
        Err(error) => {
            tracing::warn!(%error, %slug, "promote without TMDB data");
            None
        }
    };

    let mut tx = state.db.begin().await?;
    let id = enrich::insert_film(&mut tx, &doc, data.as_ref())
        .await
        .map_err(validate::conflict(
            "slug or tmdb id is already in the catalog",
        ))?;
    tx.commit().await?;
    remove_from_archive(&archive, writer, &slug).await;
    let companies = data.as_ref().map(enrich::companies).unwrap_or_default();
    if let Err(error) = crate::studios::sync(&state.db, Some(&[id]), &companies).await {
        tracing::warn!(%error, "studios sync after promote failed");
    }

    publish::run(
        &state,
        Touched::collect(&state.db, &[id], &[]).await?,
        &Reporter::silent(),
    )
    .await;
    tracing::info!(admin_id = %admin.id, entity_id = %id, %slug, full = data.is_some(), "entity promoted from archive");
    Ok((
        StatusCode::CREATED,
        Json(detail_by_id(&state.db, id).await?),
    ))
}

/// Полный ответ TMDB по фильму архива; не фильм или без TMDB id — `None`.
async fn fetch(
    state: &AppState,
    doc: &ArchiveDoc,
) -> Result<Option<serde_json::Value>, enrich::TmdbError> {
    match doc.tmdb_id {
        Some(tmdb_id) if enrich::is_movie(doc) => enrich::fetch_movie(&state.config, tmdb_id).await,
        _ => Ok(None),
    }
}

/// Ленивое удаление документа (место освободит слияние) и коммит. Не вышло — фильм уже в
/// каталоге, документ уберёт следующая сборка (`active_manual` → `keep`).
async fn remove_from_archive(archive: &Arc<Archive>, writer: tantivy::IndexWriter, slug: &str) {
    let (archive, slug) = (archive.clone(), slug.to_string());
    let removed = blocking({
        let slug = slug.clone();
        move || {
            let mut writer = writer;
            archive.delete(&writer, &slug);
            writer.commit()?;
            archive.reload()
        }
    })
    .await;
    if let Err(error) = removed {
        tracing::warn!(%error, %slug, "returned entity is still in the archive");
    }
}

/// Возвраты открытием страницы: лимит в час на каталог архива (в тестах у каждого свой).
type Limiters = HashMap<(String, u64), Arc<DefaultDirectRateLimiter>>;
static RESTORES: Mutex<Option<Limiters>> = Mutex::new(None);
/// Фильмы, которые сейчас возвращаются: второй запрос того же slug ждёт первого.
static IN_FLIGHT: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);

fn restore_limiter(state: &AppState) -> Option<Arc<DefaultDirectRateLimiter>> {
    let per_hour = NonZeroU32::new(u32::try_from(state.config.archive_restore_per_hour).ok()?)?;
    let key = (
        state.config.archive_dir.clone().unwrap_or_default(),
        state.config.archive_restore_per_hour,
    );
    let mut limiters = RESTORES.lock().unwrap();
    Some(
        limiters
            .get_or_insert_with(HashMap::new)
            .entry(key)
            .or_insert_with(|| Arc::new(RateLimiter::direct(Quota::per_hour(per_hour))))
            .clone(),
    )
}

fn in_flight(slug: &str) -> Arc<tokio::sync::Mutex<()>> {
    IN_FLIGHT
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .entry(slug.to_string())
        .or_default()
        .clone()
}

/// Уже на сайте (вернулся раньше): карточка.
async fn on_site(state: &AppState, slug: &str) -> AppResult<Option<EntityDetail>> {
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM entities WHERE slug = $1 AND active")
        .bind(slug)
        .fetch_optional(&state.db)
        .await?;
    match id {
        Some(id) => Ok(Some(detail_by_id(&state.db, id).await?)),
        None => Ok(None),
    }
}

/// Открыли страницу фильма в архиве — фильм возвращается на сайт со всеми данными TMDB
/// (описание, постер, длительность, страны, студии, возрастной рейтинг, трейлеры, жанры, все
/// актёры и съёмочная группа) и сразу отдаётся полной карточкой: фронтенд показывает её вместо
/// архивной страницы. Кто-то ищет фильм в архиве — значит, ему место на сайте.
///
/// Уже на сайте (вернулся раньше или параллельным запросом) — 200 и карточка. Поиск сайта,
/// известность участников и статика обновляются в фоне. Без авторизации; лимит — сколько фильмов
/// в час возвращается открытием страницы (`ARCHIVE_RESTORE_PER_HOUR`).
#[utoipa::path(
    post, operation_id = "archive_restore", path = "/archive/entities/{slug}/restore",
    tag = "catalog",
    params(("slug" = String, Path, description = "slug фильма в архиве")),
    responses(
        (status = 201, description = "Фильм возвращён на сайт", body = EntityDetail),
        (status = 200, description = "Фильм уже на сайте", body = EntityDetail),
        (status = 404, description = "Нет ни в архиве, ни на сайте", body = ErrorBody),
        (status = 409, description = "`kept_in_archive` — админ убрал фильм в архив вручную; `not_restorable` — не фильм TMDB", body = ErrorBody),
        (status = 429, description = "Лимит возвратов в час исчерпан (`Retry-After`)", body = ErrorBody),
        (status = 503, description = "`restore_unavailable` — нет `TMDB_API_KEY` или лимит 0; `tmdb_unavailable` — TMDB не ответил; архив недоступен", body = ErrorBody),
    )
)]
pub async fn restore(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<(StatusCode, Json<EntityDetail>)> {
    validate::slug("slug", &slug).map_err(|_| AppError::NotFound)?;
    let lock = in_flight(&slug);
    let result = {
        let _guard = lock.lock().await;
        restore_locked(&state, &slug).await
    };
    // Запись снимается последним вышедшим: ждущие держат свою копию Arc.
    let mut flights = IN_FLIGHT.lock().unwrap();
    if let Some(map) = flights.as_mut() {
        if map.get(&slug).is_some_and(|m| Arc::strong_count(m) <= 2) {
            map.remove(&slug);
        }
    }
    result
}

async fn restore_locked(
    state: &AppState,
    slug: &str,
) -> AppResult<(StatusCode, Json<EntityDetail>)> {
    if let Some(card) = on_site(state, slug).await? {
        return Ok((StatusCode::OK, Json(card)));
    }
    let archive = open(state)?;
    let doc = {
        let (archive, slug) = (archive.clone(), slug.to_string());
        blocking(move || archive.get(&slug)).await?
    }
    .ok_or(AppError::NotFound)?;
    let Some(tmdb_id) = doc.tmdb_id.filter(|_| enrich::is_movie(&doc)) else {
        return Err(
            AppError::Conflict("only TMDB films are restored".into()).with_code("not_restorable")
        );
    };
    if doc.manual {
        return Err(
            AppError::Conflict("the film was moved to the archive by an admin".into())
                .with_code("kept_in_archive"),
        );
    }
    let disabled = || {
        AppError::Unavailable("restoring from the archive is disabled".into())
            .with_code("restore_unavailable")
    };
    if state.config.tmdb_api_key.is_none() {
        return Err(disabled());
    }
    let limiter = restore_limiter(state).ok_or_else(disabled)?;
    if let Err(not_until) = limiter.check() {
        let wait = not_until.wait_time_from(DefaultClock::default().now());
        return Err(AppError::TooManyRequests {
            retry_after_secs: wait.as_secs().max(1),
        });
    }

    // TMDB не ответил — фильм остаётся в архиве, страница показывает его как есть.
    let data = enrich::fetch_movie(&state.config, tmdb_id)
        .await
        .map_err(|error| {
            tracing::warn!(%error, %slug, "archive restore: TMDB request failed");
            AppError::Unavailable("TMDB is not available, try again later".into())
                .with_code("tmdb_unavailable")
        })?;
    let mut tx = state.db.begin().await?;
    let id = match enrich::insert_film(&mut tx, &doc, data.as_ref()).await {
        Ok(id) => id,
        // Вернул другой инстанс: отдаём его карточку.
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            drop(tx);
            return match on_site(state, slug).await? {
                Some(card) => Ok((StatusCode::OK, Json(card))),
                None => Err(AppError::Conflict("slug is already taken".into())),
            };
        }
        Err(error) => return Err(error.into()),
    };
    tx.commit().await?;
    // Студии фильма — до карточки: в ней ссылки на их страницы.
    let companies = data.as_ref().map(enrich::companies).unwrap_or_default();
    if let Err(error) = crate::studios::sync(&state.db, Some(&[id]), &companies).await {
        tracing::warn!(%error, "studios sync after restore failed");
    }

    // Документ архива — если писатель свободен (идёт сборка — уберёт она).
    let writer = {
        let archive = archive.clone();
        blocking(move || archive.writer()).await
    };
    match writer {
        Ok(writer) => remove_from_archive(&archive, writer, slug).await,
        Err(error) => {
            tracing::info!(%error, %slug, "restored entity stays in the archive until the next build")
        }
    }

    // Поиск, известность участников и статика — в фоне: карточка уже готова.
    let background = state.clone();
    tokio::spawn(async move {
        match Touched::collect(&background.db, &[id], &[]).await {
            Ok(touched) => {
                publish::run(&background, touched, &Reporter::silent()).await;
            }
            Err(error) => tracing::warn!(%error, %id, "restored entity is not published"),
        }
    });
    tracing::info!(entity_id = %id, slug, full = data.is_some(), "entity restored from archive by a visitor");
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
/// PostgreSQL (каскадом титры, теги, статистика; люди без других работ — тоже). Документ помечен
/// `manual`: сборка горячего слоя такой фильм не вернёт, импорт пропустит (TMDB id есть в архиве). Поиск сайта и статика обновляются. Вернуть — `POST /admin/archive/promote/{slug}`
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
                (e.external_ids ->> 'tmdb')::int AS tmdb_id, s.tmdb_popularity AS popularity
         FROM entities e
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
            manual: true,
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

    let deleted = async {
        let mut tx = state.db.begin().await?;
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
