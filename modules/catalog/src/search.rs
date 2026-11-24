//! Поиск по каталогу на Tantivy.
//!
//! Индексы: `works` ([`crate::works`]: фильмы, сериалы, игры, картины), `people`
//! ([`crate::people_idx`]) и `books` ([`crate::books`]); лежат под `SEARCH_INDEX_DIR`. Источник
//! правды — PostgreSQL, индексы производные и нужны только для поиска: карточки
//! (`GET /entities/{slug}`, `GET /people/{slug}`) читаются из БД. Документ компактный
//! ([`SearchDoc`]): без карточки, описания и участников.
//!
//! После каждой записи в админке затронутые сущности переписываются в индексы
//! ([`update_index`] из [`crate::publish::run`]). Ошибка индекса запись не отменяет, только
//! пишется в лог и в поток публикации.
//!
//! **Перестройка** ([`reindex`]) — без простоя: каждый индекс строится в новое поколение
//! каталога `<name>.v<ms>` и переключается атомарно (`<name>.current`, хранятся два поколения):
//! 1. работы из БД пачками; новый индекс не меньше `SEARCH_REINDEX_MIN_PERCENT`% текущего
//!    (80, в dev — 0), иначе поколение не переключается; `force` — пропустить;
//! 2. известность людей и все публичные люди — так же;
//! 3. фронтенду — пересборка всей статики (`{"all": true}`), только при ручном запуске.
//!
//! Перестройка одна за раз: в процессе — флаг, между инстансами — блокировка в Redis. Правки во
//! время перестройки повторяются после переключения.

use crate::entities::check_year;
use crate::jobs::{self, DoneEvent, JobEvent, Reporter, Step, StepStatus};
use crate::models::{
    page_bounds, CountryHit, Entity, EntityKind, EntitySummary, FacetQuery, IndexEntities,
    IndexResult, Neighbors, Page, PublisherHit, ReindexQuery, ReindexResult, ReleaseStatus,
    SearchQuery, SearchSort, StudioHit, StudiosQuery, TagHit, ENTITY_STATS_JOIN, IN_CATALOG,
    VISIBLE,
};
use crate::people_search;
use crate::publish::Touched;
use crate::validate;
use axum::extract::State;
use axum::response::Response;
use axum::Json;
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use serde::Serialize;
use serde_json::Value;
use shared::directory::RatingStats;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query, Repeated};
use shared::{AdminUser, AppError, AppResult, AppState};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Сколько сущностей читать из БД и отправлять одним запросом при перестройке.
pub const BATCH: usize = 500;

