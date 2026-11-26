//! Админка поисковых индексов Tantivy: один список на все индексы (`catalog` — единый индекс
//! книг, фильмов, сериалов, игр, картин и людей, `collections`, `archive`) — размер, документы, сегменты, поколения, ожидаемое число из БД — и
//! операции над ними (перестройка, слияние сегментов). Подробности книжного индекса — в
//! [`super::books`] (`GET /admin/books/index`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{Path as UrlPath, Query};
use shared::tantivy::generations;
use shared::{AdminUser, AppError, AppResult, AppState};
use tantivy::Index;
use utoipa::{IntoParams, ToSchema};

use super::books::{IndexFieldUsage, IndexOperation};
use crate::jobs::Reporter;
use crate::search::ReindexLock;

/// Идентификаторы индексов.
const IDS: [&str; 3] = ["catalog", "collections", "archive"];

/// Поколение каталога индекса (`<name>.v<ms>`).
#[derive(Debug, Serialize, ToSchema)]
pub struct GenerationInfo {
    #[schema(example = "works.v1795475848838")]
    pub name: String,
    /// Его читает поиск.
    pub current: bool,
    pub bytes: u64,
}

/// Состояние одного индекса.
#[derive(Debug, Serialize, ToSchema)]
pub struct SearchIndexInfo {
    /// `works`, `people`, `books`, `collections`, `archive`.
    #[schema(example = "works")]
    pub id: String,
    #[schema(example = "Работы")]
    pub title: String,
    /// Что в индексе.
    pub description: String,
    /// Каталог индекса задан (`SEARCH_INDEX_DIR`, `BOOKS_INDEX_DIR`, `ARCHIVE_DIR`).
    pub enabled: bool,
    /// Каталог текущего поколения.
    pub dir: Option<String>,
    /// Документов в индексе (`null` — индекса ещё нет).
    pub docs: Option<u64>,
    /// Сколько должно быть по БД (`null` — не считается).
    pub db_docs: Option<i64>,
    pub segments: u64,
    pub deleted_docs: u64,
    /// Всё, что занимает индекс по данным Tantivy.
    pub index_bytes: u64,
    /// Хранилище документов (поле `card`, сжато целиком — по полям не делится).
    pub store_bytes: u64,
    /// Из чего состоит индекс: место по полям (словарь, документы, позиции, колонки, нормы).
    pub fields: Vec<IndexFieldUsage>,
    /// Размер текущего поколения на диске.
    pub disk_bytes: u64,
    /// Версия схемы (файл `nexus-schema`).
    pub schema_version: Option<String>,
    /// Все поколения каталога; у книг и архива — одно.
    pub generations: Vec<GenerationInfo>,
    pub can_rebuild: bool,
    pub can_merge: bool,
    /// Перестройка не здесь, а по этому адресу (коллекции живут в `social`).
    pub rebuild_path: Option<String>,
    /// Единый индекс: документы по видам (`book`, `movie`, …, `person`) и сколько должно быть по БД.
    pub kinds: Vec<IndexKindInfo>,
    /// Единый индекс: слой Б (RAM) — сущности, люди, оценка памяти.
    pub sidecar: Option<IndexSidecarInfo>,
    /// Текущая или последняя операция в этом процессе.
    pub operation: Option<IndexOperation>,
}

/// Документы одного вида в едином индексе.
#[derive(Debug, Serialize, ToSchema)]
pub struct IndexKindInfo {
    #[schema(example = "movie")]
    pub kind: String,
    pub docs: u64,
    /// Сколько должно быть по БД.
    pub db_docs: Option<i64>,
}

