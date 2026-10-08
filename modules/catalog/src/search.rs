//! Поиск по каталогу через Meilisearch.
//!
//! Источник правды — PostgreSQL, индекс `entities` производный и нужен только для поиска:
//! карточки (`GET /entities/{slug}`, `GET /people/{slug}`) читаются из БД. В документе пока
//! остаётся и полная карточка `card`, её никто не читает.
//!
//! После каждой записи в админке затронутые сущности переотправляются в индекс
//! ([`update_index`] из [`crate::publish::run`]) с ожиданием применения. Ошибка Meilisearch
//! запись не отменяет, только пишется в лог и в поток публикации.
//!
//! **Перестройка** ([`reindex`]) — без простоя и без очистки живого индекса. Поиск всегда
//! читает индекс `entities` (роль алиаса: в Meilisearch 1.15 алиасов нет):
//! 1. создаётся теневой `entities_v<unix ms>`;
//! 2. все сущности из БД пачками по [`BATCH`] уходят в него (`progress` в потоке);
//! 3. новый индекс должен быть не меньше [`MIN_SIZE_PERCENT`]% текущего — иначе поиск не
//!    переключается (сломанная или неполная выборка), теневой удаляется; `force` — пропустить;
//! 4. `swap-indexes`: `entities` получает новые документы атомарно для всех инстансов, а
//!    `entities_v<ms>` — предыдущую версию (откат — swap обратно);
//! 5. ротация: хранятся текущая и одна предыдущая версия, `entities_v*` старше удаляются;
//! 6. фронтенду — запустить пересборку всей статики (`{"all": true}`; он отвечает `202` и
//!    пересобирает в фоне), только при ручном запуске.
//!
//! Перестройка одна за раз: в процессе — набор занятых индексов, между инстансами — блокировка
//! в Redis. Правки во время перестройки пишутся и в теневой индекс (в пределах инстанса).

use crate::entities::{check_year, details};
use crate::jobs::{self, DoneEvent, JobEvent, Reporter, Step, StepStatus};
use crate::models::{
    page_bounds, Entity, EntityDetail, EntityKind, EntitySummary, Page, ReindexQuery,
    ReindexResult, SearchQuery, ENTITY_COLUMNS,
};
use crate::publish::Touched;
use crate::validate;
use axum::extract::State;
use axum::http::Method;
use axum::response::Response;
use axum::Json;
use chrono::{Datelike, NaiveDate};
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

/// Новый индекс меньше этой доли текущего — поиск на него не переключается.
pub const MIN_SIZE_PERCENT: u64 = 80;

/// Перестройка по расписанию: одна на все инстансы за интервал.
const SCHEDULE_LOCK: &str = "catalog:reindex-lock";
/// Идёт перестройка (любая): одна на все инстансы. TTL — на случай падения процесса.
const RUNNING_LOCK: &str = "catalog:reindex-running";
const RUNNING_LOCK_TTL: Duration = Duration::from_secs(2 * 60 * 60);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Документ индекса: поля для отображения в выдаче, для поиска и карточка.
#[derive(Debug, Serialize)]
struct SearchDoc {
    id: Uuid,
    kind: EntityKind,
    slug: String,
    title: String,
    original_title: Option<String>,
    description: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    /// Для фильтра `year = 2021`.
    year: Option<i32>,
    /// Slug'и тегов: фильтр.
    tags: Vec<String>,
    /// Названия тегов: поиск (в порядке slug'ов).
    tag_names: Vec<String>,
    /// Имена участников: поиск («вильнёв» находит его фильмы).
    people: Vec<String>,
    /// Карточка целиком. Никем не читается (карточка — из БД и статики), пока остаётся в
    /// документе. Не ищется и не отдаётся поиском.
    card: EntityDetail,
}

impl From<EntityDetail> for SearchDoc {
    fn from(card: EntityDetail) -> Self {
        let entity = &card.entity;
        let mut tags: Vec<(String, String)> = card
            .tags
            .iter()
            .map(|tag| (tag.slug.clone(), tag.name.clone()))
            .collect();
        tags.sort_unstable();
        let mut people: Vec<String> = card
            .credits
            .iter()
            .map(|credit| credit.person.full_name.clone())
            .collect();
        people.sort_unstable();
        people.dedup();
        Self {
            id: entity.id,
            kind: entity.kind,
            slug: entity.slug.clone(),
            title: entity.title.clone(),
            original_title: entity.original_title.clone(),
            description: entity.description.clone(),
            release_date: entity.release_date,
            cover_url: entity.cover_url.clone(),
            year: entity.release_date.map(|date| date.year()),
            tag_names: tags.iter().map(|(_, name)| name.clone()).collect(),
            tags: tags.into_iter().map(|(slug, _)| slug).collect(),
            people,
            card,
        }
    }
}

