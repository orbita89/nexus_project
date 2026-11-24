//! Индекс людей Tantivy. Документ — человек с хотя бы одной работой; в `card` лежит готовый
//! JSON карточки (как `PersonHit`), чтобы выдача не ходила в БД.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use shared::tantivy::{
    fuzzy_query, normalize_key, open_versioned, strict_query, TextSpec, RU_RAW, RU_STEM,
};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value as _, FAST, STORED,
    STRING,
};
use tantivy::{
    Index, IndexReader, IndexWriter, Order, ReloadPolicy, SegmentReader, TantivyDocument,
    TantivyError, Term,
};

/// Версия схемы и анализаторов; увеличить при любом их изменении.
pub const SCHEMA_VERSION: &str = "people-1";
/// Память писателя: перестройка не должна мешать поиску на слабом железе.
pub const WRITER_BUDGET: usize = 50_000_000;
/// Глубже в выдачу не листаем.
pub const MAX_OFFSET: usize = 100_000;

const BOOST_NAME_KEY: f32 = 6.0;
const BOOST_NAME: f32 = 2.0;
const BOOST_ORIGINAL: f32 = 1.0;

/// Порядок выдачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeopleSort {
    /// Релевантность, при равной — известность; без `q` — известность.
    Relevance,
    Fame,
    Name,
}

/// Человек для индекса.
pub struct PersonEntry {
    pub id: String,
    pub slug: String,
    pub full_name: String,
    pub original_name: Option<String>,
    pub known_for: Option<String>,
    pub fame: f64,
    /// JSON карточки (`PersonHit`).
    pub card: String,
}

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    slug: Field,
    full_name: Field,
    full_name_raw: Field,
    original_name: Field,
    name_key: Field,
    known_for: Field,
    fame: Field,
    name_sort: Field,
    card: Field,
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
        slug: b.add_text_field("slug", STRING | STORED),
        full_name: b.add_text_field("full_name", text(RU_STEM)),
        full_name_raw: b.add_text_field("full_name_raw", text(RU_RAW)),
        original_name: b.add_text_field("original_name", text(RU_RAW)),
        name_key: b.add_text_field("name_key", STRING),
        known_for: b.add_text_field("known_for", STRING | FAST),
        fame: b.add_f64_field("fame", FAST),
        name_sort: b.add_text_field("name_sort", STRING | FAST),
        card: b.add_text_field("card", STORED),
    };
    (b.build(), f)
}

static OPEN: Mutex<Option<HashMap<PathBuf, Arc<PeopleIndex>>>> = Mutex::new(None);

/// Забыть открытый индекс каталога (после смены поколения).
pub fn forget(dir: &Path) {
    if let Some(map) = OPEN.lock().unwrap().as_mut() {
        map.remove(dir);
    }
}

pub struct PeopleIndex {
    index: Index,
    reader: IndexReader,
    f: Fields,
}

impl PeopleIndex {
    /// Индекс в `dir`; нет или другая версия схемы — создаётся пустой.
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

    pub fn is_empty(&self) -> bool {
        self.doc_count() == 0
    }

    fn writer(&self) -> Result<IndexWriter, TantivyError> {
        self.index.writer_with_num_threads(1, WRITER_BUDGET)
    }

    fn document(&self, p: &PersonEntry) -> TantivyDocument {
        let mut d = TantivyDocument::default();
        d.add_text(self.f.id, &p.id);
        d.add_text(self.f.slug, &p.slug);
        d.add_text(self.f.full_name, &p.full_name);
        d.add_text(self.f.full_name_raw, &p.full_name);
        if let Some(original) = &p.original_name {
            d.add_text(self.f.original_name, original);
        }
        d.add_text(self.f.name_key, normalize_key(&p.full_name));
        if let Some(role) = &p.known_for {
            d.add_text(self.f.known_for, role);
        }
        d.add_f64(self.f.fame, p.fame);
        d.add_text(self.f.name_sort, normalize_key(&p.full_name));
        d.add_text(self.f.card, &p.card);
        d
    }

    /// Добавляет (заменяет по `id`) людей и удаляет `delete`; один commit.
    pub fn upsert(&self, people: &[PersonEntry], delete: &[String]) -> Result<(), TantivyError> {
        let mut w = self.writer()?;
        for id in delete.iter().chain(people.iter().map(|p| &p.id)) {
            w.delete_term(Term::from_field_text(self.f.id, id));
        }
        for p in people {
            w.add_document(self.document(p))?;
        }
        w.commit()?;
        drop(w);
        self.reader.reload()
    }