/// Слой Б единого индекса.
#[derive(Debug, Serialize, ToSchema)]
pub struct IndexSidecarInfo {
    /// Видимые сущности и люди в RAM.
    pub entities: u64,
    pub people: u64,
    /// Оценка памяти: массивы, порядки, битмапы фасетов, люди.
    pub entities_bytes: u64,
    pub people_bytes: u64,
    /// Фасеты сущностей: префикс ключа → значения и байты.
    pub facets: Vec<IndexFacetInfo>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IndexFacetInfo {
    #[schema(example = "tag")]
    pub kind: String,
    pub values: u64,
    pub bytes: u64,
}

static OPERATIONS: Mutex<Option<HashMap<String, IndexOperation>>> = Mutex::new(None);

fn operation(id: &str) -> Option<IndexOperation> {
    OPERATIONS.lock().unwrap().as_ref()?.get(id).cloned()
}

fn begin(id: &str, op: &str) -> AppResult<()> {
    let mut ops = OPERATIONS.lock().unwrap();
    let ops = ops.get_or_insert_with(HashMap::new);
    if ops.get(id).is_some_and(|o| o.finished_at.is_none()) {
        return Err(AppError::Conflict(
            "index operation is already running".into(),
        ));
    }
    ops.insert(
        id.to_owned(),
        IndexOperation {
            op: op.to_owned(),
            started_at: Utc::now(),
            finished_at: None,
            ok: None,
            error: None,
            progress: None,
        },
    );
    Ok(())
}

fn finish(id: &str, result: Result<(), String>) {
    if let Some(op) = OPERATIONS
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|ops| ops.get_mut(id))
    {
        op.finished_at = Some(Utc::now());
        op.ok = Some(result.is_ok());
        op.error = result.err();
    }
}

/// Ход операции: шаг и счётчик из событий [`Reporter`].
fn track(id: String, mut events: tokio::sync::mpsc::UnboundedReceiver<crate::jobs::JobEvent>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let progress = match event {
                crate::jobs::JobEvent::Progress(p) => super::books::IndexProgress {
                    step: p.step.title().to_owned(),
                    done: p.done,
                    total: p.total,
                },
                _ => continue,
            };
            if let Some(op) = OPERATIONS
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|ops| ops.get_mut(&id))
            {
                op.progress = Some(progress);
            }
        }
    });
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_file() { m.len() } else { 0 })
        .sum()
}

/// Каталоги индекса: (текущий, корень поколений и имя). Книги и архив — без поколений.
struct Location {
    current: PathBuf,
    root: Option<(PathBuf, &'static str)>,
}

fn location(state: &AppState, id: &str) -> Option<Location> {
    let c = &state.config;
    match id {
        "catalog" => Some(Location {
            current: crate::index::sync::dir(state)?,
            root: None,
        }),
        "archive" => Some(Location {
            current: PathBuf::from(c.archive_dir.as_deref()?),
            root: None,
        }),
        "collections" => {
            let root = PathBuf::from(c.search_index_dir.as_deref()?);
            let name = IDS.iter().find(|n| **n == id)?;
            Some(Location {
                current: generations::current(&root, name),
                root: Some((root, name)),
            })
        }
        _ => None,
    }
}

struct Stats {
    docs: u64,
    segments: u64,
    deleted: u64,
    index_bytes: u64,
    store_bytes: u64,
    fields: Vec<IndexFieldUsage>,
}

fn inspect(dir: &Path) -> Option<Stats> {
    let index = Index::open_in_dir(dir).ok()?;
    let searcher = index.reader().ok()?.searcher();
    let docs = searcher.num_docs();
    let max: u64 = searcher
        .segment_readers()
        .iter()
        .map(|s| u64::from(s.max_doc()))
        .sum();
    let usage = searcher.space_usage().ok()?;
    let mut by_field: std::collections::BTreeMap<String, [u64; 5]> = Default::default();
    let mut store = 0u64;
    for segment in usage.segments() {
        let parts = [
            segment.termdict(),
            segment.postings(),
            segment.positions(),
            segment.fast_fields(),
            segment.fieldnorms(),
        ];
        for (slot, per_field) in parts.into_iter().enumerate() {
            for field in per_field.fields() {
                by_field.entry(field.field_name().to_string()).or_default()[slot] +=
                    field.total().get_bytes();
            }
        }
        store += segment.store().total().get_bytes();
    }
    let mut fields: Vec<IndexFieldUsage> = by_field
        .into_iter()
        .map(|(field, b)| IndexFieldUsage {
            field,
            total: b.iter().sum(),
            termdict: b[0],
            postings: b[1],
            positions: b[2],
            fast: b[3],
            fieldnorms: b[4],
        })
        .collect();
    fields.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.field.cmp(&b.field)));
    Some(Stats {
        docs,
        segments: searcher.segment_readers().len() as u64,
        deleted: max.saturating_sub(docs),
        index_bytes: usage.total().get_bytes(),
        store_bytes: store,
        fields,
    })
}

