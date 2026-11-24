//! Наполнение и обновление индекса людей Tantivy: полная перестройка в новое поколение каталога
//! и точечные правки по id. Правки, пришедшие во время перестройки, повторяются после переключения.

use std::path::Path;
use std::sync::{Arc, Mutex};

use shared::tantivy::generations;
use shared::AppState;
use uuid::Uuid;

use super::index::{forget, PeopleIndex, PersonEntry};
use crate::jobs::{Reporter, Step};
use crate::people_search::{
    refresh_fame, PersonDoc, BATCH, DOC_COLUMNS, INDEXED_PERSON, WORKS_LATERAL,
};
use crate::search::BoxError;
use crate::works::sync::BuildError;

const NAME: &str = "people";

/// Id, изменённые за время перестройки (`Some` — перестройка идёт).
static JOURNAL: Mutex<Option<Vec<Uuid>>> = Mutex::new(None);

/// Индекс людей строится: задан `SEARCH_INDEX_DIR`.
pub fn enabled(state: &AppState) -> bool {
    state.config.search_index_dir.is_some()
}

/// Индекс текущего поколения. `None` — выключен.
pub fn open(state: &AppState) -> Option<Result<Arc<PeopleIndex>, tantivy::TantivyError>> {
    let root = state.config.search_index_dir.as_deref()?;
    Some(PeopleIndex::open(&generations::current(
        Path::new(root),
        NAME,
    )))
}

fn entry(doc: &PersonDoc) -> Result<PersonEntry, serde_json::Error> {
    Ok(PersonEntry {
        id: doc.id.to_string(),
        slug: doc.slug.clone(),
        full_name: doc.full_name.clone(),
        original_name: doc.original_name.clone(),
        known_for: doc.known_for.clone(),
        fame: doc.fame,
        card: serde_json::to_string(doc)?,
    })
}

/// Документов в текущем поколении (0 — индекса нет).
pub fn doc_count(state: &AppState) -> u64 {
    match open(state) {
        Some(Ok(index)) => index.doc_count(),
        _ => 0,
    }
}

/// Перестройка без `force`.
pub async fn rebuild(state: &AppState, report: &Reporter) -> Result<u64, BoxError> {
    rebuild_with(state, report, false)
        .await
        .map_err(|e| e.to_string().into())
}

/// Перестройка: известность всех людей, затем пачки по [`BATCH`] в новый каталог `people.v<ms>`
/// одним commit, переключение, повтор правок из журнала. Возвращает число людей.
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
    refresh_fame(&state.db, None).await?;
    let total = crate::people_search::count(&state.db).await?;
    report.progress(Step::People, 0, total);
    let old = generations::current(root, NAME);
    let dir = generations::fresh(root, NAME);
    let index = PeopleIndex::open(&dir)?;
    let writer = index.writer_for_build()?;
    let mut after: Option<Uuid> = None;
    let mut done: u64 = 0;
    loop {
        let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
            "SELECT {DOC_COLUMNS}
             FROM (SELECT * FROM people p WHERE {INDEXED_PERSON}
                     AND ($1::uuid IS NULL OR p.id > $1) ORDER BY p.id LIMIT $2) p
             LEFT JOIN person_stats ps ON ps.person_id = p.id
             {WORKS_LATERAL}
             ORDER BY p.id"
        ))
        .bind(after)
        .bind(BATCH as i64)
        .fetch_all(&state.db)
        .await?;
        let Some(last) = docs.last() else { break };
        after = Some(last.id);
        let entries = docs.iter().map(entry).collect::<Result<Vec<_>, _>>()?;
        index.add(&writer, &entries)?;
        done += entries.len() as u64;
        report.progress(Step::People, done, total.max(done));
    }
    index.finish(writer)?;
    // Защита от пустой/обрезанной выдачи: новое поколение не меньше процента от старого.
    let percent = state.config.search_reindex_min_percent;
    let previous = PeopleIndex::open(&old).map(|i| i.doc_count()).unwrap_or(0);
    if !force && previous > 0 && done * 100 < previous * percent {
        forget(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        return Err(BuildError::TooSmall { new: done });
    }
    generations::switch(root, NAME, &dir)?;
    forget(&old);
    Ok(done)
}

/// Правка затронула людей: документы переписываются, скрытые и удалённые — удаляются. Известность
/// пересчитывает вызывающий ([`crate::people_search::update_index`]).
pub async fn update(state: &AppState, people: &[Uuid]) -> Result<(), BoxError> {
    if people.is_empty() {
        return Ok(());
    }
    if let Some(journal) = JOURNAL.lock().unwrap().as_mut() {
        journal.extend_from_slice(people);
    }
    let Some(index) = open(state) else {
        return Ok(());
    };
    let index = index?;
    let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
        "SELECT {DOC_COLUMNS}
         FROM people p LEFT JOIN person_stats ps ON ps.person_id = p.id
         {WORKS_LATERAL}
         WHERE p.id = ANY($1) AND {INDEXED_PERSON} ORDER BY p.id"
    ))
    .bind(people)
    .fetch_all(&state.db)
    .await?;
    let entries = docs.iter().map(entry).collect::<Result<Vec<_>, _>>()?;
    let delete: Vec<String> = people.iter().map(ToString::to_string).collect();
    shared::tantivy::blocking(move || index.upsert(&entries, &delete)).await?;
    Ok(())
}

/// Индекс пуст (первый запуск, новая схема) — строится в фоне из БД.
pub fn spawn_initial(state: AppState) {
    let Some(index) = open(&state) else { return };
    tokio::spawn(async move {
        match index {
            Ok(index) if index.is_empty() => {
                tracing::info!("people index is empty, building it from the database");
                match rebuild(&state, &Reporter::silent()).await {
                    Ok(n) => tracing::info!(people = n, "people index built"),
                    Err(error) => tracing::warn!(%error, "people index build failed"),
                }
            }
            Ok(index) => tracing::info!(people = index.doc_count(), "people index opened"),
            Err(error) => tracing::warn!(%error, "people index open failed"),
        }
    });
}