/// Перестройка по расписанию: одна на все инстансы за интервал.
const SCHEDULE_LOCK: &str = "catalog:reindex-lock";
/// Идёт перестройка (любая): одна на все инстансы. TTL — на случай падения процесса.
const RUNNING_LOCK: &str = "catalog:reindex-running";
/// Короткий TTL, который работающая перестройка продлевает каждые [`RUNNING_LOCK_RENEW`]: процесс
/// убит посреди работы (перезапуск, деплой) — блокировка сама снимется через 2 мин, а не держит
/// перестройку и операции админки («уже идёт», 409) часами.
const RUNNING_LOCK_TTL: Duration = Duration::from_secs(120);
const RUNNING_LOCK_RENEW: Duration = Duration::from_secs(30);

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Документ индекса — компактный (решение 2026-10-14): то, по чему ищут (`title`,
/// `original_title`, `studios`), что показывает выдача (`EntitySummary`: `slug`, постер, дата,
/// статус выхода, рейтинг IMDb), фильтры (`kind`, `tags`, `year`, `studios`, `release_ts`,
/// `slug`) и сортировки. Карточки,
/// описания, имён участников и названий тегов в индексе нет: карточка читается из БД, а полный
/// состав (миллионы титров) делал перестройку в разы дольше.
#[derive(Debug, Serialize)]
pub(crate) struct SearchDoc {
    id: Uuid,
    kind: EntityKind,
    slug: String,
    title: String,
    original_title: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    /// Размеры файла обложки: выдача ставит карточке `aspect-ratio` до загрузки картинки.
    cover_width: Option<i32>,
    cover_height: Option<i32>,
    /// Для фильтров `year = 2021`, `year 2000 TO 2010`.
    year: Option<i32>,
    /// `release_date` в unix-секундах (полночь UTC): сортировки `new` / `soon` и их фильтр
    /// «вышел / не вышел» (для сортировок и фильтра «вышел / не вышел»).
    release_ts: Option<i64>,
    /// Все студии из `metadata.studios`: поиск («dreamworks»), фильтр `studio`, подсказки.
    studios: Vec<String>,
    /// Издатели: `metadata.publisher` книги, `publisher` и `developer` игры — у них студий нет,
    /// а фильтр раздела тот же по смыслу. Фильтр `publisher`, подсказки `GET /search/publishers`.
    publishers: Vec<String>,
    /// Музей или собрание картины (`metadata.museum`): фильтр `museum`, подсказки
    /// `GET /search/museums`.
    museums: Vec<String>,
    /// Страны производства (`metadata.countries`), коды ISO 3166-1 alpha-2: фильтр `country`,
    /// подсказки `GET /search/countries`. Названия стран показывает фронтенд.
    countries: Vec<String>,
    /// Slug'и тегов: фильтр.
    tags: Vec<String>,
    /// Статистика (`entity_stats`): сортировки `recommended` (`rank_score`), `trending`
    /// (`popularity`), `top_rated` (`quality`); рейтинг IMDb и статус выхода — для выдачи.
    rank_score: Option<f64>,
    popularity: Option<f64>,
    quality: Option<f64>,
    imdb_rating: Option<f64>,
    imdb_votes: Option<i32>,
    release_status: ReleaseStatus,
    /// Оценки Nexus (`entity_ratings`, сводки от `social`): сортировки `popular` (число) и
    /// `rating` (средняя; без оценок — `null`, в конце).
    nexus_ratings_count: i64,
    nexus_rating_avg: Option<f64>,
    /// Slug'и страниц студий из `studios` (по названиям `metadata.studios`): фильтр
    /// `studio_slug`. Студии без строки в `studios` здесь нет.
    studio_slugs: Vec<String>,
    /// Награды (`award_nominations`): фильтр «есть „Оскар“» (`award_slugs`), фасет «лауреаты
    /// 2024» (`award_years`) и сортировка по числу наград (`award_wins`).
    #[serde(flatten)]
    awards: crate::awards::AwardFacets,
}

impl SearchDoc {
    fn new(
        entity: Entity,
        tags: Vec<String>,
        ratings: Option<RatingStats>,
        studio_slugs: &HashMap<String, String>,
        awards: crate::awards::AwardFacets,
    ) -> Self {
        let ratings = ratings.unwrap_or(RatingStats {
            average: None,
            count: 0,
        });
        let studios: Vec<String> = entity.metadata["studios"]
            .as_array()
            .map(|studios| {
                studios
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let studio_slugs = studios
            .iter()
            .filter_map(|name| studio_slugs.get(name).cloned())
            .collect();
        // Издатель книги и издатель с разработчиком игры — по одному полю, без повторов.
        let mut publishers: Vec<String> = Vec::new();
        for key in ["publisher", "developer"] {
            if let Some(value) = entity.metadata[key].as_str() {
                let value = value.trim();
                if !value.is_empty() && !publishers.iter().any(|p| p == value) {
                    publishers.push(value.to_owned());
                }
            }
        }
        // В БД коды уже в верхнем регистре (`metadata::validate`), но индекс строится и по
        // старым строкам — приводим здесь, иначе фильтр `country=US` их не найдёт.
        let countries: Vec<String> = entity.metadata["countries"]
            .as_array()
            .map(|codes| {
                codes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|code| code.trim().to_ascii_uppercase())
                    .filter(|code| !code.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let museums = entity.metadata["museum"]
            .as_str()
            .map(str::trim)
            .filter(|museum| !museum.is_empty())
            .map(|museum| vec![museum.to_owned()])
            .unwrap_or_default();
        Self {
            id: entity.id,
            kind: entity.kind,
            year: entity.release_date.map(|date| date.year()),
            release_ts: entity.release_date.map(day_ts),
            slug: entity.slug,
            title: entity.title,
            original_title: entity.original_title,
            release_date: entity.release_date,
            cover_url: entity.cover_url,
            cover_width: entity.cover_width,
            cover_height: entity.cover_height,
            studios,
            publishers,
            museums,
            countries,
            tags,
            rank_score: entity.rank_score,
            popularity: entity.tmdb_popularity,
            quality: entity.quality,
            imdb_rating: entity.imdb_rating,
            imdb_votes: entity.imdb_votes,
            release_status: entity.release_status,
            nexus_ratings_count: ratings.count,
            nexus_rating_avg: ratings.average,
            studio_slugs,
            awards,
        }
    }
}

/// Дата → unix-секунды её полуночи (UTC).
fn day_ts(date: NaiveDate) -> i64 {
    date.and_time(NaiveTime::MIN).and_utc().timestamp()
}

/// Документы пачки сущностей: строки уже прочитаны, теги, оценки Nexus и slug'и студий — по
/// запросу на пачку. Участников в документе нет: фильтр `person` — через [`person_filter`].
pub(crate) async fn docs_of(
    db: &sqlx::PgPool,
    entities: Vec<Entity>,
) -> Result<Vec<SearchDoc>, BoxError> {
    let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT et.entity_id, t.slug FROM entity_tags et JOIN tags t ON t.id = et.tag_id
         WHERE et.entity_id = ANY($1) ORDER BY t.slug",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut tags: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (id, slug) in rows {
        tags.entry(id).or_default().push(slug);
    }
    let rows: Vec<(Uuid, Option<f64>, i64)> = sqlx::query_as(
        "SELECT entity_id, average, count FROM entity_ratings WHERE entity_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut ratings: HashMap<Uuid, RatingStats> = rows
        .into_iter()
        .map(|(id, average, count)| (id, RatingStats { average, count }))
        .collect();
    let names: Vec<String> = entities
        .iter()
        .flat_map(|entity| crate::studios::names_of(&entity.metadata))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let studio_slugs: HashMap<String, String> =
        sqlx::query_as("SELECT name, slug FROM studios WHERE name = ANY($1)")
            .bind(&names)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    let mut awards = crate::awards::facets_of(db, &ids).await?;
    Ok(entities
        .into_iter()
        .map(|entity| {
            let tags = tags.remove(&entity.id).unwrap_or_default();
            let ratings = ratings.remove(&entity.id);
            let awards = awards.remove(&entity.id).unwrap_or_default();
            SearchDoc::new(entity, tags, ratings, &studio_slugs, awards)
        })
        .collect())
}

/// Переотправляет затронутое в индекс (и в теневой, если идёт перестройка) и ждёт применения;
/// удалённое из БД и невидимое удаляет из индекса. Вызывается из [`crate::publish::run`] после
/// коммита.
pub(crate) async fn update_index(state: &AppState, touched: &Touched) -> Result<(), BoxError> {
    update_entities(state, touched).await?;
    people_search::update_index(state, &touched.people).await
}

async fn update_entities(state: &AppState, touched: &Touched) -> Result<(), BoxError> {
    if touched.entities.is_empty() {
        return Ok(());
    }
    // Книги и работы — каждые в свой индекс Tantivy; ошибка одного не мешает другому.
    let books = crate::books::sync::update(state, &touched.entities).await;
    let works = crate::works::sync::update(state, &touched.entities).await;
    books.and(works)
}

// ---------------------------------------------------------------- перестройка

/// Как перестраивать.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReindexOptions {
    /// Переключить поиск, даже если новый индекс меньше `search_reindex_min_percent`% текущего.
    pub force: bool,
    /// Пересобрать всю статику фронтенда в конце. При старте и по расписанию — нет.
    pub revalidate: bool,
}

impl ReindexOptions {
    /// Шаги для `plan`.
    pub fn steps(self) -> Vec<Step> {
        let mut steps = vec![Step::Works, Step::People];
        if self.revalidate {
            steps.push(Step::Isr);
        }
        steps
    }
}

#[derive(Debug)]
pub enum ReindexError {
    Disabled,
    /// Уже идёт перестройка (в этом процессе или на другом инстансе).
    Busy,
    /// Новый индекс слишком мал: поиск не переключён.
    TooSmall {
        /// `entities` или `people`.
        index: &'static str,
        new: u64,
        current: u64,
        /// Порог, `search_reindex_min_percent`.
        min: u64,
    },
    Failed(String),
}

impl fmt::Display for ReindexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "search is disabled"),
            Self::Busy => write!(f, "reindex is already running"),
            Self::TooSmall {
                index,
                new,
                current,
                min,
            } => write!(
                f,
                "new {index} index has {new} documents, current has {current}: less than \
                 {min}%, search was not switched (use force=true if intended)"
            ),
            Self::Failed(error) => write!(f, "reindex failed: {error}"),
        }
    }
}

impl From<ReindexError> for AppError {
    fn from(error: ReindexError) -> Self {
        match error {
            ReindexError::Disabled => unavailable(),
            ReindexError::Busy | ReindexError::TooSmall { .. } => {
                AppError::Conflict(error.to_string())
            }
            ReindexError::Failed(_) => {
                tracing::warn!(%error, "search reindex failed");
                unavailable()
            }
        }
    }
}

/// Идёт ли перестройка в этом процессе.
static RUNNING: Mutex<bool> = Mutex::new(false);

pub(crate) fn unavailable() -> AppError {
    AppError::Unavailable("search is temporarily unavailable".into())
}

/// Право на перестройку. Снимать через [`ReindexLock::release`]; в процессе снимается и при drop.
pub struct ReindexLock {
    redis: bool,
    /// Продление блокировки в Redis, пока она взята.
    renew: Option<tokio::task::JoinHandle<()>>,
}

impl ReindexLock {
    pub async fn acquire(state: &AppState) -> Result<Self, ReindexError> {
        if state.config.search_index_dir.is_none() {
            return Err(ReindexError::Disabled);
        }
        {
            let mut running = RUNNING.lock().unwrap();
            if *running {
                return Err(ReindexError::Busy);
            }
            *running = true;
        }
        let mut lock = Self {
            redis: false,
            renew: None,
        };
        match state.cache.try_lock(RUNNING_LOCK, RUNNING_LOCK_TTL).await {
            Ok(true) => {
                lock.redis = true;
                let cache = state.cache.clone();
                lock.renew = Some(tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(RUNNING_LOCK_RENEW).await;
                        match cache.extend_lock(RUNNING_LOCK, RUNNING_LOCK_TTL).await {
                            Ok(true) => {}
                            Ok(false) => tracing::warn!("reindex lock expired while running"),
                            Err(error) => tracing::warn!(%error, "reindex lock renewal failed"),
                        }
                    }
                }));
            }
            Ok(false) => return Err(ReindexError::Busy),
            // Без Redis не узнать про другие инстансы: перестраиваем, как раньше.
            Err(error) => tracing::warn!(%error, "reindex lock unavailable, rebuilding anyway"),
        }
        Ok(lock)
    }

    pub async fn release(mut self, state: &AppState) {
        if let Some(renew) = self.renew.take() {
            renew.abort();
        }
        if std::mem::take(&mut self.redis) {
            state.cache.unlock(RUNNING_LOCK).await;
        }
    }
}