fn generation_list(location: &Location) -> Vec<GenerationInfo> {
    let Some((root, name)) = &location.root else {
        return vec![GenerationInfo {
            name: location
                .current
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned(),
            current: true,
            bytes: dir_bytes(&location.current),
        }];
    };
    let prefix = format!("{name}.v");
    let mut list: Vec<GenerationInfo> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            n.starts_with(&prefix).then(|| GenerationInfo {
                current: e.path() == location.current,
                bytes: dir_bytes(&e.path()),
                name: n,
            })
        })
        .collect();
    list.sort_by(|a, b| b.name.cmp(&a.name));
    list
}

/// Сколько документов должно быть по БД: подсчёты тяжёлые (десятки секунд на большом каталоге),
/// а админка опрашивает список раз в 3 с, пока идёт операция. Без кэша опросы накладывались,
/// занимали весь пул и перестройка падала с `pool timed out`. Считаем не чаще раза в
/// [`DB_COUNTS_TTL`] и одним запросом на всех (мьютекс: остальные ждут готовый результат).
#[derive(Clone, Default)]
struct DbCounts {
    entities: Option<i64>,
    people: Option<i64>,
    kinds: HashMap<&'static str, Option<i64>>,
}

const DB_COUNTS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

static DB_COUNTS: std::sync::OnceLock<tokio::sync::Mutex<Option<(std::time::Instant, DbCounts)>>> =
    std::sync::OnceLock::new();

async fn db_counts(state: &AppState) -> DbCounts {
    let mut cached = DB_COUNTS
        .get_or_init(|| tokio::sync::Mutex::new(None))
        .lock()
        .await;
    if let Some((at, counts)) = cached.as_ref() {
        if at.elapsed() < DB_COUNTS_TTL {
            return counts.clone();
        }
    }
    let totals = crate::index::sync::db_counts(&state.db).await.ok();
    let mut counts = DbCounts {
        entities: totals.map(|(entities, _, _)| entities),
        people: totals.map(|(_, _, people)| people),
        kinds: HashMap::new(),
    };
    for kind in crate::models::EntityKind::ALL {
        if kind == crate::models::EntityKind::Season {
            continue;
        }
        let n = crate::index::sync::db_kind_count(&state.db, kind)
            .await
            .ok();
        counts.kinds.insert(kind.as_str(), n);
    }
    *cached = Some((std::time::Instant::now(), counts.clone()));
    counts
}

async fn db_docs(state: &AppState, id: &str) -> Option<i64> {
    match id {
        "catalog" => {
            let counts = db_counts(state).await;
            Some(counts.entities? + counts.people?)
        }
        _ => None,
    }
}

/// Единый индекс: документы по видам, сколько должно быть по БД, слой Б.
async fn catalog_details(state: &AppState) -> (Vec<IndexKindInfo>, Option<IndexSidecarInfo>) {
    let Some(Ok(index)) = crate::index::sync::open(state) else {
        return (Vec::new(), None);
    };
    let counted = {
        let index = index.clone();
        tokio::task::spawn_blocking(move || index.stats().ok())
            .await
            .ok()
            .flatten()
    };
    let counts = db_counts(state).await;
    let mut kinds = Vec::new();
    for kind in crate::models::EntityKind::ALL {
        if kind == crate::models::EntityKind::Season {
            continue;
        }
        let docs = counted
            .as_ref()
            .and_then(|s| s.kinds.iter().find(|(k, _)| k == kind.as_str()))
            .map_or(0, |(_, n)| *n);
        let db_docs = counts.kinds.get(kind.as_str()).copied().flatten();
        kinds.push(IndexKindInfo {
            kind: kind.as_str().to_owned(),
            docs,
            db_docs,
        });
    }
    kinds.push(IndexKindInfo {
        kind: crate::index::PERSON_KIND.to_owned(),
        docs: counted
            .as_ref()
            .and_then(|s| s.kinds.iter().find(|(k, _)| k == crate::index::PERSON_KIND))
            .map_or(0, |(_, n)| *n),
        db_docs: counts.people,
    });
    let entities = index.entities().stats();
    let people = index.people().stats();
    let sidecar = IndexSidecarInfo {
        entities: entities.entities,
        people: people.people,
        entities_bytes: entities.meta_bytes + entities.orders_bytes + entities.facets_bytes,
        people_bytes: people.bytes,
        facets: entities
            .facets
            .into_iter()
            .map(|(kind, g)| IndexFacetInfo {
                kind,
                values: g.values,
                bytes: g.bytes,
            })
            .collect(),
    };
    (kinds, Some(sidecar))
}