    /// Пачками для перестройки: без удаления прежних, commit — в [`Self::finish`].
    pub fn writer_for_build(&self) -> Result<IndexWriter, TantivyError> {
        self.writer()
    }

    pub fn add(&self, w: &IndexWriter, people: &[PersonEntry]) -> Result<(), TantivyError> {
        for p in people {
            w.add_document(self.document(p))?;
        }
        Ok(())
    }

    pub fn finish(&self, mut w: IndexWriter) -> Result<(), TantivyError> {
        w.commit()?;
        w.wait_merging_threads()?;
        self.reader.reload()
    }

    fn spec(&self) -> (Vec<(Field, f32)>, Vec<(Field, f32)>) {
        (
            vec![(self.f.full_name, BOOST_NAME)],
            vec![
                (self.f.full_name_raw, BOOST_NAME),
                (self.f.original_name, BOOST_ORIGINAL),
            ],
        )
    }

    /// Текстовая часть: строгий запрос, а если он ничего не нашёл — с опечатками. `None` — `q`
    /// пуст (все люди).
    fn text_query(&self, q: &str) -> Result<Option<Box<dyn Query>>, TantivyError> {
        if shared::tantivy::query_terms(q).is_empty() {
            return Ok(None);
        }
        let searcher = self.reader.searcher();
        let (stem, raw) = self.spec();
        let spec = TextSpec {
            stem: &stem,
            raw: &raw,
            key: Some((self.f.name_key, BOOST_NAME_KEY)),
        };
        let strict = strict_query(&self.index, &searcher, q, &spec)?;
        if let Some(strict) = strict {
            if searcher.search(&strict, &Count)? > 0 {
                return Ok(Some(strict));
            }
        }
        Ok(fuzzy_query(&self.index, q, &spec))
    }