fn entity_settings() -> Value {
    json!({
        // Порядок задаёт вес: совпадение в названии важнее, чем в описании.
        "searchableAttributes": ["title", "original_title", "people", "tag_names", "description"],
        "filterableAttributes": ["kind", "tags", "year", "slug"],
    })
}

/// Документы пачки сущностей: два запроса на карточки, без запроса на каждую.
async fn docs_of(db: &sqlx::PgPool, entities: Vec<Entity>) -> Result<Vec<SearchDoc>, BoxError> {
    let cards = details(db, entities)
        .await
        .map_err(|error| error.to_string())?;
    Ok(cards.into_iter().map(SearchDoc::from).collect())
}

/// Переотправляет затронутое в индекс (и в теневой, если идёт перестройка) и ждёт применения;
/// удалённое из БД удаляет из индекса. Вызывается из [`crate::publish::run`] после коммита.
pub(crate) async fn update_index(state: &AppState, touched: &Touched) -> Result<(), BoxError> {
    if touched.entities.is_empty() {
        return Ok(());
    }
    let entities: Vec<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS} FROM entities e WHERE e.id = ANY($1) ORDER BY e.id"
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
async fn upsert_and_delete(
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
    /// Переключить поиск, даже если новый индекс меньше [`MIN_SIZE_PERCENT`]% текущего.
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
        new: u64,
        current: u64,
    },
    Failed(String),
}

impl fmt::Display for ReindexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "search is disabled"),
            Self::Busy => write!(f, "reindex is already running"),
            Self::TooSmall { new, current } => write!(
                f,
                "new index has {new} documents, current has {current}: less than \
                 {MIN_SIZE_PERCENT}%, search was not switched (use force=true if intended)"
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

fn shadow_of(main: &str) -> Option<String> {
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
    redis: bool,
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
        let mut lock = Self { main, redis: false };
        match state.cache.try_lock(RUNNING_LOCK, RUNNING_LOCK_TTL).await {
            Ok(true) => lock.redis = true,
            Ok(false) => return Err(ReindexError::Busy),
            // Без Redis не узнать про другие инстансы: перестраиваем, как раньше.
            Err(error) => tracing::warn!(%error, "reindex lock unavailable, rebuilding anyway"),
        }
        Ok(lock)
    }

    pub async fn release(mut self, state: &AppState) {
        if std::mem::take(&mut self.redis) {
            state.cache.unlock(RUNNING_LOCK).await;
        }
    }
}

