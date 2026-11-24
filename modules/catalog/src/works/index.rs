//! Индекс работ Tantivy. Документ строится из JSON `SearchDoc`, чтобы
//! оба движка получали одни и те же поля. В `card` лежит `EntitySummary`: выдача читается из
//! индекса и работает при недоступной БД.

use std::collections::HashMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use shared::tantivy::{
    fuzzy_query, normalize_key, open_versioned, strict_query, TextSpec, RU_RAW, RU_STEM,
};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, RangeQuery, TermQuery, TermSetQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value as _, FAST, INDEXED,
    STORED, STRING,
};
use tantivy::{
    DocAddress, Index, IndexReader, IndexWriter, Order, ReloadPolicy, SegmentReader,
    TantivyDocument, TantivyError, Term,
};

pub const SCHEMA_VERSION: &str = "works-1";
pub const WRITER_BUDGET: usize = 50_000_000;
/// Глубже в выдачу не листаем.
pub const MAX_OFFSET: usize = 100_000;

const BOOST_TITLE_KEY: f32 = 8.0;
const BOOST_TITLE: f32 = 2.0;
const BOOST_ORIGINAL: f32 = 1.5;
const BOOST_SIDE: f32 = 0.4;

/// Поля `SearchDoc`, которые образуют `card` (`EntitySummary`).
const SUMMARY_FIELDS: [&str; 12] = [
    "id",
    "kind",
    "slug",
    "title",
    "original_title",
    "release_date",
    "cover_url",
    "cover_width",
    "cover_height",
    "release_status",
    "imdb_rating",
    "imdb_votes",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorksSort {
    /// Релевантность (с `q`) или «Рекомендуемые» (без).
    Relevance,
    Recommended,
    Trending,
    TopRated,
    New,
    Soon,
    Title,
    Popular,
    Rating,
    Awarded,
}

/// Диапазон лет.
#[derive(Debug, Clone, Copy, Default)]
pub struct YearRange {
    pub from: Option<i32>,
    pub to: Option<i32>,
}

/// Запрос к индексу работ. Пустые фильтры не действуют.
#[derive(Debug, Clone, Default)]
pub struct WorksQuery {
    pub q: String,
    /// Типы (`movie`, ...), ИЛИ между собой.
    pub kinds: Vec<String>,
    /// Коды стран, ИЛИ между собой.
    pub countries: Vec<String>,
    /// Теги, ИЛИ между собой (`tag` — один; `similar` — несколько).
    pub tags: Vec<String>,
    pub years: YearRange,
    pub studio: Option<String>,
    pub publisher: Option<String>,
    pub museum: Option<String>,
    pub studio_slug: Option<String>,
    pub award: Option<String>,
    pub award_year: Option<i32>,
    pub awarded: Option<bool>,
    /// Только эти slug'и (работы человека).
    pub only_slugs: Option<Vec<String>>,
    pub exclude_slugs: Vec<String>,
    /// Граница «вышло / не вышло» для `New` / `Soon` (unix-секунды).
    pub now_ts: i64,
    pub sort: Option<WorksSort>,
}

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    slug: Field,
    kind: Field,
    title: Field,
    title_raw: Field,
    title_key: Field,
    title_sort: Field,
    original_title: Field,
    side_text: Field,
    studios: Field,
    publishers: Field,
    museums: Field,
    countries: Field,
    tags: Field,
    studio_slugs: Field,
    award_slugs: Field,
    award_years: Field,
    awarded: Field,
    year: Field,
    release_ts: Field,
    rank_score: Field,
    popularity: Field,
    quality: Field,
    rating_avg: Field,
    ratings_count: Field,
    award_wins: Field,
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
        kind: b.add_text_field("kind", STRING),
        title: b.add_text_field("title", text(RU_STEM)),
        title_raw: b.add_text_field("title_raw", text(RU_RAW)),
        title_key: b.add_text_field("title_key", STRING),
        title_sort: b.add_text_field("title_sort", STRING | FAST),
        original_title: b.add_text_field("original_title", text(RU_RAW)),
        side_text: b.add_text_field("side_text", text(RU_RAW)),
        studios: b.add_text_field("studios", STRING),
        publishers: b.add_text_field("publishers", STRING),
        museums: b.add_text_field("museums", STRING),
        countries: b.add_text_field("countries", STRING),
        tags: b.add_text_field("tags", STRING),
        studio_slugs: b.add_text_field("studio_slugs", STRING),
        award_slugs: b.add_text_field("award_slugs", STRING),
        award_years: b.add_text_field("award_years", STRING),
        awarded: b.add_text_field("awarded", STRING),
        year: b.add_i64_field("year", INDEXED),
        release_ts: b.add_f64_field("release_ts", INDEXED | FAST),
        rank_score: b.add_f64_field("rank_score", FAST),
        popularity: b.add_f64_field("popularity", FAST),
        quality: b.add_f64_field("quality", FAST),
        rating_avg: b.add_f64_field("nexus_rating_avg", FAST),
        ratings_count: b.add_f64_field("nexus_ratings_count", FAST),
        award_wins: b.add_f64_field("award_wins", FAST),
        card: b.add_text_field("card", STORED),
    };
    (b.build(), f)
}

