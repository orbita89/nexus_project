//! Поиск по коллекциям на Tantivy: индекс `collections` ([`tantivy_index`],
//! `<SEARCH_INDEX_DIR>/collections.v<ms>`).
//!
//! Зачем индекс, а не `ILIKE` по названию: нужны опечатки и поиск по описанию, а подстрока в
//! PostgreSQL этого не даёт (и на росте числа коллекций упирается в последовательный просмотр).
//! Документ компактный — название, описание и дата: выдача всё равно читается из БД, чтобы в
//! ней были счётчики, обложка и автор, как в обычном списке.
//!
//! В индексе только **публичные** коллекции: приватную никто искать не должен, поэтому она не
//! попадает туда вовсе, а не отфильтровывается на поиске. Скрыли коллекцию — документ
//! удаляется ([`update`]).
//!
//! Индекс наполняется при старте приложения ([`spawn_fill`]) и правится при каждой записи
//! (создание, правка, удаление, смена видимости, действия админа). Ошибка индекса запись в БД
//! не отменяет: поиск догонит следующей перестройкой.

use crate::models::PageQuery;
use serde::Serialize;
use shared::{AppError, AppResult, AppState};
use uuid::Uuid;

pub use tantivy_index::CollectionsIndex;

/// Ошибка наполнения: и индекс, и PostgreSQL.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Коллекций в одной пачке при наполнении.
const BATCH: i64 = 1_000;
/// Длина поискового запроса.
pub const MAX_QUERY: usize = 300;

/// Документ коллекции: по названию и описанию ищут, по дате сортируют равные совпадения.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct CollectionDoc {
    id: Uuid,
    title: String,
    description: Option<String>,
    /// `created_at` в unix-секундах.
    created_ts: i64,
}

const DOC_SELECT: &str = "SELECT c.id, c.title, c.description,
                                 extract(epoch FROM c.created_at)::bigint AS created_ts
                          FROM collections c WHERE c.is_public";

/// Наполняет индекс с нуля в новое поколение: все публичные коллекции. Коллекций немного,
/// перестройка занимает доли секунды. Индекс выключен — 0.
pub async fn fill(state: &AppState) -> Result<u64, BoxError> {
    if !tantivy_index::enabled(state) {
        return Ok(0);
    }
    tantivy_index::fill(state).await
}

/// Наполнение при старте: в фоне, чтобы не задерживать запуск. Поиск выключен — ничего.
pub fn spawn_fill(state: AppState) {
    if !tantivy_index::enabled(&state) {
        return;
    }
    tokio::spawn(async move {
        match fill(&state).await {
            Ok(sent) => tracing::info!(collections = sent, "collections index built"),
            Err(error) => tracing::warn!(%error, "collections index build failed"),
        }
    });
}

/// Переписывает изменённые коллекции: публичные обновляются, остальные (приватные и удалённые)
/// убираются из индекса. Вызывается после коммита.
pub async fn update(state: &AppState, ids: &[Uuid]) {
    if ids.is_empty() || !tantivy_index::enabled(state) {
        return;
    }
    if let Err(error) = tantivy_index::update(state, ids).await {
        // Поиск отстанет до следующей перестройки, но запись уже сохранена.
        tracing::warn!(%error, "collections index update failed");
    }
}

/// Ищет публичные коллекции: id в порядке релевантности и сколько всего нашлось.
pub async fn search(state: &AppState, q: &str, query: &PageQuery) -> AppResult<(Vec<Uuid>, i64)> {
    let (limit, offset) = shared::pagination::page_bounds(query.limit, query.offset);
    if q.chars().count() > MAX_QUERY {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_QUERY} characters"
        )));
    }
    if !tantivy_index::enabled(state) {
        return Err(AppError::Unavailable(
            "search is temporarily unavailable".into(),
        ));
    }
    tantivy_index::search(state, q, limit, offset).await
}