impl Drop for ReindexLock {
    fn drop(&mut self) {
        if let Some(running) = RUNNING.lock().unwrap().as_mut() {
            running.remove(&self.main);
        }
        set_shadow(&self.main, None);
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
    let main = lock.main.clone();
    let version = format!("{main}_v{}", unix_ms());
    let fail = |step: Step, started: Instant, error: String| {
        report.finish(step, StepStatus::Failed, error.clone(), started);
        ReindexError::Failed(error)
    };

    // 1. Теневой индекс. Основной создаётся, если его нет: swap требует оба.
    let started = report.start(Step::CreateIndex);
    create_version(search, &main, &version)
        .await
        .map_err(|error| fail(Step::CreateIndex, started, error.to_string()))?;
    set_shadow(&main, Some(version.clone()));
    report.finish(
        Step::CreateIndex,
        StepStatus::Done,
        format!("Создан теневой индекс {version}"),
        started,
    );

    // 2. Все сущности пачками.
    let started = report.start(Step::Fill);
    let indexed = match fill(state, &version, report).await {
        Ok(indexed) => indexed,
        Err(error) => {
            drop_index(search, &version).await;
            return Err(fail(Step::Fill, started, error.to_string()));
        }
    };
    report.finish(
        Step::Fill,
        StepStatus::Done,
        format!("Проиндексировано {indexed} сущностей"),
        started,
    );

    // 3. Не переключать на подозрительно маленький индекс.
    let started = report.start(Step::Check);
    let sizes = async {
        Ok::<_, SearchError>((
            documents(search, &version).await?,
            documents(search, &main).await?,
        ))
    };
    let (new, current) = match sizes.await {
        Ok(sizes) => sizes,
        Err(error) => {
            drop_index(search, &version).await;
            return Err(fail(Step::Check, started, error.to_string()));
        }
    };
    let percent = (new * 100).checked_div(current).unwrap_or(100);
    if new * 100 < current * MIN_SIZE_PERCENT {
        if !options.force {
            drop_index(search, &version).await;
            report.finish(
                Step::Check,
                StepStatus::Failed,
                format!(
                    "Новый индекс: {new} документов, текущий: {current} ({percent}%). Нужно не \
                     меньше {MIN_SIZE_PERCENT}% — поиск не переключён, теневой индекс удалён. \
                     Если сущности удалены намеренно — повторить с force=true"
                ),
                started,
            );
            return Err(ReindexError::TooSmall { new, current });
        }
        report.finish(
            Step::Check,
            StepStatus::Done,
            format!(
                "Новый индекс: {new} документов, текущий: {current} ({percent}%) — меньше \
                 {MIN_SIZE_PERCENT}%, но force=true"
            ),
            started,
        );
    } else {
        report.finish(
            Step::Check,
            StepStatus::Done,
            format!("Новый индекс: {new} документов, текущий: {current} ({percent}%)"),
            started,
        );
    }

    // 4. Переключение: entities ⇄ entities_v<ms>.
    let started = report.start(Step::Swap);
    search
        .call_and_wait(
            Method::POST,
            "/swap-indexes",
            Some(&json!([{ "indexes": [main, version] }])),
        )
        .await
        .map_err(|error| fail(Step::Swap, started, error.to_string()))?;
    // До сих пор правки шли и в теневой: ни одна не осталась только в старой версии.
    set_shadow(&main, None);
    report.finish(
        Step::Swap,
        StepStatus::Done,
        format!("Поиск переключён на новый индекс; предыдущая версия сохранена в {version}"),
        started,
    );

    // 5. Ротация: текущая (main) и предыдущая (version), всё старше — удалить.
    let started = report.start(Step::Rotate);
    let deleted = rotate(search, &main, &version)
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

async fn create_version(search: &Search, main: &str, version: &str) -> Result<(), SearchError> {
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
            Some(&entity_settings()),
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

/// Все сущности из БД пачками по [`BATCH`] (keyset по id) в индекс `version`. Пока Meilisearch
/// применяет пачку, читается следующая. Возвращает число отправленных документов.
async fn fill(state: &AppState, version: &str, report: &Reporter) -> Result<usize, BoxError> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM entities")
        .fetch_one(&state.db)
        .await?;
    let total = u64::try_from(total).unwrap_or(0);
    report.progress(Step::Fill, 0, total);

    let mut after: Option<Uuid> = None;
    let mut indexed: usize = 0;
    let mut pending: Option<(Value, usize)> = None;
    loop {
        let entities: Vec<Entity> = sqlx::query_as(&format!(
            "SELECT {ENTITY_COLUMNS} FROM entities e
             WHERE $1::uuid IS NULL OR e.id > $1 ORDER BY e.id LIMIT $2"
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

/// Полнотекстовый поиск по каталогу: опечатки, ранжирование, фильтры. Сортировка по релевантности.
///
/// Индекс обновляется после изменений в админке с задержкой в доли секунды.
#[utoipa::path(
    get, path = "/search", tag = "catalog",
    params(SearchQuery),
    responses(
        (status = 200, description = "Страница результатов. `total` — оценка", body = Page<EntitySummary>),
        (status = 400, description = "Неверный фильтр", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Page<EntitySummary>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let mut filters = Vec::new();
    if let Some(kind) = query.kind {
        filters.push(format!("kind = {}", kind.as_str()));
    }
    if let Some(tag) = &query.tag {
        // Проверенный slug безопасно подставлять в строку фильтра.
        validate::slug("tag", tag)?;
        filters.push(format!("tags = \"{tag}\""));
    }
    if let Some(year) = query.year {
        check_year(year)?;
        filters.push(format!("year = {year}"));
    }
    let body = json!({
        "q": query.q.as_deref().unwrap_or_default().trim(),
        "filter": if filters.is_empty() { Value::Null } else { filters.join(" AND ").into() },
        "limit": limit,
        "offset": offset,
        "attributesToRetrieve":
            ["id", "kind", "slug", "title", "original_title", "release_date", "cover_url"],
    });

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