fn meta(id: &str) -> (&'static str, &'static str) {
    match id {
        "catalog" => (
            "Каталог",
            "Единый индекс: книги, фильмы, сериалы, игры, картины и люди. Слой А — текст, карточки \
             и фильтры с большим числом значений в Tantivy; слой Б — сортировки (файл, mmap) и \
             мелкие фильтры (память)",
        ),
        "collections" => (
            "Коллекции",
            "Публичные коллекции пользователей: название и описание",
        ),
        "archive" => (
            "Холодный архив",
            "Фильмы вне горячего слоя: поиск по названию",
        ),
        _ => ("", ""),
    }
}

async fn info(state: &AppState, id: &str) -> SearchIndexInfo {
    let (title, description) = meta(id);
    let location = location(state, id);
    let db = db_docs(state, id).await;
    let (stats, disk, schema, generations, dir) = match &location {
        Some(l) => {
            let current = l.current.clone();
            let stats = tokio::task::spawn_blocking({
                let current = current.clone();
                move || inspect(&current)
            })
            .await
            .ok()
            .flatten();
            let schema = std::fs::read_to_string(current.join(shared::tantivy::SCHEMA_FILE))
                .ok()
                .map(|s| s.trim().to_owned());
            (
                stats,
                dir_bytes(&current),
                schema,
                generation_list(l),
                Some(current.to_string_lossy().into_owned()),
            )
        }
        None => (None, 0, None, Vec::new(), None),
    };
    let (kinds, sidecar) = if id == "catalog" && location.is_some() {
        catalog_details(state).await
    } else {
        (Vec::new(), None)
    };
    SearchIndexInfo {
        id: id.to_owned(),
        title: title.to_owned(),
        description: description.to_owned(),
        enabled: location.is_some(),
        dir,
        docs: stats.as_ref().map(|s| s.docs),
        db_docs: db,
        segments: stats.as_ref().map_or(0, |s| s.segments),
        deleted_docs: stats.as_ref().map_or(0, |s| s.deleted),
        index_bytes: stats.as_ref().map_or(0, |s| s.index_bytes),
        store_bytes: stats.as_ref().map_or(0, |s| s.store_bytes),
        fields: stats.map(|s| s.fields).unwrap_or_default(),
        disk_bytes: disk,
        schema_version: schema,
        generations,
        can_rebuild: id == "catalog" && location.is_some(),
        can_merge: matches!(id, "catalog" | "collections") && location.is_some(),
        rebuild_path: (id == "collections")
            .then(|| "/social/admin/collections/index/rebuild".to_owned()),
        kinds,
        sidecar,
        operation: operation(id),
    }
}

/// Все поисковые индексы: размер, документы, сегменты, поколения, число по БД, ход операций.
#[utoipa::path(
    get, operation_id = "list_search_indexes", path = "/admin/search/indexes", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Индексы", body = Vec<SearchIndexInfo>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Vec<SearchIndexInfo>>> {
    let (catalog, collections, archive) = tokio::join!(
        info(&state, "catalog"),
        info(&state, "collections"),
        info(&state, "archive"),
    );
    Ok(Json(vec![catalog, collections, archive]))
}

/// Параметры перестройки.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RebuildQuery {
    /// Переключить, даже если новый индекс меньше `SEARCH_REINDEX_MIN_PERCENT`% текущего.
    #[serde(default)]
    pub force: bool,
}

