//! Поиск по каталогу через Meilisearch.
//!
//! Два индекса: `entities` (здесь) и `people` ([`crate::people_search`]); перестраиваются и
//! переключаются вместе. Источник правды — PostgreSQL, индексы производные и нужны только для поиска:
//! карточки (`GET /entities/{slug}`, `GET /people/{slug}`) читаются из БД. Документ компактный
//! ([`SearchDoc`]): без карточки, описания и участников.
//!
//! После каждой записи в админке затронутые сущности переотправляются в индекс
//! ([`update_index`] из [`crate::publish::run`]) с ожиданием применения. Ошибка Meilisearch
//! запись не отменяет, только пишется в лог и в поток публикации.
//!
//! **Перестройка** ([`reindex`]) — без простоя и без очистки живого индекса. Поиск всегда
//! читает индекс `entities` (роль алиаса: в Meilisearch 1.15 алиасов нет):
//! 1. создаются теневые `entities_v<unix ms>` и `people_v<unix ms>`;
//! 2. все сущности из БД пачками по [`BATCH`] уходят в первый, затем пересчитывается известность
//!    людей и все публичные люди уходят во второй (`progress` в потоке по каждому шагу);
//! 3. каждый новый индекс должен быть не меньше `SEARCH_REINDEX_MIN_PERCENT`% текущего (80, в
//!    dev — 0) — иначе поиск не переключается (сломанная или неполная выборка), теневые
//!    удаляются; `force` — пропустить;
//! 4. `swap-indexes` (обе пары одной задачей): `entities` и `people` получают новые документы
//!    атомарно для всех инстансов, а `*_v<ms>` — предыдущую версию (откат — swap обратно);
//! 5. ротация: хранятся текущая и одна предыдущая версия, `*_v*` старше удаляются;
//! 6. фронтенду — запустить пересборку всей статики (`{"all": true}`; он отвечает `202` и
//!    пересобирает в фоне), только при ручном запуске.
//!
//! Перестройка одна за раз: в процессе — набор занятых индексов, между инстансами — блокировка
//! в Redis. Правки во время перестройки пишутся и в теневой индекс (в пределах инстанса).