impl Drop for ReindexLock {
    fn drop(&mut self) {
        if let Some(renew) = self.renew.take() {
            renew.abort();
        }
        *RUNNING.lock().unwrap() = false;
    }
}

/// Перестройка целиком: блокировка, шаги, снятие блокировки.
pub async fn reindex(
    state: &AppState,
    options: ReindexOptions,
    report: &Reporter,
) -> Result<ReindexResult, ReindexError> {
    let lock = ReindexLock::acquire(state).await?;
    let result = reindex_locked(state, &lock, options, report).await;
    lock.release(state).await;
    result
}

/// Имена поколений индексов в корне (`works.v<ms>`, `people.v<ms>`).
fn generation_names(root: &std::path::Path) -> HashSet<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.contains(".v"))
        .collect()
}

/// Ошибка шага перестройки: слишком маленький индекс или сбой.
fn step_error(
    index: &'static str,
    error: crate::works::sync::BuildError,
    current: u64,
    min: u64,
) -> ReindexError {
    match error {
        crate::works::sync::BuildError::TooSmall { new } => ReindexError::TooSmall {
            index,
            new,
            current,
            min,
        },
        crate::works::sync::BuildError::Other(error) => ReindexError::Failed(error.to_string()),
    }
}

/// Шаги перестройки (см. описание модуля): работы, люди, статика. Каждый сообщает о себе в
/// `report`; упавший шаг — `failed` и ошибка, следующие не выполняются.
pub async fn reindex_locked(
    state: &AppState,
    _lock: &ReindexLock,
    options: ReindexOptions,
    report: &Reporter,
) -> Result<ReindexResult, ReindexError> {
    let Some(root) = state.config.search_index_dir.as_deref() else {
        return Err(ReindexError::Disabled);
    };
    let root = std::path::Path::new(root);
    let min = state.config.search_reindex_min_percent;
    let before = generation_names(root);
    let previous_version = shared::tantivy::generations::current(root, "works")
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let previous_count = crate::works::sync::doc_count(state);
    let people_before = crate::people_idx::sync::doc_count(state);

    // 1. Работы. Перед ними — студии для новых названий в `metadata.studios`.
    let started = report.start(Step::Works);
    let studios_started = Instant::now();
    match crate::studios::sync(&state.db, None, &[]).await {
        Ok(created) => {
            tracing::info!(created, elapsed = ?studios_started.elapsed(), "studios synced")
        }
        Err(error) => tracing::warn!(%error, "studios sync failed"),
    }
    let indexed = match crate::works::sync::rebuild_with(state, report, options.force).await {
        Ok(count) => count,
        Err(error) => {
            report.finish(Step::Works, StepStatus::Failed, error.to_string(), started);
            return Err(step_error("works", error, previous_count, min));
        }
    };
    report.finish(
        Step::Works,
        StepStatus::Done,
        format!("Проиндексировано {indexed} работ"),
        started,
    );

    // 2. Люди: известность и все публичные люди.
    let started = report.start(Step::People);
    let people_indexed =
        match crate::people_idx::sync::rebuild_with(state, report, options.force).await {
            Ok(count) => count,
            Err(error) => {
                report.finish(Step::People, StepStatus::Failed, error.to_string(), started);
                return Err(step_error("people", error, people_before, min));
            }
        };
    report.finish(
        Step::People,
        StepStatus::Done,
        format!("Проиндексировано {people_indexed} человек"),
        started,
    );

    // 3. Статика.
    if options.revalidate {
        let started = report.start(Step::Isr);
        if !state.isr.is_enabled() {
            report.finish(
                Step::Isr,
                StepStatus::Skipped,
                "ISR не настроен (ISR_URL, ISR_SECRET)",
                started,
            );
        } else {
            match state.isr.revalidate_all().await {
                Ok(()) => report.finish(
                    Step::Isr,
                    StepStatus::Done,
                    "Пересборка всей статики HTML/JSON запущена (идёт в фоне на фронтенде)",
                    started,
                ),
                // Поиск уже переключён: перестройка удалась, упала только статика.
                Err(error) => {
                    tracing::warn!(%error, "isr full revalidation failed");
                    report.finish(
                        Step::Isr,
                        StepStatus::Failed,
                        format!("Пересборка статики не запущена: {error}"),
                        started,
                    );
                }
            }
        }
    }

    let after = generation_names(root);
    let mut deleted: Vec<String> = before.difference(&after).cloned().collect();
    deleted.sort();
    Ok(ReindexResult {
        indexed: indexed as usize,
        people_indexed: people_indexed as usize,
        previous_count,
        previous_version,
        deleted,
    })
}

/// Перестройка в фоне: сразу при старте приложения, затем каждые
/// `SEARCH_REINDEX_INTERVAL_SECS` (если не 0). По расписанию перестраивает только один инстанс:
/// блокировка в Redis. Статику фронтенда не трогает.
pub fn spawn_reindex(state: AppState) {
    if state.config.search_index_dir.is_none() {
        return;
    }
    let interval = Duration::from_secs(state.config.search_reindex_interval_secs);
    tokio::spawn(async move {
        // Индексы уже построены (диск сохраняется между запусками; пустые строит `spawn_initial`):
        // полная перестройка при каждом рестарте — десятки минут нагрузки впустую.
        if crate::works::sync::doc_count(&state) == 0
            || crate::people_idx::sync::doc_count(&state) == 0
        {
            run_reindex(&state, "startup").await;
        }
        if interval.is_zero() {
            return;
        }
        // Блокировка чуть короче интервала: к следующему разу она точно истечёт.
        let lock_ttl = interval
            .saturating_sub(Duration::from_secs(60))
            .max(Duration::from_secs(60));
        loop {
            tokio::time::sleep(interval).await;
            match state.cache.try_lock(SCHEDULE_LOCK, lock_ttl).await {
                Ok(true) => run_reindex(&state, "scheduled").await,
                Ok(false) => tracing::debug!("scheduled reindex is running on another instance"),
                // Без Redis не узнать, перестраивает ли кто-то ещё: лишняя перестройка безопасна.
                Err(error) => {
                    tracing::warn!(%error, "reindex lock unavailable, rebuilding anyway");
                    run_reindex(&state, "scheduled").await;
                }
            }
        }
    });
}

async fn run_reindex(state: &AppState, reason: &str) {
    match reindex(state, ReindexOptions::default(), &Reporter::silent()).await {
        Ok(result) => tracing::info!(indexed = result.indexed, reason, "search index rebuilt"),
        Err(ReindexError::Busy) => tracing::info!(reason, "reindex is already running"),
        Err(error) => tracing::warn!(%error, reason, "search index rebuild failed"),
    }
}

/// Как часто оценки Nexus доезжают до индекса ([`spawn_ratings_sync`]).
const RATINGS_SYNC_INTERVAL: Duration = Duration::from_secs(60);
/// Нахлёст окна: сводка, записанная транзакцией, начатой до прошлого прохода, не теряется.
const RATINGS_SYNC_OVERLAP_SECS: i64 = 60;