static OPEN: Mutex<Option<HashMap<PathBuf, Arc<WorksIndex>>>> = Mutex::new(None);

/// Забыть открытый индекс каталога (после смены поколения).
pub fn forget(dir: &Path) {
    if let Some(map) = OPEN.lock().unwrap().as_mut() {
        map.remove(dir);
    }
}

pub struct WorksIndex {
    index: Index,
    reader: IndexReader,
    f: Fields,
    /// Ответы поиска; сбрасываются при любой записи в индекс.
    cache: Mutex<HashMap<String, (Vec<Value>, usize)>>,
}

/// Сколько ответов поиска держать в памяти.
const CACHE_ENTRIES: usize = 1024;

fn strings(doc: &Value, key: &str) -> Vec<String> {
    doc[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

impl WorksIndex {
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
        let this = Arc::new(Self {
            index,
            reader,
            f,
            cache: Mutex::new(HashMap::new()),
        });
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

    /// Документ из JSON `SearchDoc`.
    fn document(&self, v: &Value) -> Option<TantivyDocument> {
        let f = &self.f;
        let id = v["id"].as_str()?;
        let slug = v["slug"].as_str()?;
        let title = v["title"].as_str().unwrap_or_default();
        let mut d = TantivyDocument::default();
        d.add_text(f.id, id);
        d.add_text(f.slug, slug);
        d.add_text(f.kind, v["kind"].as_str().unwrap_or_default());
        d.add_text(f.title, title);
        d.add_text(f.title_raw, title);
        d.add_text(f.title_key, normalize_key(title));
        d.add_text(f.title_sort, normalize_key(title));
        if let Some(o) = v["original_title"].as_str() {
            d.add_text(f.original_title, o);
        }
        for (key, field) in [
            ("studios", f.studios),
            ("publishers", f.publishers),
            ("museums", f.museums),
            ("countries", f.countries),
            ("tags", f.tags),
            ("studio_slugs", f.studio_slugs),
            ("award_slugs", f.award_slugs),
        ] {
            for value in strings(v, key) {
                d.add_text(field, value);
            }
        }
        for key in ["studios", "publishers", "museums"] {
            for value in strings(v, key) {
                d.add_text(f.side_text, value);
            }
        }
        for year in v["award_years"].as_array().into_iter().flatten() {
            if let Some(y) = year.as_i64() {
                d.add_text(f.award_years, y.to_string());
            }
        }
        let wins = v["award_wins"].as_f64().unwrap_or(0.0);
        d.add_text(f.awarded, if wins > 0.0 { "1" } else { "0" });
        if let Some(y) = v["year"].as_i64() {
            d.add_i64(f.year, y);
        }
        if let Some(ts) = v["release_ts"].as_f64() {
            d.add_f64(f.release_ts, ts);
        }
        for (key, field) in [
            ("rank_score", f.rank_score),
            ("popularity", f.popularity),
            ("quality", f.quality),
            ("nexus_rating_avg", f.rating_avg),
        ] {
            if let Some(x) = v[key].as_f64() {
                d.add_f64(field, x);
            }
        }
        d.add_f64(
            f.ratings_count,
            v["nexus_ratings_count"].as_f64().unwrap_or(0.0),
        );
        d.add_f64(f.award_wins, wins);
        let mut card = serde_json::Map::new();
        for key in SUMMARY_FIELDS {
            if let Some(x) = v.get(key) {
                card.insert(key.to_owned(), x.clone());
            }
        }
        d.add_text(f.card, Value::Object(card).to_string());
        Some(d)
    }

    /// Заменяет по `id` документы `docs` и удаляет `delete` (id); один commit.
    pub fn upsert(&self, docs: &[Value], delete: &[String]) -> Result<(), TantivyError> {
        let mut w = self.writer()?;
        for id in delete {
            w.delete_term(Term::from_field_text(self.f.id, id));
        }
        for doc in docs {
            if let Some(d) = self.document(doc) {
                w.add_document(d)?;
            }
        }
        w.commit()?;
        drop(w);
        self.cache.lock().unwrap().clear();
        self.reader.reload()
    }

    pub fn writer_for_build(&self) -> Result<IndexWriter, TantivyError> {
        self.writer()
    }

    pub fn add(&self, w: &IndexWriter, docs: &[Value]) -> Result<(), TantivyError> {
        for doc in docs {
            if let Some(d) = self.document(doc) {
                w.add_document(d)?;
            }
        }
        Ok(())
    }

    pub fn finish(&self, mut w: IndexWriter) -> Result<(), TantivyError> {
        w.commit()?;
        w.wait_merging_threads()?;
        self.cache.lock().unwrap().clear();
        self.reader.reload()
    }

    fn term(&self, field: Field, value: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, value),
            IndexRecordOption::Basic,
        ))
    }

    fn any_of(&self, field: Field, values: &[String]) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(
            values
                .iter()
                .map(|v| (Occur::Should, self.term(field, v)))
                .collect(),
        ))
    }

    /// Текстовая часть: строгий запрос или (`fuzzy`) с опечатками. `None` — `q` пуст.
    fn text_query(
        &self,
        q: &str,
        searcher: &tantivy::Searcher,
        fuzzy: bool,
    ) -> Result<Option<Box<dyn Query>>, TantivyError> {
        if shared::tantivy::query_terms(q).is_empty() {
            return Ok(None);
        }
        let stem = [(self.f.title, BOOST_TITLE)];
        let raw = [
            (self.f.title_raw, BOOST_TITLE),
            (self.f.original_title, BOOST_ORIGINAL),
            (self.f.side_text, BOOST_SIDE),
        ];
        let spec = TextSpec {
            stem: &stem,
            raw: &raw,
            key: Some((self.f.title_key, BOOST_TITLE_KEY)),
        };
        if fuzzy {
            Ok(fuzzy_query(&self.index, q, &spec))
        } else {
            strict_query(&self.index, searcher, q, &spec)
        }
    }

    /// Фильтры без текста.
    fn filters(&self, q: &WorksQuery) -> Vec<(Occur, Box<dyn Query>)> {
        let f = &self.f;
        let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        if !q.kinds.is_empty() {
            must.push((Occur::Must, self.any_of(f.kind, &q.kinds)));
        }
        if !q.countries.is_empty() {
            must.push((Occur::Must, self.any_of(f.countries, &q.countries)));
        }
        if !q.tags.is_empty() {
            must.push((Occur::Must, self.any_of(f.tags, &q.tags)));
        }
        for (field, value) in [
            (f.studios, &q.studio),
            (f.publishers, &q.publisher),
            (f.museums, &q.museum),
            (f.studio_slugs, &q.studio_slug),
            (f.award_slugs, &q.award),
        ] {
            if let Some(v) = value {
                must.push((Occur::Must, self.term(field, v)));
            }
        }
        if let Some(y) = q.award_year {
            must.push((Occur::Must, self.term(f.award_years, &y.to_string())));
        }
        if let Some(a) = q.awarded {
            must.push((Occur::Must, self.term(f.awarded, if a { "1" } else { "0" })));
        }
        if q.years.from.is_some() || q.years.to.is_some() {
            let bound = |y: Option<i32>| match y {
                Some(y) => Bound::Included(Term::from_field_i64(f.year, i64::from(y))),
                None => Bound::Unbounded,
            };
            must.push((
                Occur::Must,
                Box::new(RangeQuery::new(bound(q.years.from), bound(q.years.to))),
            ));
        }
        let ts = q.now_ts as f64;
        match q.sort {
            Some(WorksSort::New) => must.push((
                Occur::Must,
                Box::new(RangeQuery::new(
                    Bound::Unbounded,
                    Bound::Included(Term::from_field_f64(f.release_ts, ts)),
                )),
            )),
            Some(WorksSort::Soon) => must.push((
                Occur::Must,
                Box::new(RangeQuery::new(
                    Bound::Excluded(Term::from_field_f64(f.release_ts, ts)),
                    Bound::Unbounded,
                )),
            )),
            _ => {}
        }
        if let Some(slugs) = &q.only_slugs {
            must.push((
                Occur::Must,
                Box::new(TermSetQuery::new(
                    slugs.iter().map(|s| Term::from_field_text(f.slug, s)),
                )),
            ));
        }
        for slug in &q.exclude_slugs {
            must.push((Occur::MustNot, self.term(f.slug, slug)));
        }
        must
    }

    fn full_query(
        &self,
        q: &WorksQuery,
        searcher: &tantivy::Searcher,
        fuzzy: bool,
    ) -> Result<Box<dyn Query>, TantivyError> {
        let mut must = self.filters(q);
        match self.text_query(&q.q, searcher, fuzzy)? {
            Some(t) => must.push((Occur::Must, t)),
            None => must.insert(0, (Occur::Must, Box::new(AllQuery))),
        }
        Ok(Box::new(BooleanQuery::new(must)))
    }

    /// Всего найдено.
    pub fn count(&self, q: &WorksQuery) -> Result<usize, TantivyError> {
        let searcher = self.reader.searcher();
        let mut total = searcher.search(&self.full_query(q, &searcher, false)?, &Count)?;
        if total == 0 && !q.q.trim().is_empty() {
            total = searcher.search(&self.full_query(q, &searcher, true)?, &Count)?;
        }
        Ok(total)
    }

    /// Ключ сортировки: (поле, знак) до трёх, по убыванию ключа; без значения — в конец.
    fn sort_keys(&self, sort: WorksSort, has_text: bool) -> (bool, [(&'static str, f64); 3]) {
        const NONE: (&str, f64) = ("", 0.0);
        let keys = match sort {
            WorksSort::Relevance if has_text => (true, [("rank_score", 1.0), NONE, NONE]),
            WorksSort::Relevance | WorksSort::Recommended => {
                (false, [("rank_score", 1.0), ("release_ts", 1.0), NONE])
            }
            WorksSort::Trending => (false, [("popularity", 1.0), ("rank_score", 1.0), NONE]),
            WorksSort::TopRated => (false, [("quality", 1.0), ("rank_score", 1.0), NONE]),
            WorksSort::New => (false, [("release_ts", 1.0), ("rank_score", 1.0), NONE]),
            WorksSort::Soon => (false, [("release_ts", -1.0), ("rank_score", 1.0), NONE]),
            WorksSort::Popular => (
                false,
                [("nexus_ratings_count", 1.0), ("release_ts", 1.0), NONE],
            ),
            WorksSort::Rating => (
                false,
                [
                    ("nexus_rating_avg", 1.0),
                    ("nexus_ratings_count", 1.0),
                    ("release_ts", 1.0),
                ],
            ),
            WorksSort::Awarded => (false, [("award_wins", 1.0), ("rank_score", 1.0), NONE]),
            WorksSort::Title => (false, [NONE, NONE, NONE]),
        };
        keys
    }

    /// Страница карточек (`EntitySummary` в JSON).
    pub fn search(
        &self,
        q: &WorksQuery,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Value>, TantivyError> {
        Ok(self.page(q, limit, offset)?.0)
    }

    /// Страница карточек и сколько всего нашлось — одним проходом по индексу. Строгий запрос не
    /// нашёл ничего — повтор с опечатками. Ответы кэшируются до следующей записи в индекс.
    pub fn page(
        &self,
        q: &WorksQuery,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Value>, usize), TantivyError> {
        if offset >= MAX_OFFSET || limit == 0 {
            return Ok((Vec::new(), self.count(q)?));
        }
        let key = format!("{q:?}|{limit}|{offset}");
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return Ok(hit.clone());
        }
        let searcher = self.reader.searcher();
        let mut found = self.run(
            &searcher,
            &self.full_query(q, &searcher, false)?,
            q,
            limit,
            offset,
        )?;
        if found.1 == 0 && !q.q.trim().is_empty() {
            found = self.run(
                &searcher,
                &self.full_query(q, &searcher, true)?,
                q,
                limit,
                offset,
            )?;
        }
        let (addrs, total) = found;
        let mut cards = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(card) = doc.get_first(self.f.card).and_then(|v| v.as_str()) {
                if let Ok(v) = serde_json::from_str(card) {
                    cards.push(v);
                }
            }
        }
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (cards.clone(), total));
        Ok((cards, total))
    }

    /// Один проход: адреса страницы в порядке сортировки и общее число совпадений.
    fn run(
        &self,
        searcher: &tantivy::Searcher,
        query: &dyn Query,
        q: &WorksQuery,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<DocAddress>, usize), TantivyError> {
        let top = TopDocs::with_limit(limit).and_offset(offset);
        let sort = q.sort.unwrap_or(WorksSort::Relevance);
        if sort == WorksSort::Title {
            let (hits, total) = searcher.search(
                query,
                &(
                    top.order_by_string_fast_field("title_sort", Order::Asc),
                    Count,
                ),
            )?;
            return Ok((hits.into_iter().map(|(_, a)| a).collect(), total));
        }
        let (by_score, keys) = self.sort_keys(sort, !q.q.trim().is_empty());
        let tweak = move |seg: &SegmentReader| {
            let cols: Vec<Option<tantivy::columnar::Column<f64>>> = keys
                .iter()
                .map(|(name, _)| {
                    (!name.is_empty())
                        .then(|| seg.fast_fields().f64(name).ok())
                        .flatten()
                })
                .collect();
            move |doc: u32, score: f32| {
                let mut key = [f64::NEG_INFINITY; 4];
                // Релевантность — первым ключом (округлена: близкие по качеству совпадения
                // упорядочивает рейтинг).
                key[0] = if by_score {
                    f64::from((score * 2.0).floor())
                } else {
                    0.0
                };
                for (i, col) in cols.iter().enumerate() {
                    key[i + 1] = match col {
                        Some(c) => c.first(doc).map_or(f64::NEG_INFINITY, |x| x * keys[i].1),
                        None => 0.0,
                    };
                }
                key
            }
        };
        let (hits, total) = searcher.search(query, &(top.tweak_score(tweak), Count))?;
        Ok((hits.into_iter().map(|(_, a)| a).collect(), total))
    }

    /// Slug'и в порядке выдачи — для «соседей»: до `limit` начиная с `offset`.
    pub fn slugs(
        &self,
        q: &WorksQuery,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<String>, TantivyError> {
        Ok(self
            .search(q, limit, offset)?
            .iter()
            .filter_map(|c| c["slug"].as_str().map(str::to_owned))
            .collect())
    }

    /// Карточки по slug'ам (порядок как в `slugs`).
    pub fn by_slugs(&self, slugs: &[String]) -> Result<Vec<Value>, TantivyError> {
        let q = WorksQuery {
            only_slugs: Some(slugs.to_vec()),
            ..WorksQuery::default()
        };
        let found = self.search(&q, slugs.len().max(1), 0)?;
        Ok(slugs
            .iter()
            .filter_map(|s| {
                found
                    .iter()
                    .find(|c| c["slug"].as_str() == Some(s))
                    .cloned()
            })
            .collect())
    }

    /// Подсказки фасета: значения поля с числом работ (опционально внутри `kind`), по части
    /// значения без регистра; без `q` — самые частые.
    pub fn facet(
        &self,
        field: &str,
        q: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, i64)>, TantivyError> {
        let field = match field {
            "studios" => self.f.studios,
            "publishers" => self.f.publishers,
            "museums" => self.f.museums,
            "countries" => self.f.countries,
            "tags" => self.f.tags,
            _ => return Ok(Vec::new()),
        };
        let searcher = self.reader.searcher();
        let needle = normalize_key(q);
        let mut freq: HashMap<String, u64> = HashMap::new();
        for segment in searcher.segment_readers() {
            let inverted = segment.inverted_index(field)?;
            let mut stream = inverted.terms().stream().map_err(TantivyError::from)?;
            while stream.advance() {
                let Ok(value) = std::str::from_utf8(stream.key()) else {
                    continue;
                };
                if !needle.is_empty() && !normalize_key(value).contains(&needle) {
                    continue;
                }
                *freq.entry(value.to_owned()).or_default() += u64::from(stream.value().doc_freq);
            }
        }
        let mut values: Vec<(String, u64)> = freq.into_iter().collect();
        values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if let Some(kind) = kind {
            // Счётчик внутри типа: пересчитывается по верхушке словаря.
            values.truncate(2_000);
            let mut counted = Vec::new();
            for (value, _) in values {
                let query = BooleanQuery::new(vec![
                    (Occur::Must, self.term(field, &value)),
                    (Occur::Must, self.term(self.f.kind, kind)),
                ]);
                let n = searcher.search(&query, &Count)?;
                if n > 0 {
                    counted.push((value, n as u64));
                }
            }
            counted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            values = counted;
        }
        Ok(values
            .into_iter()
            .take(limit)
            .map(|(v, n)| (v, n as i64))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(n: u32, kind: &str, title: &str, f: Value) -> Value {
        let mut v = json!({
            "id": format!("id-{n}"), "slug": format!("w-{n}"), "kind": kind, "title": title,
            "release_status": "released", "tags": [], "studios": [], "publishers": [],
            "museums": [], "countries": [], "studio_slugs": [], "award_slugs": [],
            "award_years": [], "award_wins": 0, "nexus_ratings_count": 0,
        });
        for (k, x) in f.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    fn sample(tag: &str) -> Arc<WorksIndex> {
        let d = std::env::temp_dir().join(format!("nexus-works-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let index = WorksIndex::open(&d).unwrap();
        index
            .upsert(
                &[
                    doc(
                        1,
                        "movie",
                        "Матрица",
                        json!({"rank_score": 5.0, "quality": 8.0, "year": 1999,
                        "release_ts": 922_000_000.0, "tags": ["sci-fi"], "countries": ["US"],
                        "studios": ["Warner Bros."], "popularity": 1.0, "award_wins": 4,
                        "award_slugs": ["oscar"], "award_years": [2000]}),
                    ),
                    doc(
                        2,
                        "movie",
                        "Матрица: Перезагрузка",
                        json!({"rank_score": 3.0, "quality": 9.0,
                        "year": 2003, "release_ts": 1_050_000_000.0, "tags": ["sci-fi"],
                        "popularity": 9.0}),
                    ),
                    doc(
                        3,
                        "game",
                        "Ведьмак 3",
                        json!({"rank_score": 9.0, "year": 2015,
                        "release_ts": 1_430_000_000.0, "publishers": ["CD Projekt"]}),
                    ),
                    doc(
                        4,
                        "movie",
                        "Скоро",
                        json!({"rank_score": 1.0, "year": 2099,
                        "release_ts": 4_000_000_000.0}),
                    ),
                ],
                &[],
            )
            .unwrap();
        index
    }

    fn slugs(index: &WorksIndex, q: &WorksQuery) -> Vec<String> {
        index.slugs(q, 20, 0).unwrap()
    }

    #[test]
    fn sorts_and_filters() {
        let index = sample("sorts");
        let q = |f: &dyn Fn(&mut WorksQuery)| {
            let mut q = WorksQuery {
                now_ts: 2_000_000_000,
                ..Default::default()
            };
            f(&mut q);
            q
        };
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::Recommended))),
            ["w-3", "w-1", "w-2", "w-4"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::TopRated)))[..2],
            ["w-2", "w-1"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::Trending)))[..2],
            ["w-2", "w-1"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::New)))[0],
            "w-3"
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::Soon))),
            ["w-4"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::Title)))[0],
            "w-3"
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.sort = Some(WorksSort::Awarded)))[0],
            "w-1"
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.kinds = vec!["game".into()])),
            ["w-3"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.tags = vec!["sci-fi".into()])).len(),
            2
        );
        assert_eq!(
            slugs(
                &index,
                &q(&|q| {
                    q.years.from = Some(2000);
                    q.years.to = Some(2010);
                })
            ),
            ["w-2"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.studio = Some("Warner Bros.".into()))),
            ["w-1"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.award = Some("oscar".into()))),
            ["w-1"]
        );
        assert_eq!(slugs(&index, &q(&|q| q.award_year = Some(2000))), ["w-1"]);
        assert_eq!(slugs(&index, &q(&|q| q.awarded = Some(false))).len(), 3);
        assert_eq!(
            slugs(&index, &q(&|q| q.only_slugs = Some(vec!["w-2".into()]))),
            ["w-2"]
        );
        assert_eq!(
            slugs(
                &index,
                &q(&|q| {
                    q.tags = vec!["sci-fi".into()];
                    q.exclude_slugs = vec!["w-1".into()];
                })
            ),
            ["w-2"]
        );
        assert_eq!(
            slugs(&index, &q(&|q| q.countries = vec!["US".into()])),
            ["w-1"]
        );
    }

    #[test]
    fn text_search_exact_typo_and_facets() {
        let index = sample("text");
        let q = |text: &str| WorksQuery {
            q: text.into(),
            ..Default::default()
        };
        assert_eq!(slugs(&index, &q("матрица"))[0], "w-1");
        assert_eq!(slugs(&index, &q("матрица перезагрузка")), ["w-2"]);
        assert_eq!(slugs(&index, &q("ведмак")), ["w-3"]);
        assert_eq!(index.count(&q("матрица")).unwrap(), 2);
        let facet = index.facet("publishers", "cd pr", None, 10).unwrap();
        assert_eq!(facet, [("CD Projekt".to_string(), 1)]);
        let tags = index.facet("tags", "", Some("movie"), 10).unwrap();
        assert_eq!(tags, [("sci-fi".to_string(), 2)]);
    }

    #[test]
    fn delete_and_cards() {
        let index = sample("cards");
        index.upsert(&[], &["id-1".to_string()]).unwrap();
        assert_eq!(index.doc_count(), 3);
        let cards = index.by_slugs(&["w-3".to_string()]).unwrap();
        assert_eq!(cards[0]["title"], "Ведьмак 3");
        assert!(cards[0].get("rank_score").is_none());
    }
}

