//! Наполнение и обновление индекса работ Tantivy: перестройка в новое поколение каталога и
//! точечные правки по id. Правки во время перестройки повторяются после переключения.

use std::path::Path;
use std::sync::{Arc, Mutex};

use shared::tantivy::generations;
use shared::AppState;
use uuid::Uuid;

use super::index::{forget, WorksIndex};
use crate::jobs::{Reporter, Step};
use crate::models::{Entity, ENTITY_COLUMNS, ENTITY_STATS_JOIN, VISIBLE};
use crate::search::{docs_of, BoxError, BATCH};

const NAME: &str = "works";
/// Какие виды живут в индексе работ.
const KINDS: &str = "e.kind IN ('movie', 'series', 'game', 'painting')";

static JOURNAL: Mutex<Option<Vec<Uuid>>> = Mutex::new(None);

/// Индекс работ строится: задан `SEARCH_INDEX_DIR`.
pub fn enabled(state: &AppState) -> bool {
    state.config.search_index_dir.is_some()
}

/// Индекс текущего поколения; `None` — выключен.
pub fn open(state: &AppState) -> Option<Result<Arc<WorksIndex>, tantivy::TantivyError>> {
    let root = state.config.search_index_dir.as_deref()?;
    Some(WorksIndex::open(&generations::current(
        Path::new(root),
        NAME,
    )))
}

/// Ошибка перестройки: новое поколение слишком мало (не переключено) или сбой.
#[derive(Debug)]
pub enum BuildError {
    TooSmall { new: u64 },
    Other(BoxError),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooSmall { new } => write!(f, "new index has {new} documents: too small"),
            Self::Other(error) => write!(f, "{error}"),
        }
    }
}

impl<E: Into<BoxError>> From<E> for BuildError {
    fn from(error: E) -> Self {
        Self::Other(error.into())
    }
}

/// Документов в текущем поколении (0 — индекса нет).
pub fn doc_count(state: &AppState) -> u64 {
    match open(state) {
        Some(Ok(index)) => index.doc_count(),
        _ => 0,
    }
}

/// Перестройка из БД в новое поколение `works.v<ms>`; возвращает число документов.
pub async fn rebuild(state: &AppState, report: &Reporter) -> Result<u64, BoxError> {
    rebuild_with(state, report, false)
        .await
        .map_err(|e| e.to_string().into())
}

/// То же; `force` — переключить, даже если новый индекс меньше `SEARCH_REINDEX_MIN_PERCENT`%.
pub async fn rebuild_with(
    state: &AppState,
    report: &Reporter,
    force: bool,
) -> Result<u64, BuildError> {
    let root = state
        .config
        .search_index_dir
        .clone()
        .ok_or("SEARCH_INDEX_DIR is not set")?;
    let root = Path::new(&root).to_path_buf();
    std::fs::create_dir_all(&root)?;
    *JOURNAL.lock().unwrap() = Some(Vec::new());
    let result = build(state, &root, report, force).await;
    let journal = JOURNAL.lock().unwrap().take().unwrap_or_default();
    let built = result?;
    if !journal.is_empty() {
        update(state, &journal).await?;
    }
    Ok(built)
}

async fn build(
    state: &AppState,
    root: &Path,
    report: &Reporter,
    force: bool,
) -> Result<u64, BuildError> {
    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM entities e WHERE {VISIBLE} AND {KINDS}"
    ))
    .fetch_one(&state.db)
    .await?;
    let total = u64::try_from(total).unwrap_or(0);
    report.progress(Step::Works, 0, total);
    let old = generations::current(root, NAME);
    let dir = generations::fresh(root, NAME);
    let index = WorksIndex::open(&dir)?;
    let writer = index.writer_for_build()?;
    let mut after: Option<Uuid> = None;
    let mut done: u64 = 0;
    loop {
        let entities: Vec<Entity> = sqlx::query_as(&format!(
            "SELECT {ENTITY_COLUMNS} FROM entities e {ENTITY_STATS_JOIN}
             WHERE {VISIBLE} AND {KINDS} AND ($1::uuid IS NULL OR e.id > $1)
             ORDER BY e.id LIMIT $2"
        ))
        .bind(after)
        .bind(BATCH as i64)
        .fetch_all(&state.db)
        .await?;
        let Some(last) = entities.last() else { break };
        after = Some(last.id);
        let docs = docs_of(&state.db, entities).await?;
        let docs = serde_json::to_value(&docs)?;
        let docs = docs.as_array().cloned().unwrap_or_default();
        index.add(&writer, &docs)?;
        done += docs.len() as u64;
        report.progress(Step::Works, done, total.max(done));
    }
    index.finish(writer)?;
    // Защита: новое поколение не меньше процента от старого.
    let percent = state.config.search_reindex_min_percent;
    let previous = WorksIndex::open(&old).map(|i| i.doc_count()).unwrap_or(0);
    if !force && previous > 0 && done * 100 < previous * percent {
        forget(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        return Err(BuildError::TooSmall { new: done });
    }
    generations::switch(root, NAME, &dir)?;
    forget(&old);
    Ok(done)
}

/// Правка затронула сущности: видимые работы переписываются, остальные удаляются из индекса.
pub async fn update(state: &AppState, ids: &[Uuid]) -> Result<(), BoxError> {
    if ids.is_empty() {
        return Ok(());
    }
    if let Some(journal) = JOURNAL.lock().unwrap().as_mut() {
        journal.extend_from_slice(ids);
    }
    let Some(index) = open(state) else {
        return Ok(());
    };
    let index = index?;
    let entities: Vec<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS} FROM entities e {ENTITY_STATS_JOIN}
         WHERE e.id = ANY($1) AND {VISIBLE} AND {KINDS} ORDER BY e.id"
    ))
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    let docs = docs_of(&state.db, entities).await?;
    let docs = serde_json::to_value(&docs)?;
    let docs = docs.as_array().cloned().unwrap_or_default();
    let delete: Vec<String> = ids.iter().map(ToString::to_string).collect();
    shared::tantivy::blocking(move || index.upsert(&docs, &delete)).await?;
    Ok(())
}

/// Индекс пуст (первый запуск, новая схема) — строится в фоне из БД.
pub fn spawn_initial(state: AppState) {
    let Some(index) = open(&state) else { return };
    tokio::spawn(async move {
        match index {
            Ok(index) if index.is_empty() => {
                tracing::info!("works index is empty, building it from the database");
                match rebuild(&state, &Reporter::silent()).await {
                    Ok(n) => tracing::info!(works = n, "works index built"),
                    Err(error) => tracing::warn!(%error, "works index build failed"),
                }
            }
            Ok(index) => tracing::info!(works = index.doc_count(), "works index opened"),
            Err(error) => tracing::warn!(%error, "works index open failed"),
        }
    });
}