/// Оценки Nexus в индексе: раз в [`RATINGS_SYNC_INTERVAL`] переотправляет документы сущностей,
/// чьи сводки (`entity_ratings.updated_at`) изменились с прошлого прохода; первый проход — все
/// сводки. Оценки пишет `social` через `EntityRatings` (после каждой рецензии и при старте),
/// модулю `social` о поиске знать не нужно. Повтор безопасен: документы строятся целиком из БД.
pub fn spawn_ratings_sync(state: AppState) {
    if state.config.search_index_dir.is_none() {
        return;
    }
    tokio::spawn(async move {
        let mut since = None;
        loop {
            tokio::time::sleep(RATINGS_SYNC_INTERVAL).await;
            match sync_ratings(&state, since).await {
                Ok((next, count)) => {
                    since = Some(next);
                    if count > 0 {
                        tracing::info!(entities = count, "nexus ratings synced to search");
                    }
                }
                Err(error) => tracing::warn!(%error, "nexus ratings search sync failed"),
            }
        }
    });
}

/// Один проход [`spawn_ratings_sync`]: сводки, изменённые после `since` (`None` — все), → индекс.
/// Возвращает начало следующего окна и число переотправленных сущностей.
pub async fn sync_ratings(
    state: &AppState,
    since: Option<DateTime<Utc>>,
) -> Result<(DateTime<Utc>, usize), BoxError> {
    let next: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&state.db)
        .await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM entity_ratings WHERE $1::timestamptz IS NULL OR updated_at > $1",
    )
    .bind(since)
    .fetch_all(&state.db)
    .await?;
    for chunk in ids.chunks(1000) {
        update_entities(state, &Touched::entities(chunk)).await?;
    }
    Ok((
        next - chrono::Duration::seconds(RATINGS_SYNC_OVERLAP_SECS),
        ids.len(),
    ))
}

/// Полнотекстовый поиск по активным сущностям каталога: опечатки, ранжирование, фильтры. Весь
/// поиск на сайте идёт сюда (в `GET /entities` поиска по названию нет).
///
/// Фильтры (через AND): `tag`, год (`year` — ровно, `year_from` / `year_to` — диапазон
/// включительно), `studio` (точное название, без учёта регистра), `publisher` (издательство книги,
/// издатель или разработчик игры — тоже точное название), `museum` (музей картины),
/// `person` (slug участника) и
/// `role` (его роль в этой сущности; без `person` — 400), `award` (slug премии), `award_year`
/// (год церемонии), `awarded` (только с наградой) и `country` (страна производства, код
/// ISO 3166-1 alpha-2; несколько — ИЛИ между собой).
///
/// Сортировка по релевантности; `sort=recommended` / `trending` / `top_rated` — по баллу
/// «Рекомендуемые», TMDB popularity, рейтингу IMDb (у кого значения нет — в конце); `new` — только
/// вышедшие, новые сверху; `soon` — только не вышедшие, ближайшие сверху; `title` — по названию;
/// `awarded` — больше наград сверху; `random` — случайное окно выдачи (`limit=1` — одна случайная
/// работа под те же фильтры: ею заканчивается листание, когда ряд и похожие кончились).
///
/// Соседей работы в этой же выдаче отдаёт `GET /entities/{slug}/neighbors`.
///
/// Индекс обновляется после изменений в админке с задержкой в доли секунды.
#[utoipa::path(
    get, path = "/search", tag = "catalog",
    params(
        SearchQuery,
        ("kind" = Option<Vec<EntityKind>>, Query,
         description = "Типы работ: можно повторить (`kind=movie&kind=game`) или перечислить \
                        через запятую. Несколько — ИЛИ между собой и И с остальными фильтрами. \
                        Не передан — все типы", explode = true),
        ("country" = Option<Vec<String>>, Query,
         description = "Страны производства (`metadata.countries`), коды ISO 3166-1 alpha-2: \
                        можно повторить (`country=US&country=GB`) или перечислить через запятую. \
                        Несколько — ИЛИ между собой и И с остальными фильтрами. Регистр не важен; \
                        неизвестный код — 400", explode = true, example = "US"),
    ),
    responses(
        (status = 200, description = "Страница результатов. `total` — всего найдено (точно)", body = Page<EntitySummary>),
        (status = 400, description = "Неверный фильтр", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    repeated: Repeated,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Page<EntitySummary>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    if let Some(books) = books_query(&state, &repeated, &query)? {
        let random = matches!(query.sort, Some(SearchSort::Random));
        return Ok(Json(
            crate::books::search::search(&state, books, random, limit, offset).await?,
        ));
    }
    if let Some(page) = blended(&state, &repeated, &query, limit, offset).await? {
        return Ok(Json(page));
    }
    // Ни одной работы (или нет такого человека) — искать нечего.
    let Some(works) = works_query(&state, &repeated, &query).await? else {
        return Ok(Json(Page {
            items: Vec::new(),
            total: 0,
            limit,
            offset,
        }));
    };
    let random = matches!(query.sort, Some(SearchSort::Random));
    Ok(Json(
        crate::works::search::search(&state, works, random, limit, offset).await?,
    ))
}

/// Запрос к книжному индексу, если отвечать должен он: запрос только про книги (`kind=book`) и
/// книжный поиск включён ([`crate::books::search::routed`]). Внешний `None` — отвечает
/// индекс; внутренний — под фильтры у книг заведомо ничего нет. Параметры проверяются теми
/// же правилами, что и раньше (неверный фильтр — 400).
fn books_query(
    state: &AppState,
    repeated: &Repeated,
    query: &SearchQuery,
) -> AppResult<Option<Option<crate::books::index::BookQuery>>> {
    let kinds = kinds_of(repeated)?;
    if !crate::books::search::routed(state, &kinds) {
        return Ok(None);
    }
    let countries = countries_of(repeated)?;
    filters_of(query)?;
    Ok(Some(crate::books::search::query_of(query, &countries)))
}

/// Сколько работ идёт в общей выдаче перед каждой книгой ([`blended`]).
const BLEND_OTHERS_PER_BOOK: usize = 2;

/// Порядок общей выдачи двух индексов: `true` — очередная работа, `false` —
/// книга. Чередование «две работы — книга», пока есть обе стороны, дальше — что осталось.
/// Возвращает порядок страницы и сколько работ каждой стороны стоит перед ней.
fn blend_order(
    others: usize,
    books: usize,
    limit: usize,
    offset: usize,
) -> (Vec<bool>, usize, usize) {
    let (mut o, mut b) = (0usize, 0usize);
    let (mut before_o, mut before_b) = (0, 0);
    let mut order = Vec::with_capacity(limit);
    while o + b < offset + limit && (o < others || b < books) {
        let position = o + b;
        if position == offset {
            (before_o, before_b) = (o, b);
        }
        let take_other = if o >= others {
            false
        } else if b >= books {
            true
        } else {
            // В каждой тройке позиций третья — книга.
            position % (BLEND_OTHERS_PER_BOOK + 1) < BLEND_OTHERS_PER_BOOK
        };
        if position >= offset {
            order.push(take_other);
        }
        if take_other {
            o += 1;
        } else {
            b += 1;
        }
    }
    if order.is_empty() {
        (before_o, before_b) = (o, b);
    }
    (order, before_o, before_b)
}

async fn blended(
    state: &AppState,
    repeated: &Repeated,
    query: &SearchQuery,
    limit: i64,
    offset: i64,
) -> AppResult<Option<Page<EntitySummary>>> {
    if !crate::books::sync::enabled(state) || matches!(query.sort, Some(SearchSort::Random)) {
        return Ok(None);
    }
    let kinds = kinds_of(repeated)?;
    if !(kinds.is_empty() || kinds.contains(&EntityKind::Book)) {
        return Ok(None);
    }
    let countries = countries_of(repeated)?;
    let Some(books) = crate::books::search::query_of(query, &countries) else {
        return Ok(None);
    };
    // Проверка параметров (400) — как у обычного пути.
    let works = works_query(state, repeated, query).await?;
    let books_total = crate::books::search::count(state, books.clone()).await?;
    if books_total == 0 {
        return Ok(None);
    }
    // Первая страница работ — одним проходом по индексу (и итог для чередования), как у подсказок
    // шапки; дальше страницы запрашиваются со смещением.
    let (first_items, others_total) = match &works {
        Some(works) => {
            let page = crate::works::search::search(state, works.clone(), false, limit, 0).await?;
            (page.items, page.total)
        }
        None => (Vec::new(), 0),
    };
    let (order, others_offset, books_offset) = blend_order(
        others_total as usize,
        books_total as usize,
        limit as usize,
        offset as usize,
    );
    let others_limit = order.iter().filter(|other| **other).count();
    let books_limit = order.len() - others_limit;
    let mut others: std::vec::IntoIter<EntitySummary> = match works {
        Some(_) if others_limit > 0 && others_offset == 0 => {
            let mut items = first_items;
            items.truncate(others_limit);
            items.into_iter()
        }
        Some(works) if others_limit > 0 => crate::works::search::search(
            state,
            works,
            false,
            others_limit as i64,
            others_offset as i64,
        )
        .await?
        .items
        .into_iter(),
        _ => Vec::new().into_iter(),
    };
    let mut books = if books_limit > 0 {
        crate::books::search::search(
            state,
            Some(books),
            false,
            books_limit as i64,
            books_offset as i64,
        )
        .await?
        .items
        .into_iter()
    } else {
        Vec::new().into_iter()
    };
    let items = order
        .into_iter()
        .filter_map(|other| if other { others.next() } else { books.next() })
        .collect();
    Ok(Some(Page {
        items,
        total: others_total + books_total as i64,
        limit,
        offset,
    }))
}

/// Случайное число 0..`n` (`n` > 0). Отдельной зависимости ради выбора одной карточки не нужно:
/// наносекунды часов, перемешанные splitmix64, — этого хватает.
pub(crate) fn random_below(n: i64) -> i64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x % n.max(1) as u64) as i64
}