#[cfg(test)]
mod facet_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn museums_and_publishers_facets_by_kind() {
        let d = std::env::temp_dir().join(format!("nexus-works-facets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let index = WorksIndex::open(&d).unwrap();
        let base = |n: u32, kind: &str, extra: serde_json::Value| {
            let mut v = json!({"id": format!("i{n}"), "slug": format!("s{n}"), "kind": kind,
                "title": format!("Работа {n}")});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            v
        };
        index
            .upsert(
                &[
                    base(1, "painting", json!({"museums": ["Лувр"]})),
                    base(2, "painting", json!({"museums": ["Лувр"]})),
                    base(3, "painting", json!({"museums": ["Эрмитаж"]})),
                    base(
                        4,
                        "game",
                        json!({"publishers": ["Valve"], "museums": ["Лувр"]}),
                    ),
                ],
                &[],
            )
            .unwrap();
        let all = index.facet("museums", "", Some("painting"), 10).unwrap();
        assert_eq!(all, [("Лувр".to_string(), 2), ("Эрмитаж".to_string(), 1)]);
        assert_eq!(index.facet("museums", "эрм", None, 10).unwrap().len(), 1);
        assert_eq!(
            index.facet("publishers", "", Some("game"), 10).unwrap(),
            [("Valve".to_string(), 1)]
        );
        let q = WorksQuery {
            museum: Some("Лувр".into()),
            kinds: vec!["painting".into()],
            ..Default::default()
        };
        assert_eq!(index.count(&q).unwrap(), 2);
    }
}