/// Индекс коллекций на Tantivy. Коллекций мало: перестройка занимает доли секунды.
pub mod tantivy_index {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use shared::tantivy::{
        fuzzy_query, generations, normalize_key, open_versioned, strict_query, TextSpec, RU_RAW,
        RU_STEM,
    };
    use shared::{AppError, AppResult, AppState};
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::{AllQuery, Query};
    use tantivy::schema::{
        Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value as _, FAST, STORED,
        STRING,
    };
    use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, TantivyError, Term};
    use uuid::Uuid;

    use super::{BoxError, CollectionDoc, BATCH, DOC_SELECT};

    pub const SCHEMA_VERSION: &str = "collections-1";
    const NAME: &str = "collections";
    const WRITER_BUDGET: usize = 20_000_000;

    #[derive(Clone, Copy)]
    struct Fields {
        id: Field,
        title: Field,
        title_raw: Field,
        title_key: Field,
        description: Field,
        created_ts: Field,
    }

    fn text(analyzer: &str) -> TextOptions {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(analyzer)
                .set_index_option(IndexRecordOption::WithFreqs),
        )
    }

    fn schema() -> (Schema, Fields) {
        let mut b = Schema::builder();
        let f = Fields {
            id: b.add_text_field("id", STRING | STORED),
            title: b.add_text_field("title", text(RU_STEM)),
            title_raw: b.add_text_field("title_raw", text(RU_RAW)),
            title_key: b.add_text_field("title_key", STRING),
            description: b.add_text_field("description", text(RU_STEM)),
            created_ts: b.add_i64_field("created_ts", FAST),
        };
        (b.build(), f)
    }

    static OPEN: Mutex<Option<HashMap<PathBuf, Arc<CollectionsIndex>>>> = Mutex::new(None);

    pub struct CollectionsIndex {
        index: Index,
        reader: IndexReader,
        f: Fields,
    }

    impl CollectionsIndex {
        pub fn open(dir: &Path) -> Result<Arc<Self>, TantivyError> {
            let mut open = OPEN.lock().unwrap();
            let open = open.get_or_insert_with(HashMap::new);
            if let Some(index) = open.get(dir) {
                return Ok(index.clone());
            }
            let (schema, f) = schema();
            let index = open_versioned(dir, schema, SCHEMA_VERSION)?;
            let reader = index
                .reader_builder()
                .reload_policy(ReloadPolicy::OnCommitWithDelay)
                .try_into()?;
            let this = Arc::new(Self { index, reader, f });
            open.insert(dir.to_path_buf(), this.clone());
            Ok(this)
        }

        pub fn doc_count(&self) -> u64 {
            self.reader.searcher().num_docs()
        }

        fn document(&self, doc: &CollectionDoc) -> TantivyDocument {
            let mut d = TantivyDocument::default();
            d.add_text(self.f.id, doc.id.to_string());
            d.add_text(self.f.title, &doc.title);
            d.add_text(self.f.title_raw, &doc.title);
            d.add_text(self.f.title_key, normalize_key(&doc.title));
            if let Some(description) = &doc.description {
                d.add_text(self.f.description, description);
            }
            d.add_i64(self.f.created_ts, doc.created_ts);
            d
        }

        /// Заменяет по `id` документы `docs`, удаляет `delete`; один commit.
        pub(super) fn upsert(
            &self,
            docs: &[CollectionDoc],
            delete: &[Uuid],
        ) -> Result<(), TantivyError> {
            let mut w = self.index.writer_with_num_threads(1, WRITER_BUDGET)?;
            for id in delete {
                w.delete_term(Term::from_field_text(self.f.id, &id.to_string()));
            }
            for doc in docs {
                w.add_document(self.document(doc))?;
            }
            w.commit()?;
            drop(w);
            self.reader.reload()
        }

        fn query(&self, q: &str) -> Result<Box<dyn Query>, TantivyError> {
            if shared::tantivy::query_terms(q).is_empty() {
                return Ok(Box::new(AllQuery));
            }
            let searcher = self.reader.searcher();
            let stem = [(self.f.title, 2.0), (self.f.description, 1.0)];
            let raw = [(self.f.title_raw, 2.0)];
            let spec = TextSpec {
                stem: &stem,
                raw: &raw,
                key: Some((self.f.title_key, 6.0)),
            };
            if let Some(strict) = strict_query(&self.index, &searcher, q, &spec)? {
                if searcher.search(&strict, &Count)? > 0 {
                    return Ok(strict);
                }
            }
            // Опечатки — по названию (описание ищется только точно, через стем).
            Ok(fuzzy_query(&self.index, q, &spec).unwrap_or_else(|| Box::new(AllQuery)))
        }

        /// Id в порядке релевантности (при равной — свежие выше) и сколько всего нашлось.
        pub fn search(
            &self,
            q: &str,
            limit: usize,
            offset: usize,
        ) -> Result<(Vec<Uuid>, usize), TantivyError> {
            let searcher = self.reader.searcher();
            let query = self.query(q)?;
            let total = searcher.search(&query, &Count)?;
            let col = |seg: &tantivy::SegmentReader| {
                let ts = seg.fast_fields().i64("created_ts").ok();
                move |doc: u32, score: f32| {
                    let ts = ts.as_ref().and_then(|c| c.first(doc)).unwrap_or(0);
                    ((score * 2.0) as i64, ts)
                }
            };
            let top = TopDocs::with_limit(limit.max(1)).and_offset(offset);
            let found = searcher.search(&query, &top.tweak_score(col))?;
            let mut ids = Vec::with_capacity(found.len());
            for (_, addr) in found {
                let doc: TantivyDocument = searcher.doc(addr)?;
                if let Some(id) = doc.get_first(self.f.id).and_then(|v| v.as_str()) {
                    if let Ok(id) = id.parse() {
                        ids.push(id);
                    }
                }
            }
            Ok((ids, total))
        }
    }

    fn root(state: &AppState) -> Option<PathBuf> {
        state.config.search_index_dir.as_deref().map(PathBuf::from)
    }

    /// Поиск включён: задан `SEARCH_INDEX_DIR`.
    pub fn enabled(state: &AppState) -> bool {
        state.config.search_index_dir.is_some()
    }

    fn open(state: &AppState) -> Result<Arc<CollectionsIndex>, TantivyError> {
        let root = root(state).ok_or_else(|| TantivyError::SystemError("no index dir".into()))?;
        CollectionsIndex::open(&generations::current(&root, NAME))
    }

    /// Перестройка в новое поколение; возвращает число публичных коллекций.
    pub async fn fill(state: &AppState) -> Result<u64, BoxError> {
        let root = root(state).ok_or("SEARCH_INDEX_DIR is not set")?;
        std::fs::create_dir_all(&root)?;
        let dir = generations::fresh(&root, NAME);
        let index = CollectionsIndex::open(&dir)?;
        let mut sent = 0u64;
        let mut after: Option<Uuid> = None;
        loop {
            let docs: Vec<CollectionDoc> = sqlx::query_as(&format!(
                "{DOC_SELECT} AND ($1::uuid IS NULL OR c.id > $1) ORDER BY c.id LIMIT $2"
            ))
            .bind(after)
            .bind(BATCH)
            .fetch_all(&state.db)
            .await?;
            let Some(last) = docs.last() else { break };
            after = Some(last.id);
            sent += docs.len() as u64;
            let index = index.clone();
            shared::tantivy::blocking(move || index.upsert(&docs, &[])).await?;
        }
        let old = generations::current(&root, NAME);
        generations::switch(&root, NAME, &dir)?;
        if let Some(map) = OPEN.lock().unwrap().as_mut() {
            map.remove(&old);
        }
        Ok(sent)
    }

    /// Публичные коллекции из `ids` переписываются, остальные удаляются.
    pub async fn update(state: &AppState, ids: &[Uuid]) -> Result<(), BoxError> {
        let index = open(state)?;
        let docs: Vec<CollectionDoc> = sqlx::query_as(&format!("{DOC_SELECT} AND c.id = ANY($1)"))
            .bind(ids)
            .fetch_all(&state.db)
            .await?;
        let ids = ids.to_vec();
        shared::tantivy::blocking(move || index.upsert(&docs, &ids)).await?;
        Ok(())
    }

    pub async fn search(
        state: &AppState,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> AppResult<(Vec<Uuid>, i64)> {
        let unavailable = || AppError::Unavailable("search is temporarily unavailable".into());
        let index = open(state).map_err(|_| unavailable())?;
        let q = q.to_owned();
        let (ids, total) =
            shared::tantivy::blocking(move || index.search(&q, limit as usize, offset as usize))
                .await
                .map_err(|error: TantivyError| {
                    tracing::warn!(%error, "collections tantivy search failed");
                    unavailable()
                })?;
        Ok((ids, total as i64))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn doc(n: u128, title: &str, description: &str, ts: i64) -> CollectionDoc {
            CollectionDoc {
                id: Uuid::from_u128(n),
                title: title.into(),
                description: Some(description.into()),
                created_ts: ts,
            }
        }

        #[test]
        fn typo_description_delete_and_order() {
            let dir = std::env::temp_dir().join(format!("nexus-coll-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let index = CollectionsIndex::open(&dir).unwrap();
            index
                .upsert(
                    &[
                        doc(1, "Фантастика 90-х", "Лучшие фильмы о будущем", 10),
                        doc(2, "Фантастика новая", "Свежие сериалы", 20),
                        doc(3, "Комедии", "Смешные фильмы про космос", 5),
                    ],
                    &[],
                )
                .unwrap();
            // Опечатка в названии; при равной оценке свежие выше.
            let (ids, total) = index.search("фантастка", 10, 0).unwrap();
            assert_eq!(
                (total, ids),
                (2, vec![Uuid::from_u128(2), Uuid::from_u128(1)])
            );
            // По описанию.
            let (ids, _) = index.search("космос", 10, 0).unwrap();
            assert_eq!(ids, [Uuid::from_u128(3)]);
            // Удаление (скрыли коллекцию).
            index.upsert(&[], &[Uuid::from_u128(3)]).unwrap();
            assert_eq!(index.search("космос", 10, 0).unwrap().1, 0);
            assert_eq!(index.doc_count(), 2);
        }
    }
}