/// Предыдущая и следующая работа в той же выдаче, что отдаёт `GET /search`: листание на карточке
/// идёт по тому списку, откуда человек пришёл (художник, тег, музей, поиск).
///
/// Фильтры, запрос и сортировка — ровно как у `GET /search` (`limit` и `offset` не нужны и
/// игнорируются), поэтому порядок совпадает со списком. `sort=random` — 400: у случайного
/// порядка соседей нет.
///
/// Работы нет в выдаче (фильтры ей не подходят, слишком длинный ряд) — `{"prev": null,
/// "next": null}`, а не 404: фронт в этом случае показывает похожие.
///
/// Позиции и длины ряда здесь нет намеренно: индекс не отдаёт «номер документа в
/// выдаче», а счётчика «14 из 142» в интерфейсе нет.
#[utoipa::path(
    get, path = "/entities/{slug}/neighbors", tag = "catalog",
    operation_id = "entity_neighbors",
    params(
        ("slug" = String, Path, description = "Slug работы"),
        SearchQuery,
        ("kind" = Option<Vec<EntityKind>>, Query,
         description = "Типы работ, как в `GET /search`", explode = true),
        ("country" = Option<Vec<String>>, Query,
         description = "Страны производства, как в `GET /search`", explode = true),
    ),
    responses(
        (status = 200, description = "Соседи; на краях ряда — `null`", body = Neighbors),
        (status = 400, description = "Неверный фильтр или `sort=random`", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn neighbors(
    State(state): State<AppState>,
    repeated: Repeated,
    Path(slug): Path<String>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Neighbors>> {
    if matches!(query.sort, Some(SearchSort::Random)) {
        return Err(AppError::BadRequest(
            "sort=random has no neighbors".to_owned(),
        ));
    }
    if let Some(books) = books_query(&state, &repeated, &query)? {
        return Ok(Json(
            crate::books::search::neighbors(&state, books, slug).await?,
        ));
    }
    let Some(works) = works_query(&state, &repeated, &query).await? else {
        return Ok(Json(Neighbors {
            prev: None,
            next: None,
        }));
    };
    Ok(Json(
        crate::works::search::neighbors(&state, works, slug).await?,
    ))
}

/// Похожие для `GET /entities/{slug}/similar`: видимые того же типа, у которых есть хотя бы один из
/// тегов `tags`, кроме `exclude` (сама сущность и связанные); сверху — по баллу «Рекомендуемые»,
/// затем новее.
pub(crate) async fn similar(
    state: &AppState,
    kind: EntityKind,
    tags: &[String],
    exclude: &[String],
    limit: i64,
) -> AppResult<(Vec<EntitySummary>, i64)> {
    if crate::books::search::routed(state, &[kind]) {
        return crate::books::search::similar(state, tags, exclude, limit).await;
    }
    crate::works::search::similar(state, kind.as_str(), tags, exclude, limit).await
}

/// Длина значения фильтра `studio` и запроса подсказок.
const MAX_STUDIO: usize = 200;

/// Проверка параметров запроса (неверный фильтр — 400). Сами фильтры строит [`works_query`].
fn filters_of(query: &SearchQuery) -> AppResult<()> {
    if let Some(form) = &query.book_form {
        if crate::books::search::parse_forms(form).is_none() {
            return Err(AppError::BadRequest(
                "book_form: book, collection, article, document or all".into(),
            ));
        }
    }
    if let Some(tag) = &query.tag {
        validate::slug("tag", tag)?;
    }
    match (query.year, query.year_from, query.year_to) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(AppError::BadRequest(
                "year cannot be combined with year_from / year_to".into(),
            ))
        }
        (Some(year), None, None) => {
            check_year(year)?;
        }
        (None, from, to) => {
            for year in [from, to].into_iter().flatten() {
                check_year(year)?;
            }
            match (from, to) {
                (Some(from), Some(to)) if from > to => {
                    return Err(AppError::BadRequest(
                        "year_from must not be greater than year_to".into(),
                    ))
                }
                _ => {}
            }
        }
    }
    if let Some(studio) = &query.studio {
        let studio = studio.trim();
        if studio.is_empty() || studio.chars().count() > MAX_STUDIO {
            return Err(AppError::BadRequest(format!(
                "studio must be 1-{MAX_STUDIO} characters"
            )));
        }
    }
    if let Some(publisher) = &query.publisher {
        let publisher = publisher.trim();
        if publisher.is_empty() || publisher.chars().count() > MAX_STUDIO {
            return Err(AppError::BadRequest(format!(
                "publisher must be 1-{MAX_STUDIO} characters"
            )));
        }
    }
    if let Some(museum) = &query.museum {
        let museum = museum.trim();
        if museum.is_empty() || museum.chars().count() > MAX_STUDIO {
            return Err(AppError::BadRequest(format!(
                "museum must be 1-{MAX_STUDIO} characters"
            )));
        }
    }
    if let Some(studio) = &query.studio_slug {
        validate::slug("studio_slug", studio)?;
    }
    if let Some(award) = &query.award {
        validate::slug("award", award)?;
    }
    if let Some(year) = query.award_year {
        check_year(year)?;
    }
    match (&query.person, &query.role) {
        (None, Some(_)) => {
            return Err(AppError::BadRequest("role requires person".into()));
        }
        (Some(person), role) => {
            // Сам фильтр — в [`person_filter`] (нужна БД).
            validate::slug("person", person)?;
            if let Some(role) = role {
                validate::role(role)?;
            }
        }
        (None, None) => {}
    }
    Ok(())
}

/// Типы из запроса: `?kind=movie&kind=game` или `?kind=movie,game`. Неизвестный — 400, как и
/// раньше у единственного `kind`. Повторы схлопываются, порядок не важен (это ИЛИ).
fn kinds_of(repeated: &Repeated) -> AppResult<Vec<EntityKind>> {
    let mut kinds: Vec<EntityKind> = Vec::new();
    for value in repeated.values("kind") {
        let kind = EntityKind::parse(value).ok_or_else(|| {
            AppError::BadRequest(format!(
                "kind must be one of movie, series, book, game, painting (got {value})"
            ))
        })?;
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    Ok(kinds)
}

/// Страны из запроса: `?country=US&country=GB` или `?country=US,GB` — «страна одна из
/// перечисленных». Регистр не важен (`gb` → `GB`), повторы схлопываются, неизвестный код — 400.
fn countries_of(repeated: &Repeated) -> AppResult<Vec<String>> {
    let mut countries: Vec<String> = Vec::new();
    for value in repeated.values("country") {
        let code = validate::country("country", value)?;
        if !countries.contains(&code) {
            countries.push(code);
        }
    }
    Ok(countries)
}

/// Slug'и работ человека (`None` — работ нет): общая часть [`person_filter`] и Tantivy-ветки.
async fn person_slugs(
    db: &sqlx::PgPool,
    person: &str,
    role: Option<&str>,
) -> sqlx::Result<Option<Vec<String>>> {
    let slugs: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT e.slug FROM people p
         JOIN entity_credits ec ON ec.person_id = p.id
         JOIN entities e ON e.id = ec.entity_id
         WHERE p.slug = $1 AND ($2::text IS NULL OR ec.role = $2) AND {VISIBLE}"
    ))
    .bind(person)
    .bind(role.map(str::trim))
    .fetch_all(db)
    .await?;
    Ok((!slugs.is_empty()).then_some(slugs))
}