/// Перестроить индекс из БД в фоне: `catalog` (или `all`) — всё, либо только вид (`book`,
/// `movie`, `series`, `game`, `painting`) или `people`. Ход — в `operation` у
/// `GET /admin/search/indexes`. Вид заменяется одним коммитом (поиск не видит дыры); если новых
/// документов меньше `SEARCH_REINDEX_MIN_PERCENT`% прежних, коммита нет (`force` — применить).
#[utoipa::path(
    post, operation_id = "rebuild_search_index", path = "/admin/search/indexes/{id}/rebuild", tag = "catalog-admin",
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "`catalog`, `all`, `book`, `movie`, `series`, `game`, `painting` или `people`"),
        RebuildQuery,
    ),
    responses(
        (status = 202, description = "Перестройка запущена"),
        (status = 400, description = "Неизвестный индекс или его нельзя перестроить здесь", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт операция или перестройка", body = ErrorBody),
        (status = 503, description = "Индекс выключен", body = ErrorBody),
    )
)]
pub async fn rebuild(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<RebuildQuery>,
) -> AppResult<StatusCode> {
    let target = match id.as_str() {
        "catalog" => Some(crate::index::sync::Target::All),
        other => crate::index::sync::Target::parse(other),
    }
    .ok_or_else(|| {
        AppError::BadRequest(
            "id: catalog, all, book, movie, series, game, painting or people".into(),
        )
    })?;
    if location(&state, "catalog").is_none() {
        return Err(AppError::Unavailable("index is not enabled".into()));
    }
    let key = "catalog".to_owned();
    let force = query.force;
    begin(&key, "rebuild")?;
    // Блокировка перестройки — до фона: «уже идёт» — обычный 409.
    let lock = match ReindexLock::acquire(&state).await {
        Ok(lock) => lock,
        Err(error) => {
            finish(&key, Err(error.to_string()));
            return Err(error.into());
        }
    };
    tracing::info!(admin_id = %admin.id, index = %id, force, "search index rebuild started");
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let (reporter, events) = Reporter::channel();
        track(key.clone(), events);
        let result = crate::index::sync::rebuild(&state, target, &reporter, force)
            .await
            .map_err(|e| e.to_string());
        let elapsed_secs = started.elapsed().as_secs_f64();
        match &result {
            Ok(docs) => tracing::info!(index = %key, docs, elapsed_secs, "search index rebuilt"),
            Err(error) => {
                tracing::warn!(index = %key, %error, elapsed_secs, "search index rebuild failed");
            }
        }
        let result = result.map(|_| ());
        lock.release(&state).await;
        finish(&key, result);
    });
    Ok(StatusCode::ACCEPTED)
}

/// Пересобрать слой Б единого индекса (фасеты, сортировки, ручные места, люди) из БД — в фоне,
/// Tantivy не трогается.
#[utoipa::path(
    post, operation_id = "reload_catalog_sidecar", path = "/admin/search/indexes/catalog/sidecar", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 202, description = "Пересборка запущена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт операция", body = ErrorBody),
        (status = 503, description = "Индекс выключен", body = ErrorBody),
    )
)]
pub async fn reload_sidecar(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<StatusCode> {
    if location(&state, "catalog").is_none() {
        return Err(AppError::Unavailable("index is not enabled".into()));
    }
    begin("catalog", "sidecar")?;
    tracing::info!(admin_id = %admin.id, "catalog sidecar reload started");
    tokio::spawn(async move {
        let result = crate::index::sync::reload_sidecar(&state)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        if let Err(error) = &result {
            tracing::warn!(%error, "catalog sidecar reload failed");
        }
        finish("catalog", result);
    });
    Ok(StatusCode::ACCEPTED)
}

/// Слить сегменты индекса в один и убрать старые файлы — в фоне: освобождает место удалённых
/// документов и ускоряет поиск. Данные не меняются.
#[utoipa::path(
    post, operation_id = "merge_search_index", path = "/admin/search/indexes/{id}/merge", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "`catalog` или `collections`")),
    responses(
        (status = 202, description = "Слияние запущено"),
        (status = 400, description = "Неизвестный индекс", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт операция", body = ErrorBody),
        (status = 503, description = "Индекс выключен", body = ErrorBody),
    )
)]
pub async fn merge(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    UrlPath(id): UrlPath<String>,
) -> AppResult<StatusCode> {
    if !matches!(id.as_str(), "catalog" | "collections") {
        return Err(AppError::BadRequest("id: catalog or collections".into()));
    }
    let Some(location) = location(&state, &id) else {
        return Err(AppError::Unavailable("index is not enabled".into()));
    };
    let catalog = if id == "catalog" {
        match crate::index::sync::open(&state) {
            Some(Ok(index)) => Some(index),
            _ => return Err(AppError::Unavailable("index is not enabled".into())),
        }
    } else {
        None
    };
    begin(&id, "merge")?;
    tracing::info!(admin_id = %admin.id, index = %id, "search index merge started");
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || match catalog {
            Some(index) => index.merge().map(|_| ()).map_err(|e| e.to_string()),
            None => shared::tantivy::merge_all(&location.current).map_err(|e| e.to_string()),
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        if let Err(error) = &result {
            tracing::warn!(index = %id, %error, "search index merge failed");
        }
        finish(&id, result);
    });
    Ok(StatusCode::ACCEPTED)
}