use crate::entities::check_year;
use crate::jobs::{self, DoneEvent, JobEvent, Reporter, Step, StepStatus};
use crate::models::{
    page_bounds, Entity, EntityKind, EntitySummary, Page, ReindexQuery, ReindexResult,
    ReleaseStatus, SearchQuery, SearchSort, StudioHit, StudiosQuery, ENTITY_COLUMNS,
    ENTITY_STATS_JOIN, VISIBLE,
};
use crate::people_search;
use crate::publish::Touched;
use crate::validate;
use axum::extract::State;
use axum::http::Method;
use axum::response::Response;
use axum::Json;
use chrono::{Datelike, NaiveDate, NaiveTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::search::{Search, SearchError};
use shared::{AdminUser, AppError, AppResult, AppState};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Имя индекса сущностей (без префикса). Его читает поиск.
pub const INDEX: &str = "entities";

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
struct SearchDoc {
    id: Uuid,
    kind: EntityKind,
    slug: String,
    title: String,
    original_title: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    /// Для фильтров `year = 2021`, `year 2000 TO 2010`.
    year: Option<i32>,
    /// `release_date` в unix-секундах (полночь UTC): сортировки `new` / `soon` и их фильтр
    /// «вышел / не вышел» (Meilisearch сравнивает только числа).
    release_ts: Option<i64>,
    /// Все студии из `metadata.studios`: поиск («dreamworks»), фильтр `studio`, подсказки.
    studios: Vec<String>,
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
}

impl SearchDoc {
    fn new(entity: Entity, tags: Vec<String>) -> Self {
        let studios = entity.metadata["studios"]
            .as_array()
            .map(|studios| {
                studios
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
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
            studios,
            tags,
            rank_score: entity.rank_score,
            popularity: entity.tmdb_popularity,
            quality: entity.quality,
            imdb_rating: entity.imdb_rating,
            imdb_votes: entity.imdb_votes,
            release_status: entity.release_status,
        }
    }
}

/// Дата → unix-секунды её полуночи (UTC).
fn day_ts(date: NaiveDate) -> i64 {
    date.and_time(NaiveTime::MIN).and_utc().timestamp()
}

fn entity_settings() -> Value {
    json!({
        // Порядок задаёт вес: совпадение в названии важнее, чем в студии.
        "searchableAttributes": ["title", "original_title", "studios"],
        "filterableAttributes": ["kind", "tags", "year", "slug", "studios", "release_ts"],
        "sortableAttributes": ["rank_score", "popularity", "quality", "release_ts", "title"],
        // Подсказки студий без запроса — самые частые.
        "faceting": { "sortFacetValuesBy": { "studios": "count" } },
    })
}

/// Документы пачки сущностей: строки уже прочитаны, теги — одним запросом на пачку.
async fn docs_of(db: &sqlx::PgPool, entities: Vec<Entity>) -> Result<Vec<SearchDoc>, BoxError> {
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
    Ok(entities
        .into_iter()
        .map(|entity| {
            let tags = tags.remove(&entity.id).unwrap_or_default();
            SearchDoc::new(entity, tags)
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
    let entities: Vec<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS} FROM entities e {ENTITY_STATS_JOIN}
         WHERE e.id = ANY($1) AND {VISIBLE} ORDER BY e.id"
    ))
    .bind(&touched.entities)
    .fetch_all(&state.db)
    .await?;
    let docs = docs_of(&state.db, entities).await?;
    let present: Vec<Uuid> = docs.iter().map(|doc| doc.id).collect();
    let docs = serde_json::to_value(&docs)?;

    let main = state.search.index(INDEX);
    upsert_and_delete(&state.search, &main, &docs, &touched.entities, &present).await?;
    if let Some(shadow) = shadow_of(&main) {
        // Не успеет — перестройка перезапишет документ более ранним чтением; не страшно.
        if let Err(error) =
            upsert_and_delete(&state.search, &shadow, &docs, &touched.entities, &present).await
        {
            tracing::warn!(%error, %shadow, "shadow index sync failed");
        }
    }
    Ok(())
}

/// Обновляет документы `docs` в индексе `index` и удаляет те из `ids`, которых нет среди
/// `present`. Ждёт применения.
pub(crate) async fn upsert_and_delete(
    search: &Search,
    index: &str,
    docs: &Value,
    ids: &[Uuid],
    present: &[Uuid],
) -> Result<(), BoxError> {
    if docs.as_array().is_some_and(|docs| !docs.is_empty()) {
        search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents?primaryKey=id"),
                Some(docs),
            )
            .await?;
    }
    let deleted: Vec<Uuid> = ids
        .iter()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    if !deleted.is_empty() {
        let result = search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents/delete-batch"),
                Some(&json!(deleted)),
            )
            .await;
        // Удалять из ещё не созданного индекса нечего.
        ignore(result, "index_not_found")?;
    }
    Ok(())
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
        let mut steps = vec![
            Step::CreateIndex,
            Step::Fill,
            Step::FillPeople,
            Step::Check,
            Step::Swap,
            Step::Rotate,
        ];
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

/// Индексы, которые сейчас перестраиваются в этом процессе (в тестах у каждого свой префикс).
static RUNNING: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// Теневой индекс идущей перестройки: основной → теневой.
static SHADOWS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub(crate) fn shadow_of(main: &str) -> Option<String> {
    SHADOWS.lock().unwrap().as_ref()?.get(main).cloned()
}

fn set_shadow(main: &str, shadow: Option<String>) {
    let mut shadows = SHADOWS.lock().unwrap();
    let shadows = shadows.get_or_insert_with(HashMap::new);
    match shadow {
        Some(shadow) => shadows.insert(main.to_string(), shadow),
        None => shadows.remove(main),
    };
}

/// Право на перестройку. Снимать через [`ReindexLock::release`]; в процессе снимается и при drop.
pub struct ReindexLock {
    main: String,
    /// Индекс людей: его теневой тоже снимается при drop.
    people: String,
    redis: bool,
    /// Продление блокировки в Redis, пока она взята.
    renew: Option<tokio::task::JoinHandle<()>>,
}

impl ReindexLock {
    pub async fn acquire(state: &AppState) -> Result<Self, ReindexError> {
        if !state.search.is_enabled() {
            return Err(ReindexError::Disabled);
        }
        let main = state.search.index(INDEX);
        if !RUNNING
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(main.clone())
        {
            return Err(ReindexError::Busy);
        }
        let mut lock = Self {
            main,
            people: state.search.index(people_search::INDEX),
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
        if let Some(running) = RUNNING.lock().unwrap().as_mut() {
            running.remove(&self.main);
        }
        set_shadow(&self.main, None);
        set_shadow(&self.people, None);
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

/// Шаги перестройки (см. описание модуля). Каждый сообщает о себе в `report`; упавший шаг —
/// `failed` и ошибка, следующие не выполняются.
pub async fn reindex_locked(
    state: &AppState,
    lock: &ReindexLock,
    options: ReindexOptions,
    report: &Reporter,
) -> Result<ReindexResult, ReindexError> {
    let search = &state.search;
    let stamp = unix_ms();
    let main = lock.main.clone();
    let version = format!("{main}_v{stamp}");
    let people_main = lock.people.clone();
    let people_version = format!("{people_main}_v{stamp}");
    let fail = |step: Step, started: Instant, error: String| {
        report.finish(step, StepStatus::Failed, error.clone(), started);
        ReindexError::Failed(error)
    };
    let drop_versions = || async {
        drop_index(search, &version).await;
        drop_index(search, &people_version).await;
    };

    // 1. Теневые индексы. Основные создаются, если их нет: swap требует оба.
    let started = report.start(Step::CreateIndex);
    let created = async {
        create_version(search, &main, &version, &entity_settings()).await?;
        create_version(
            search,
            &people_main,
            &people_version,
            &people_search::settings(),
        )
        .await
    };
    if let Err(error) = created.await {
        drop_versions().await;
        return Err(fail(Step::CreateIndex, started, error.to_string()));
    }
    set_shadow(&main, Some(version.clone()));
    set_shadow(&people_main, Some(people_version.clone()));
    report.finish(
        Step::CreateIndex,
        StepStatus::Done,
        format!("Созданы теневые индексы {version}, {people_version}"),
        started,
    );

    // 2. Все сущности пачками.
    let started = report.start(Step::Fill);
    let indexed = match fill(state, &version, report).await {
        Ok(indexed) => indexed,
        Err(error) => {
            drop_versions().await;
            return Err(fail(Step::Fill, started, error.to_string()));
        }
    };
    report.finish(
        Step::Fill,
        StepStatus::Done,
        format!("Проиндексировано {indexed} сущностей"),
        started,
    );

    // 2б. Известность людей и все публичные люди.
    let started = report.start(Step::FillPeople);
    let people_indexed = match people_search::fill(state, &people_version, report).await {
        Ok(indexed) => indexed,
        Err(error) => {
            drop_versions().await;
            return Err(fail(Step::FillPeople, started, error.to_string()));
        }
    };
    report.finish(
        Step::FillPeople,
        StepStatus::Done,
        format!("Проиндексировано {people_indexed} человек"),
        started,
    );

    // 3. Не переключать на подозрительно маленький индекс.
    let started = report.start(Step::Check);
    let sizes = async {
        Ok::<_, SearchError>([
            (
                INDEX,
                documents(search, &version).await?,
                documents(search, &main).await?,
            ),
            (
                people_search::INDEX,
                documents(search, &people_version).await?,
                documents(search, &people_main).await?,
            ),
        ])
    };
    let sizes = match sizes.await {
        Ok(sizes) => sizes,
        Err(error) => {
            drop_versions().await;
            return Err(fail(Step::Check, started, error.to_string()));
        }
    };
    let min = state.config.search_reindex_min_percent;
    let describe = |(name, new, current): &(&str, u64, u64)| {
        let percent = (new * 100).checked_div(*current).unwrap_or(100);
        format!("{name}: {new} документов, текущий: {current} ({percent}%)")
    };
    let summary = sizes.iter().map(describe).collect::<Vec<_>>().join("; ");
    let current = sizes[0].2;
    match sizes
        .iter()
        .find(|(_, new, current)| new * 100 < current * min)
    {
        Some(&(index, new, current)) if !options.force => {
            drop_versions().await;
            report.finish(
                Step::Check,
                StepStatus::Failed,
                format!(
                    "Новые индексы — {summary}. Нужно не меньше {min}% — поиск не переключён, \
                     теневые индексы удалены. Если данные удалены намеренно — повторить с \
                     force=true"
                ),
                started,
            );
            return Err(ReindexError::TooSmall {
                index,
                new,
                current,
                min,
            });
        }
        Some(_) => report.finish(
            Step::Check,
            StepStatus::Done,
            format!("Новые индексы — {summary}: меньше {min}%, но force=true"),
            started,
        ),
        None => report.finish(
            Step::Check,
            StepStatus::Done,
            format!("Новые индексы — {summary}"),
            started,
        ),
    }

    // 4. Переключение одной задачей: entities ⇄ entities_v<ms>, people ⇄ people_v<ms>.
    let started = report.start(Step::Swap);
    search
        .call_and_wait(
            Method::POST,
            "/swap-indexes",
            Some(&json!([
                { "indexes": [main, version] },
                { "indexes": [people_main, people_version] },
            ])),
        )
        .await
        .map_err(|error| fail(Step::Swap, started, error.to_string()))?;
    // До сих пор правки шли и в теневые: ни одна не осталась только в старой версии.
    set_shadow(&main, None);
    set_shadow(&people_main, None);
    report.finish(
        Step::Swap,
        StepStatus::Done,
        format!(
            "Поиск переключён на новые индексы; предыдущие версии сохранены в {version}, \
             {people_version}"
        ),
        started,
    );

    // 5. Ротация: текущая (main) и предыдущая (version), всё старше — удалить.
    let started = report.start(Step::Rotate);
    let rotated = async {
        let mut deleted = rotate(search, &main, &version).await?;
        deleted.extend(rotate(search, &people_main, &people_version).await?);
        Ok::<_, SearchError>(deleted)
    };
    let deleted = rotated
        .await
        .map_err(|error| fail(Step::Rotate, started, error.to_string()))?;
    let message = if deleted.is_empty() {
        "Устаревших индексов нет".to_string()
    } else {
        format!("Удалены устаревшие индексы: {}", deleted.join(", "))
    };
    report.finish(Step::Rotate, StepStatus::Done, message, started);

    // 6. Статика.
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

    Ok(ReindexResult {
        indexed,
        people_indexed,
        previous_count: current,
        previous_version: version,
        deleted,
    })
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Метка версии: `entities_v1759912345123` → `1759912345123`.
fn version_ms(main: &str, uid: &str) -> Option<u128> {
    uid.strip_prefix(main)?.strip_prefix("_v")?.parse().ok()
}

async fn create_version(
    search: &Search,
    main: &str,
    version: &str,
    settings: &Value,
) -> Result<(), SearchError> {
    search
        .call_and_wait(
            Method::POST,
            "/indexes",
            Some(&json!({ "uid": version, "primaryKey": "id" })),
        )
        .await?;
    search
        .call_and_wait(
            Method::PATCH,
            &format!("/indexes/{version}/settings"),
            Some(settings),
        )
        .await?;
    ignore(
        search
            .call_and_wait(
                Method::POST,
                "/indexes",
                Some(&json!({ "uid": main, "primaryKey": "id" })),
            )
            .await,
        "index_already_exists",
    )
}

/// Все видимые сущности из БД пачками по [`BATCH`] (keyset по id) в индекс `version`. Пока Meilisearch
/// применяет пачку, читается следующая. Возвращает число отправленных документов.
async fn fill(state: &AppState, version: &str, report: &Reporter) -> Result<usize, BoxError> {
    let total: i64 =
        sqlx::query_scalar(&format!("SELECT count(*) FROM entities e WHERE {VISIBLE}"))
            .fetch_one(&state.db)
            .await?;
    let total = u64::try_from(total).unwrap_or(0);
    report.progress(Step::Fill, 0, total);

    let mut after: Option<Uuid> = None;
    let mut indexed: usize = 0;
    let mut pending: Option<(Value, usize)> = None;
    loop {
        let entities: Vec<Entity> = sqlx::query_as(&format!(
            "SELECT {ENTITY_COLUMNS} FROM entities e {ENTITY_STATS_JOIN}
             WHERE {VISIBLE} AND ($1::uuid IS NULL OR e.id > $1) ORDER BY e.id LIMIT $2"
        ))
        .bind(after)
        .bind(BATCH as i64)
        .fetch_all(&state.db)
        .await?;
        let Some(last) = entities.last() else { break };
        after = Some(last.id);
        let docs = docs_of(&state.db, entities).await?;
        let count = docs.len();
        let task = state
            .search
            .call(
                Method::POST,
                &format!("/indexes/{version}/documents"),
                Some(&serde_json::to_value(&docs)?),
            )
            .await?;
        if let Some((task, count)) = pending.replace((task, count)) {
            state.search.wait(&task).await?;
            indexed += count;
            report.progress(Step::Fill, indexed as u64, total.max(indexed as u64));
        }
    }
    if let Some((task, count)) = pending {
        state.search.wait(&task).await?;
        indexed += count;
        report.progress(Step::Fill, indexed as u64, total.max(indexed as u64));
    }
    Ok(indexed)
}

async fn documents(search: &Search, index: &str) -> Result<u64, SearchError> {
    let stats = search
        .call(Method::GET, &format!("/indexes/{index}/stats"), None)
        .await?;
    Ok(stats["numberOfDocuments"].as_u64().unwrap_or(0))
}

/// Удаляет версии `{main}_v*` старше `keep`. Более новые не трогает (их не бывает: перестройка
/// одна за раз). Возвращает удалённые.
async fn rotate(search: &Search, main: &str, keep: &str) -> Result<Vec<String>, SearchError> {
    let keep_ms = version_ms(main, keep).unwrap_or(0);
    let indexes = search
        .call(Method::GET, "/indexes?limit=1000", None)
        .await?;
    let mut old: Vec<(u128, String)> = indexes["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|index| index["uid"].as_str())
        .filter_map(|uid| Some((version_ms(main, uid)?, uid.to_string())))
        .filter(|(ms, _)| *ms < keep_ms)
        .collect();
    old.sort_unstable();
    for (_, uid) in &old {
        ignore(
            search
                .call_and_wait(Method::DELETE, &format!("/indexes/{uid}"), None)
                .await,
            "index_not_found",
        )?;
    }
    Ok(old.into_iter().map(|(_, uid)| uid).collect())
}

/// Удаляет недостроенный теневой индекс; ошибка — только в лог (уберёт следующая ротация).
async fn drop_index(search: &Search, index: &str) {
    let result = search
        .call_and_wait(Method::DELETE, &format!("/indexes/{index}"), None)
        .await;
    if let Err(error) = ignore(result, "index_not_found") {
        tracing::warn!(%error, %index, "shadow index cleanup failed");
    }
}

fn ignore(result: Result<(), SearchError>, code: &str) -> Result<(), SearchError> {
    match result {
        Err(error) if error.code() == Some(code) => Ok(()),
        other => other,
    }
}

/// Перестройка в фоне: сразу при старте приложения, затем каждые
/// `SEARCH_REINDEX_INTERVAL_SECS` (если не 0). По расписанию перестраивает только один инстанс:
/// блокировка в Redis. Статику фронтенда не трогает.
pub fn spawn_reindex(state: AppState) {
    if !state.search.is_enabled() {
        return;
    }
    let interval = Duration::from_secs(state.config.search_reindex_interval_secs);
    tokio::spawn(async move {
        run_reindex(&state, "startup").await;
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

pub(crate) fn unavailable() -> AppError {
    AppError::Unavailable("search is temporarily unavailable".into())
}

/// Полнотекстовый поиск по активным сущностям каталога: опечатки, ранжирование, фильтры. Весь
/// поиск на сайте идёт сюда (в `GET /entities` поиска по названию нет).
///
/// Фильтры (через AND): `kind`, `tag`, год (`year` — ровно, `year_from` / `year_to` — диапазон
/// включительно), `studio` (точное название, без учёта регистра). `person` / `role` — заглушка:
/// людей в индексе пока нет, ответ 503 `person_filter_unavailable`.
///
/// Сортировка по релевантности; `sort=recommended` / `trending` / `top_rated` — по баллу
/// «Рекомендуемые», TMDB popularity, рейтингу IMDb (у кого значения нет — в конце); `new` — только
/// вышедшие, новые сверху; `soon` — только не вышедшие, ближайшие сверху; `title` — по названию.
///
/// Индекс обновляется после изменений в админке с задержкой в доли секунды.
#[utoipa::path(
    get, path = "/search", tag = "catalog",
    params(SearchQuery),
    responses(
        (status = 200, description = "Страница результатов. `total` — оценка", body = Page<EntitySummary>),
        (status = 400, description = "Неверный фильтр", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен; `person_filter_unavailable` — фильтр по людям ещё не готов", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Page<EntitySummary>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let mut filters = filters_of(&query)?;
    let sort = match query.sort {
        Some(SearchSort::Recommended) => Some("rank_score:desc"),
        Some(SearchSort::Trending) => Some("popularity:desc"),
        Some(SearchSort::TopRated) => Some("quality:desc"),
        Some(SearchSort::New) => {
            filters.push(format!("release_ts <= {}", day_ts(Utc::now().date_naive())));
            Some("release_ts:desc")
        }
        Some(SearchSort::Soon) => {
            filters.push(format!("release_ts > {}", day_ts(Utc::now().date_naive())));
            Some("release_ts:asc")
        }
        Some(SearchSort::Title) => Some("title:asc"),
        None => None,
    };
    let mut body = json!({
        "q": query.q.as_deref().unwrap_or_default().trim(),
        "filter": if filters.is_empty() { Value::Null } else { filters.join(" AND ").into() },
        "limit": limit,
        "offset": offset,
        "attributesToRetrieve":
            ["id", "kind", "slug", "title", "original_title", "release_date", "cover_url",
             "release_status", "imdb_rating", "imdb_votes"],
    });
    if let Some(sort) = sort {
        body["sort"] = json!([sort]);
    }

    let path = format!("/indexes/{}/search", state.search.index(INDEX));
    let response = match state.search.call(Method::POST, &path, Some(&body)).await {
        Ok(response) => response,
        // Индекс ещё не построен — искать не в чем.
        Err(error) if error.code() == Some("index_not_found") => json!({ "hits": [] }),
        Err(SearchError::Disabled) => return Err(unavailable()),
        Err(error) => {
            tracing::warn!(%error, "search request failed");
            return Err(unavailable());
        }
    };
    let items: Vec<EntitySummary> = serde_json::from_value(response["hits"].clone())
        .map_err(|e| AppError::Internal(format!("unexpected search hits: {e}")))?;
    let total = response["estimatedTotalHits"].as_i64().unwrap_or(0);

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Длина значения фильтра `studio` и запроса подсказок.
const MAX_STUDIO: usize = 200;

/// Фильтры Meilisearch из запроса. Всё, что подставляется строкой, проверено или экранировано.
fn filters_of(query: &SearchQuery) -> AppResult<Vec<String>> {
    let mut filters = Vec::new();
    if let Some(kind) = query.kind {
        filters.push(format!("kind = {}", kind.as_str()));
    }
    if let Some(tag) = &query.tag {
        // Проверенный slug безопасно подставлять в строку фильтра.
        validate::slug("tag", tag)?;
        filters.push(format!("tags = \"{tag}\""));
    }
    match (query.year, query.year_from, query.year_to) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(AppError::BadRequest(
                "year cannot be combined with year_from / year_to".into(),
            ))
        }
        (Some(year), None, None) => {
            check_year(year)?;
            filters.push(format!("year = {year}"));
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
                (Some(from), Some(to)) => filters.push(format!("year {from} TO {to}")),
                (Some(from), None) => filters.push(format!("year >= {from}")),
                (None, Some(to)) => filters.push(format!("year <= {to}")),
                (None, None) => {}
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
        filters.push(format!("studios = {}", filter_string(studio)));
    }
    match (&query.person, &query.role) {
        (None, Some(_)) => {
            return Err(AppError::BadRequest("role requires person".into()));
        }
        (Some(person), role) => {
            validate::slug("person", person)?;
            if let Some(role) = role {
                validate::role(role)?;
            }
            // Заглушка: участников в документе нет (компактный индекс). Когда появятся — здесь
            // фильтр по ним.
            return Err(
                AppError::Unavailable("search by people is not available yet".into())
                    .with_code("person_filter_unavailable"),
            );
        }
        (None, None) => {}
    }
    Ok(filters)
}

/// Строка в кавычках для фильтра Meilisearch: `\` и `"` экранируются.
fn filter_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Подсказки студий для фильтра `studio` (facet search Meilisearch по полю `studios`): по части
/// названия с опечатками; без `q` — самые частые. `count` — сколько видимых сущностей в индексе.
#[utoipa::path(
    get, operation_id = "search_studios", path = "/search/studios", tag = "catalog",
    params(StudiosQuery),
    responses(
        (status = 200, description = "Студии", body = Vec<StudioHit>),
        (status = 400, description = "Слишком длинный `q`", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
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
    let limit = query.limit.unwrap_or(10).clamp(1, 50) as usize;
    let body = json!({
        "facetName": "studios",
        "facetQuery": q,
        "filter": query.kind.map(|kind| format!("kind = {}", kind.as_str())),
    });
    let path = format!("/indexes/{}/facet-search", state.search.index(INDEX));
    let response = match state.search.call(Method::POST, &path, Some(&body)).await {
        Ok(response) => response,
        Err(error) if error.code() == Some("index_not_found") => json!({ "facetHits": [] }),
        Err(SearchError::Disabled) => return Err(unavailable()),
        Err(error) => {
            tracing::warn!(%error, "studio facet search failed");
            return Err(unavailable());
        }
    };
    let hits = response["facetHits"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(Json(
        hits.into_iter()
            .take(limit)
            .filter_map(|hit| {
                Some(StudioHit {
                    name: hit["value"].as_str()?.to_string(),
                    count: hit["count"].as_i64()?,
                })
            })
            .collect(),
    ))
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
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
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
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
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
    fn version_names() {
        assert_eq!(
            version_ms("entities", "entities_v1759912345123"),
            Some(1_759_912_345_123)
        );
        assert_eq!(version_ms("t_entities", "t_entities_v12"), Some(12));
        assert_eq!(version_ms("entities", "entities"), None);
        assert_eq!(version_ms("entities", "entities_reindex"), None);
        assert_eq!(version_ms("entities", "other_entities_v1"), None);
    }

    fn query(params: Value) -> SearchQuery {
        serde_json::from_value(params).unwrap()
    }

    #[test]
    fn filters() {
        let ok = |params| filters_of(&query(params)).unwrap();
        assert_eq!(ok(json!({ "year": 2001 })), ["year = 2001"]);
        assert_eq!(
            ok(json!({ "year_from": 2000, "year_to": 2010 })),
            ["year 2000 TO 2010"]
        );
        assert_eq!(ok(json!({ "year_from": 2000 })), ["year >= 2000"]);
        assert_eq!(
            ok(json!({ "year_to": 2010, "kind": "movie" })),
            ["kind = movie", "year <= 2010"]
        );
        // Кавычка и обратный слеш не выходят из строки фильтра.
        assert_eq!(
            ok(json!({ "studio": r#" A" OR kind = book \"# })),
            [r#"studios = "A\" OR kind = book \\""#]
        );
        for bad in [
            json!({ "year": 2001, "year_from": 2000 }),
            json!({ "year_from": 2010, "year_to": 2000 }),
            json!({ "year_from": 0 }),
            json!({ "studio": " " }),
            json!({ "role": "director" }),
            json!({ "person": "Bad Slug" }),
            json!({ "person": "christopher-nolan", "role": "Director" }),
        ] {
            let error = filters_of(&query(bad.clone())).unwrap_err();
            assert_eq!(error.code(), "validation", "{bad}");
        }
        let error = filters_of(&query(
            json!({ "person": "christopher-nolan", "role": "director" }),
        ))
        .unwrap_err();
        assert_eq!(error.code(), "person_filter_unavailable");
    }

    #[test]
    fn steps_follow_options() {
        let steps = ReindexOptions::default().steps();
        assert_eq!(steps.last(), Some(&Step::Rotate));
        let options = ReindexOptions {
            revalidate: true,
            ..ReindexOptions::default()
        };
        assert_eq!(options.steps().last(), Some(&Step::Isr));
    }
}