/// Виды запроса без книг: книги отвечают отдельным индексом.
fn others_of(kinds: &[EntityKind]) -> Vec<EntityKind> {
    kinds
        .iter()
        .copied()
        .filter(|kind| *kind != EntityKind::Book)
        .collect()
}

/// Запрос к индексу работ Tantivy по параметрам `GET /search`. `None` — под фильтры заведомо
/// ничего не подходит (запрос только про книги, а книжного индекса нет; у человека нет работ).
/// Параметры проверяются теми же правилами, что и раньше (неверный фильтр — 400).
async fn works_query(
    state: &AppState,
    repeated: &Repeated,
    query: &SearchQuery,
) -> AppResult<Option<crate::works::index::WorksQuery>> {
    use crate::works::index::{WorksQuery, WorksSort, YearRange};
    let kinds = kinds_of(repeated)?;
    let others = others_of(&kinds);
    let countries = countries_of(repeated)?;
    filters_of(query)?;
    if !kinds.is_empty() && others.is_empty() {
        return Ok(None);
    }
    let trimmed = |value: &Option<String>| value.as_deref().map(|v| v.trim().to_owned());
    let only_slugs = match &query.person {
        Some(person) => match person_slugs(&state.db, person, query.role.as_deref()).await? {
            Some(slugs) => Some(slugs),
            None => return Ok(None),
        },
        None => None,
    };
    let (from, to) = match query.year {
        Some(year) => (Some(year), Some(year)),
        None => (query.year_from, query.year_to),
    };
    let sort = query.sort.map(|sort| match sort {
        SearchSort::Recommended => WorksSort::Recommended,
        SearchSort::Trending => WorksSort::Trending,
        SearchSort::TopRated => WorksSort::TopRated,
        SearchSort::New => WorksSort::New,
        SearchSort::Soon => WorksSort::Soon,
        SearchSort::Title => WorksSort::Title,
        SearchSort::Popular => WorksSort::Popular,
        SearchSort::Rating => WorksSort::Rating,
        SearchSort::Awarded => WorksSort::Awarded,
        SearchSort::Random => WorksSort::Relevance,
    });
    Ok(Some(WorksQuery {
        q: query.q.as_deref().unwrap_or_default().trim().to_owned(),
        kinds: others.iter().map(|kind| kind.as_str().to_owned()).collect(),
        countries,
        tags: query.tag.iter().cloned().collect(),
        years: YearRange { from, to },
        studio: trimmed(&query.studio),
        publisher: trimmed(&query.publisher),
        museum: trimmed(&query.museum),
        studio_slug: query.studio_slug.clone(),
        award: query.award.clone(),
        award_year: query.award_year,
        awarded: query.awarded,
        only_slugs,
        exclude_slugs: Vec::new(),
        now_ts: day_ts(Utc::now().date_naive()),
        sort,
    }))
}

/// Подсказки студий для фильтра `studio` (фасет по полю `studios`): по части
/// названия с опечатками; без `q` — самые частые. `count` — сколько видимых сущностей в индексе,
/// `slug` и `logo_url` — из `studios` (одним запросом на ответ), логотип тот же, что на странице
/// студии.
#[utoipa::path(
    get, operation_id = "search_studios", path = "/search/studios", tag = "catalog",
    params(StudiosQuery),
    responses(
        (status = 200, description = "Студии", body = Vec<StudioHit>),
        (status = 400, description = "Слишком длинный `q`", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn studios(
    State(state): State<AppState>,
    Query(query): Query<StudiosQuery>,
) -> AppResult<Json<Vec<StudioHit>>> {
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > MAX_STUDIO {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_STUDIO} characters"
        )));
    }
    let hits = facet_hits(&state, "studios", q, query.kind, query.limit).await?;
    // Slug'и страниц студий и логотипы — одним запросом на ответ.
    let refs = studio_refs(&state, &hits).await?;
    Ok(Json(
        hits.into_iter()
            .map(|(name, count)| {
                let studio = refs.get(&name);
                StudioHit {
                    slug: studio.map(|r| r.slug.clone()),
                    logo_url: studio.and_then(|r| r.logo_url.clone()),
                    name,
                    count,
                }
            })
            .collect(),
    ))
}

/// Строки `studios` для значений фасета — одним запросом на ответ, а не по хиту: панель фильтров
/// открывается одним запросом в поиск, и девяти походов в БД за логотипами там быть не должно.
/// В документе индекса логотипа нет и быть не может: facet search возвращает только само значение
/// и счётчик.
async fn studio_refs(
    state: &AppState,
    hits: &[(String, i64)],
) -> AppResult<HashMap<String, crate::studios::StudioRef>> {
    let names: Vec<String> = hits.iter().map(|(name, _)| name.clone()).collect();
    Ok(crate::studios::refs(&state.db, &names).await?)
}

async fn facet_hits(
    state: &AppState,
    facet: &str,
    q: &str,
    kind: Option<EntityKind>,
    limit: Option<i64>,
) -> AppResult<Vec<(String, i64)>> {
    crate::works::search::facet(state, facet, q, kind.map(EntityKind::as_str), limit).await
}