    fn role(&self, role: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.f.known_for, role),
            IndexRecordOption::Basic,
        ))
    }

    fn full_query(&self, q: &str, role: Option<&str>) -> Result<Box<dyn Query>, TantivyError> {
        let text = self.text_query(q)?;
        let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        match text {
            Some(t) => must.push((Occur::Must, t)),
            None => must.push((Occur::Must, Box::new(AllQuery))),
        }
        if let Some(role) = role {
            must.push((Occur::Must, self.role(role)));
        }
        Ok(Box::new(BooleanQuery::new(must)))
    }

    /// Страница карточек и общее число найденных.
    pub fn search(
        &self,
        q: &str,
        role: Option<&str>,
        sort: PeopleSort,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Value>, usize), TantivyError> {
        let searcher = self.reader.searcher();
        let query = self.full_query(q, role)?;
        let total = searcher.search(&query, &Count)?;
        if offset >= MAX_OFFSET.min(total) {
            return Ok((Vec::new(), total));
        }
        let top = TopDocs::with_limit(limit).and_offset(offset);
        let by_text = !q.trim().is_empty() && sort == PeopleSort::Relevance;
        let addrs: Vec<tantivy::DocAddress> = if by_text {
            let col = |seg: &SegmentReader| {
                let fame = seg.fast_fields().f64("fame").ok();
                move |doc: u32, score: f32| {
                    let fame = fame.as_ref().and_then(|c| c.first(doc)).unwrap_or(0.0);
                    // Оценка округляется: почти равные по качеству совпадения идут по известности.
                    ((score * 2.0) as i64, fame)
                }
            };
            searcher
                .search(&query, &top.tweak_score(col))?
                .into_iter()
                .map(|(_, a)| a)
                .collect()
        } else if sort == PeopleSort::Name {
            searcher
                .search(
                    &query,
                    &top.order_by_string_fast_field("name_sort", Order::Asc),
                )?
                .into_iter()
                .map(|(_, a)| a)
                .collect()
        } else {
            searcher
                .search(&query, &top.order_by_fast_field::<f64>("fame", Order::Desc))?
                .into_iter()
                .map(|(_, a)| a)
                .collect()
        };
        let mut cards = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(card) = doc.get_first(self.f.card).and_then(|v| v.as_str()) {
                if let Ok(v) = serde_json::from_str(card) {
                    cards.push(v);
                }
            }
        }
        Ok((cards, total))
    }

    /// Людей по основным ролям среди найденных по `q` (роли без людей нет).
    pub fn role_counts(&self, q: &str) -> Result<BTreeMap<String, i64>, TantivyError> {
        let searcher = self.reader.searcher();
        let mut roles: Vec<String> = Vec::new();
        for segment in searcher.segment_readers() {
            let inverted = segment.inverted_index(self.f.known_for)?;
            let mut stream = inverted.terms().stream().map_err(TantivyError::from)?;
            while stream.advance() {
                if let Ok(role) = String::from_utf8(stream.key().to_vec()) {
                    if !roles.contains(&role) {
                        roles.push(role);
                    }
                }
            }
        }
        let text = self.text_query(q)?;
        let mut out = BTreeMap::new();
        for role in roles {
            let mut must: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, self.role(&role))];
            if let Some(t) = &text {
                must.push((Occur::Must, t.box_clone()));
            }
            let n = searcher.search(&BooleanQuery::new(must), &Count)?;
            if n > 0 {
                out.insert(role, n as i64);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u32, name: &str, orig: Option<&str>, role: &str, fame: f64) -> PersonEntry {
        PersonEntry {
            id: format!("id-{n}"),
            slug: format!("p-{n}"),
            full_name: name.into(),
            original_name: orig.map(Into::into),
            known_for: Some(role.into()),
            fame,
            card: serde_json::json!({"id": format!("id-{n}"), "slug": format!("p-{n}"),
                "full_name": name, "known_for": role})
            .to_string(),
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexus-people-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn sample(tag: &str) -> Arc<PeopleIndex> {
        let index = PeopleIndex::open(&dir(tag)).unwrap();
        index
            .upsert(
                &[
                    entry(
                        1,
                        "Андрей Тарковский",
                        Some("Andrei Tarkovsky"),
                        "director",
                        5.0,
                    ),
                    entry(
                        2,
                        "Андрей Звягинцев",
                        Some("Andrey Zvyagintsev"),
                        "director",
                        9.0,
                    ),
                    entry(3, "Андрей Миронов", None, "actor", 7.0),
                ],
                &[],
            )
            .unwrap();
        index
    }

    fn slugs(hits: &[Value]) -> Vec<String> {
        hits.iter()
            .map(|h| h["slug"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn exact_name_beats_fuzzy_and_typo_is_found() {
        let index = sample("exact");
        let (hits, _) = index
            .search("Андрей Тарковский", None, PeopleSort::Relevance, 10, 0)
            .unwrap();
        assert_eq!(slugs(&hits)[0], "p-1");
        let (hits, total) = index
            .search("Тарковкий", None, PeopleSort::Relevance, 10, 0)
            .unwrap();
        assert_eq!((total, slugs(&hits)), (1, vec!["p-1".to_string()]));
    }

    #[test]
    fn role_filter_sort_and_counts() {
        let index = sample("role");
        let (hits, total) = index
            .search("", Some("director"), PeopleSort::Fame, 10, 0)
            .unwrap();
        assert_eq!(
            (total, slugs(&hits)),
            (2, vec!["p-2".to_string(), "p-1".to_string()])
        );
        let counts = index.role_counts("андрей").unwrap();
        assert_eq!(counts["director"], 2);
        assert_eq!(counts["actor"], 1);
        let (hits, _) = index.search("", None, PeopleSort::Name, 10, 0).unwrap();
        assert_eq!(slugs(&hits)[0], "p-2");
    }

    #[test]
    fn latin_name_prefix_and_delete() {
        let index = sample("latin");
        let (hits, _) = index
            .search("tarkov", None, PeopleSort::Relevance, 10, 0)
            .unwrap();
        assert_eq!(slugs(&hits), ["p-1"]);
        index.upsert(&[], &["id-1".to_string()]).unwrap();
        assert_eq!(index.doc_count(), 2);
    }

    #[test]
    fn schema_version_change_recreates_index() {
        let d = dir("version");
        let (schema, _) = schema();
        let index = open_versioned(&d, schema.clone(), "old").unwrap();
        let mut w = index.writer_with_num_threads(1, WRITER_BUDGET).unwrap();
        w.add_document(TantivyDocument::default()).unwrap();
        w.commit().unwrap();
        drop(w);
        let again = open_versioned(&d, schema, "new").unwrap();
        assert_eq!(again.reader().unwrap().searcher().num_docs(), 0);
    }
}