/// Подсказки издателей для фильтра `publisher` (facet search по полю `publishers`): издательство
/// книги, издатель или разработчик игры. `kind` сужает до раздела — у книг и игр свои издатели.
#[utoipa::path(
    get, operation_id = "search_publishers", path = "/search/publishers", tag = "catalog",
    params(FacetQuery),
    responses(
        (status = 200, description = "Издатели", body = Vec<PublisherHit>),
        (status = 400, description = "Слишком длинный `q`", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn publishers(
    State(state): State<AppState>,
    Query(query): Query<FacetQuery>,
) -> AppResult<Json<Vec<PublisherHit>>> {
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > MAX_STUDIO {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_STUDIO} characters"
        )));
    }
    let hits = if query
        .kind
        .is_some_and(|kind| crate::books::search::routed(&state, &[kind]))
    {
        crate::books::search::publishers(&state, q, query.limit).await?
    } else {
        facet_hits(&state, "publishers", q, query.kind, query.limit).await?
    };
    Ok(Json(publisher_hits(&state, hits).await?))
}

/// Издатели и музеи в карточках подсказок. Своей таблицы с логотипами у них нет, поэтому логотип
/// берётся из `studios` по совпадению названия (издатель игры часто и есть студия-разработчик) —
/// тем же одним запросом, что и у студий; не нашлось — `null`, карточка без картинки.
async fn publisher_hits(
    state: &AppState,
    hits: Vec<(String, i64)>,
) -> AppResult<Vec<PublisherHit>> {
    let refs = studio_refs(state, &hits).await?;
    Ok(hits
        .into_iter()
        .map(|(name, count)| PublisherHit {
            logo_url: refs.get(&name).and_then(|r| r.logo_url.clone()),
            name,
            count,
        })
        .collect())
}

/// Подсказки музеев для фильтра `museum` (facet search по полю `museums`): где хранится картина.
#[utoipa::path(
    get, operation_id = "search_museums", path = "/search/museums", tag = "catalog",
    params(FacetQuery),
    responses(
        (status = 200, description = "Музеи", body = Vec<PublisherHit>),
        (status = 400, description = "Слишком длинный `q`", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn museums(
    State(state): State<AppState>,
    Query(query): Query<FacetQuery>,
) -> AppResult<Json<Vec<PublisherHit>>> {
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > MAX_STUDIO {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_STUDIO} characters"
        )));
    }
    let hits = facet_hits(&state, "museums", q, query.kind, query.limit).await?;
    Ok(Json(publisher_hits(&state, hits).await?))
}

/// Подсказки стран для фильтра `country` (facet search по полю `countries`): коды ISO 3166-1
/// alpha-2 со счётчиками, самые частые сверху. Названия («США») показывает фронтенд, поэтому
/// запрос `q` по названию бессмыслен — в фасете коды; `kind` сужает до раздела.
#[utoipa::path(
    get, operation_id = "search_countries", path = "/search/countries", tag = "catalog",
    params(FacetQuery),
    responses(
        (status = 200, description = "Страны", body = Vec<CountryHit>),
        (status = 400, description = "Слишком длинный `q`", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn countries(
    State(state): State<AppState>,
    Query(query): Query<FacetQuery>,
) -> AppResult<Json<Vec<CountryHit>>> {
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > MAX_STUDIO {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_STUDIO} characters"
        )));
    }
    let hits = facet_hits(&state, "countries", q, query.kind, query.limit).await?;
    Ok(Json(
        hits.into_iter()
            .map(|(code, count)| CountryHit { code, count })
            .collect(),
    ))
}

/// Подсказки тегов и жанров для фильтра `tag` (facet search по полю `tags`): только те, что есть
/// у сущностей этого типа — жанры фильмов не предлагаются в книгах. Фасет хранит slug'и, поэтому
/// названия и `kind` тегов дочитываются из `tags` одним запросом на ответ. Запрос `q` по названию
/// бессмысленен (в фасете slug'и), фильтровать по названию — на стороне клиента.
#[utoipa::path(
    get, operation_id = "search_tags", path = "/search/tags", tag = "catalog",
    params(FacetQuery),
    responses(
        (status = 200, description = "Теги и жанры", body = Vec<TagHit>),
        (status = 400, description = "Слишком длинный `q`", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn tags(
    State(state): State<AppState>,
    Query(query): Query<FacetQuery>,
) -> AppResult<Json<Vec<TagHit>>> {
    let q = query.q.as_deref().unwrap_or_default().trim();
    if q.chars().count() > MAX_STUDIO {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_STUDIO} characters"
        )));
    }
    let hits = if query
        .kind
        .is_some_and(|kind| crate::books::search::routed(&state, &[kind]))
    {
        crate::books::search::tags(&state, query.limit).await?
    } else {
        facet_hits(&state, "tags", q, query.kind, query.limit).await?
    };
    let slugs: Vec<String> = hits.iter().map(|(slug, _)| slug.clone()).collect();
    let rows: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT slug, name, kind, description FROM tags WHERE slug = ANY($1)")
            .bind(&slugs)
            .fetch_all(&state.db)
            .await?;
    let named: HashMap<String, (String, String, Option<String>)> = rows
        .into_iter()
        .map(|(slug, name, kind, description)| (slug, (name, kind, description)))
        .collect();
    Ok(Json(
        hits.into_iter()
            .map(|(slug, count)| {
                let (name, kind, description) = named
                    .get(&slug)
                    .cloned()
                    .unwrap_or_else(|| (slug.clone(), "tag".to_owned(), None));
                TagHit {
                    slug,
                    name,
                    kind,
                    description,
                    count,
                }
            })
            .collect(),
    ))
}

/// Дослать в поисковый индекс только указанные сущности — вместо полной перестройки.
///
/// Нужно после загрузки данных в БД в обход API (импорты `scripts/`, `make seed`): перестройка
/// обоих индексов идёт десятки минут, а сотне новых карточек хватает одного запроса. Делает то
/// же, что правка в админке ([`update_index`]), но без ISR: видимые сущности переотправляются
/// (и в теневой индекс, если идёт перестройка), скрытые и удалённые — удаляются из индекса.
#[utoipa::path(
    post, operation_id = "index_entities", path = "/admin/search/entities", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = IndexEntities,
    responses(
        (status = 200, description = "Отправлено в индекс", body = IndexResult),
        (status = 400, description = "Нужен ровно один из `slugs` / `ids`, не больше 1000", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn index_entities(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<IndexEntities>,
) -> AppResult<Json<IndexResult>> {
    let ids = match (req.slugs, req.ids) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(AppError::BadRequest(
                "exactly one of slugs / ids is required".into(),
            ))
        }
        (Some(slugs), None) => {
            check_count(slugs.len())?;
            for slug in &slugs {
                validate::slug("slugs", slug)?;
            }
            sqlx::query_scalar("SELECT id FROM entities WHERE slug = ANY($1)")
                .bind(&slugs)
                .fetch_all(&state.db)
                .await?
        }
        (None, Some(ids)) => {
            check_count(ids.len())?;
            ids
        }
    };
    let requested = ids.len();
    // Та же дорога, что у правки в админке: пачками, синхронно.
    for chunk in ids.chunks(BATCH) {
        if let Err(error) = update_entities(&state, &Touched::entities(chunk)).await {
            tracing::warn!(%error, "index entities failed");
            return Err(unavailable());
        }
    }
    // Сколько из запрошенных реально видимы: остальные из индекса удалены.
    let indexed: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM entities e {ENTITY_STATS_JOIN}
         WHERE e.id = ANY($1) AND {VISIBLE} AND {IN_CATALOG}"
    ))
    .bind(&ids)
    .fetch_one(&state.db)
    .await?;
    let indexed = indexed as usize;
    tracing::info!(admin_id = %admin.id, requested, indexed, "entities sent to search index");
    Ok(Json(IndexResult {
        indexed,
        removed: requested.saturating_sub(indexed),
    }))
}

/// Сколько сущностей принимаем за один запрос: ровно столько же читает из БД перестройка.
const MAX_INDEX_ENTITIES: usize = 1000;

fn check_count(count: usize) -> AppResult<()> {
    if count == 0 || count > MAX_INDEX_ENTITIES {
        return Err(AppError::BadRequest(format!(
            "1-{MAX_INDEX_ENTITIES} entities per request"
        )));
    }
    Ok(())
}

/// Перестроить поисковый индекс из PostgreSQL без простоя: теневой индекс, проверка размера,
/// swap, ротация версий, пересборка статики. Ждёт завершения. Лог шагов — `.../reindex/stream`.
///
/// Нужно после загрузки данных в БД в обход API (`make seed`) или если индекс отстал.
#[utoipa::path(
    post, path = "/admin/search/reindex", tag = "catalog-admin",
    security(("bearer" = [])),
    params(ReindexQuery),
    responses(
        (status = 200, description = "Индекс перестроен, поиск переключён", body = ReindexResult),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Перестройка уже идёт или новый индекс меньше 80% текущего (поиск не переключён)", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn reindex_handler(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Query(query): Query<ReindexQuery>,
) -> AppResult<Json<ReindexResult>> {
    let options = ReindexOptions {
        force: query.force,
        revalidate: true,
    };
    let result = reindex(&state, options, &Reporter::silent()).await?;
    tracing::info!(admin_id = %admin.id, indexed = result.indexed, "search index rebuilt");
    Ok(Json(result))
}

/// То же, ход перестройки потоком `text/event-stream`: `plan`, `step` по шагам `create_index`,
/// `fill` (с `progress`: `done`/`total`), `check`, `swap`, `rotate`, `isr`, затем `done`
/// (`reindex` — итог, если поиск переключён).
///
/// Перестройка уже идёт — `409` JSON до потока. Новый индекс меньше 80% текущего — шаг `check`
/// `failed`, поиск не переключается; `?force=true` — переключить всё равно. Перестройка
/// доводится до конца, даже если клиент закрыл соединение.
#[utoipa::path(
    post, path = "/admin/search/reindex/stream", tag = "catalog-admin",
    security(("bearer" = [])),
    params(ReindexQuery),
    responses(
        (status = 200, description = "Поток событий перестройки",
            content_type = "text/event-stream", body = JobEvent),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Перестройка уже идёт", body = ErrorBody),
        (status = 503, description = "Поиск недоступен", body = ErrorBody),
    )
)]
pub async fn reindex_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Query(query): Query<ReindexQuery>,
) -> AppResult<Response> {
    let options = ReindexOptions {
        force: query.force,
        revalidate: true,
    };
    // До потока: «уже идёт» — обычный 409.
    let lock = ReindexLock::acquire(&state).await?;
    tracing::info!(admin_id = %admin.id, force = options.force, "search reindex started");
    Ok(jobs::stream(&options.steps(), move |report| async move {
        let result = reindex_locked(&state, &lock, options, &report).await;
        lock.release(&state).await;
        match result {
            Ok(result) => {
                tracing::info!(indexed = result.indexed, "search index rebuilt");
                DoneEvent {
                    ok: true,
                    reindex: Some(result),
                    ..DoneEvent::default()
                }
            }
            Err(error) => {
                tracing::warn!(%error, "search reindex failed");
                DoneEvent::default()
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_alternates_two_others_and_a_book_then_drains() {
        // 4 работы и 3 книги: О О К О О К К.
        let (order, o, b) = blend_order(4, 3, 10, 0);
        assert_eq!(order, [true, true, false, true, true, false, false]);
        assert_eq!((o, b), (0, 0));
        // Вторая страница по три: перед ней две работы и книга.
        let (order, o, b) = blend_order(4, 3, 3, 3);
        assert_eq!(order, [true, true, false]);
        assert_eq!((o, b), (2, 1));
        // Книг нет — одни работы; работ нет — одни книги.
        assert_eq!(blend_order(2, 0, 5, 0).0, [true, true]);
        assert_eq!(blend_order(0, 2, 5, 0).0, [false, false]);
        // За концом выдачи — пусто.
        assert!(blend_order(1, 1, 5, 2).0.is_empty());
    }

    fn query(params: Value) -> SearchQuery {
        serde_json::from_value(params).unwrap()
    }

    /// Типы запроса из строки вида `kind=movie&kind=game` — как их видит обработчик.
    fn kinds(raw: &str) -> AppResult<Vec<EntityKind>> {
        kinds_of(&Repeated::from_query(raw))
    }

    #[test]
    fn several_kinds() {
        assert_eq!(kinds("").unwrap(), []);
        assert_eq!(kinds("kind=movie").unwrap(), [EntityKind::Movie]);
        // Повторяющийся параметр и перечисление через запятую — одно и то же.
        for raw in [
            "kind=movie&kind=game",
            "kind=movie,game",
            "kind=movie&kind=game&kind=movie",
        ] {
            assert_eq!(
                kinds(raw).unwrap(),
                [EntityKind::Movie, EntityKind::Game],
                "{raw}"
            );
        }
        assert_eq!(kinds("kind=comic").unwrap_err().code(), "validation");
        assert_eq!(
            kinds("kind=movie&kind=comic").unwrap_err().code(),
            "validation"
        );
    }

    /// Страны запроса — как их видит обработчик.
    fn countries(raw: &str) -> AppResult<Vec<String>> {
        countries_of(&Repeated::from_query(raw))
    }

    #[test]
    fn several_countries() {
        assert_eq!(countries("").unwrap(), [] as [String; 0]);
        // Повтор, запятая и регистр — одно и то же.
        for raw in [
            "country=US&country=GB",
            "country=US,GB",
            "country=us&country=gb&country=US",
        ] {
            assert_eq!(countries(raw).unwrap(), ["US", "GB"], "{raw}");
        }
        for bad in ["country=USA", "country=%D0%A1%D0%A8%D0%90", "country=ZZ"] {
            assert_eq!(countries(bad).unwrap_err().code(), "validation", "{bad}");
        }
    }

    #[test]
    fn filters() {
        let ok = |params| filters_of(&query(params)).unwrap();
        ok(json!({ "year": 2001 }));
        ok(json!({ "year_from": 2000, "year_to": 2010 }));
        ok(json!({ "person": "christopher-nolan", "role": "director" }));
        ok(json!({ "book_form": "collection,all" }));
        for bad in [
            json!({ "year": 2001, "year_from": 2000 }),
            json!({ "year_from": 2010, "year_to": 2000 }),
            json!({ "year_from": 0 }),
            json!({ "studio": " " }),
            json!({ "role": "director" }),
            json!({ "person": "Bad Slug" }),
            json!({ "person": "christopher-nolan", "role": "Director" }),
            json!({ "book_form": "book,magazine" }),
        ] {
            let error = filters_of(&query(bad.clone())).unwrap_err();
            assert_eq!(error.code(), "validation", "{bad}");
        }
    }

    #[test]
    fn steps_follow_options() {
        let steps = ReindexOptions::default().steps();
        assert_eq!(steps, [Step::Works, Step::People]);
        let options = ReindexOptions {
            revalidate: true,
            ..ReindexOptions::default()
        };
        assert_eq!(options.steps().last(), Some(&Step::Isr));
    }
}
