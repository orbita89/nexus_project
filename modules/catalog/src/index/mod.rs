//! Единый поисковый индекс каталога на Tantivy: книги, фильмы, сериалы, игры, картины и люди в
//! одном индексе `catalog` (`<SEARCH_INDEX_DIR>/catalog`).
//!
//! Источник правды — PostgreSQL, индекс производный: его строит [`sync::rebuild`]
//! (кнопка в админке или `nexus search rebuild`), правки админки и новые оценки досылает
//! [`sync::update`]. Автоматической перестройки нет: пустой индекс при старте — поиск отдаёт
//! пусто и пишет предупреждение.
//!
//! **Два слоя.** Слой А — Tantivy ([`schema`]): текст (название, люди, серия/студии), вид
//! `kind`, плотный номер `ord` и `card` — готовая строка выдачи в JSON, так что списки
//! отдаются без похода в БД. Слой Б — RAM: [`sidecar::EntitySidecar`] (фасеты, сортировки, ручные
//! места сущностей) и [`people_sidecar::PeopleSidecar`] (люди). Запрос без текста Tantivy вообще
//! не вызывает; с текстом Tantivy отдаёт номера и оценки совпадений (сборщик
//! [`collect::OrdHits`] сразу отсеивает документы не из фильтра), порядок строит слой Б.
//!
//! **Как ищет** — [`query`]. Индекс открывается через mmap, писатель живёт только на время
//! записи и берёт [`WRITER_BUDGET`]. Перестройка вида — `delete_term(kind)` + запись + один
//! `commit`: поиск не видит дыры, а гард `SEARCH_REINDEX_MIN_PERCENT` срабатывает до коммита
//! (`rollback`). Повторные запросы отвечает общий кэш: ключ — поколение (отпечаток индекса и
//! слой Б) и запрос.

pub mod collect;
pub mod columns;
pub mod people_sidecar;
pub mod query;
pub mod schema;
pub mod search;
pub mod sidecar;
pub mod sync;
pub mod types;
pub mod weights;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use roaring::RoaringBitmap;
use shared::tantivy::{normalize_key, SCHEMA_FILE};
use tantivy::collector::TopDocs;
use tantivy::directory::error::LockError;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query as TantivyQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, TantivyError, Term,
};

use collect::{OrdHits, OrdSet, Space};
pub use people_sidecar::{PeopleSidecar, PeopleSort, PersonFacts};
use query::{QueryBuilder, MAX_QUERY_CHARS, MIN_QUERY_CHARS};
pub use schema::{PERSON_FLAG, PERSON_KIND, SCHEMA_VERSION};
pub use sidecar::EntitySidecar;
pub use types::{BookForm, Card, EntityDoc, Found, Query, Sort, TextDoc};

use crate::models::{EntityKind, EntitySummary};

/// Память писателя: 50 МБ, один поток. На сервере с малой RAM перестройка индекса из сотен
/// тысяч документов не должна мешать поиску и доходить до OOM killer.
pub const WRITER_BUDGET: usize = 50_000_000;

/// Глубже в выдачу не листаем: Tantivy собирает `offset + limit` лучших.
pub const MAX_OFFSET: usize = 100_000;

/// Сколько ответов поиска держать в памяти. Страница — до 100 коротких карточек (~300 байт):
/// не больше ~4 МБ.
const CACHE_ENTRIES: usize = 128;
/// Страницы длиннее не кэшируются.
const CACHE_MAX_ITEMS: usize = 100;
/// Сколько первых сущностей списка держать в ряду: 1000 — это 40 страниц по 24. Глубже — ряд
/// пересчитывается с удвоенной длиной.
const RANKED_LEN: usize = 1000;
/// Сколько рядов держать: номер — 4 байта, ряд — до [`RANKED_MAX_LEN`]: не больше ~2 МБ.
const RANKED_ENTRIES: usize = 128;
/// Ряды длиннее (глубокие страницы) не кэшируются: иначе один ряд — до 131 тыс. номеров.
const RANKED_MAX_LEN: usize = 4096;
/// Ответов поиска людей в кэше (страница — 24 карточки JSON).
const PEOPLE_CACHE_ENTRIES: usize = 64;
/// Распакованных блоков хранилища карточек на сегмент (блок — 64 КБ). По умолчанию в Tantivy
/// 100 — до 6 МБ на сегмент; страница из 24 карточек распаковывает несколько блоков за ~1 мс.
const DOC_STORE_CACHE_BLOCKS: usize = 4;

/// Списки раздела без текста и фильтров, которые открывают чаще всего: их ряды готовятся
/// заранее ([`CatalogIndex::warm`]).
const WARM_SORTS: [Sort; 5] = [
    Sort::Recommended,
    Sort::New,
    Sort::Title,
    Sort::Popular,
    Sort::Rating,
];
const WARM_KINDS: [EntityKind; 4] = [
    EntityKind::Book,
    EntityKind::Movie,
    EntityKind::Series,
    EntityKind::Game,
];

/// Ошибка индекса. `Busy` — писатель занят (идёт перестройка или другая запись).
#[derive(Debug)]
pub enum IndexError {
    Busy,
    Index(TantivyError),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "catalog index is locked by another writer"),
            Self::Index(error) => write!(f, "catalog index: {error}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<TantivyError> for IndexError {
    fn from(error: TantivyError) -> Self {
        match error {
            TantivyError::LockFailure(LockError::LockBusy, _) => Self::Busy,
            error => Self::Index(error),
        }
    }
}

/// Часть размера сегмента по полям и куда её прибавить в [`FieldBytes`].
type FieldPart<'a> = (
    &'a tantivy::space_usage::PerFieldSpaceUsage,
    fn(&mut FieldBytes) -> &mut u64,
);

/// Место одного поля индекса на диске (сумма по сегментам), байты.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldBytes {
    pub termdict: u64,
    pub postings: u64,
    /// Позиции слов (для фраз).
    pub positions: u64,
    /// FAST-колонки.
    pub fast: u64,
    /// Нормы длины поля (для оценки совпадения).
    pub fieldnorms: u64,
}

impl FieldBytes {
    pub fn total(&self) -> u64 {
        self.termdict + self.postings + self.positions + self.fast + self.fieldnorms
    }
}

/// Состояние слоя А ([`CatalogIndex::stats`]).
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub schema_version: &'static str,
    pub docs: u64,
    /// Документов по видам (`book`, `movie`, …, `person`).
    pub kinds: Vec<(String, u64)>,
    /// Удалённые, но ещё лежащие в сегментах документы (уйдут при слиянии).
    pub deleted_docs: u64,
    pub segments: usize,
    pub index_bytes: u64,
    /// Хранилище документов (поле `card`, сжато целиком — по полям не делится).
    pub store_bytes: u64,
    pub deletes_bytes: u64,
    /// Все файлы каталога (с неубранными старыми сегментами и снимками слоя Б).
    pub disk_bytes: u64,
    pub sidecar_file_bytes: u64,
    pub fields: Vec<(String, FieldBytes)>,
    pub cache_entries: usize,
    pub cache_ranked: usize,
}

/// Какую часть индекса перестраивает [`CatalogIndex::rebuild`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Весь индекс.
    All,
    /// Только документы этих видов (`kind`).
    Kinds(Vec<&'static str>),
}

/// Ряд выдачи одного запроса в порядке сортировки и сколько всего найдено. Страницы одного
/// списка — срезы этого ряда: полная сортировка раздела делается один раз на список.
struct Ranked {
    ords: Vec<u32>,
    total: usize,
    approximate: bool,
}

impl Ranked {
    fn empty() -> Self {
        Self {
            ords: Vec::new(),
            total: 0,
            approximate: false,
        }
    }

    /// В ряду есть вся страница (или выдача кончилась раньше).
    fn covers(&self, limit: usize, offset: usize) -> bool {
        self.ords.len() >= (offset + limit).min(self.total)
    }
}

type PeopleKey = (String, Option<String>, PeopleSort, usize, usize);

/// Ответы поиска; живёт, пока не изменились индекс и слой Б.
#[derive(Default)]
struct Cache {
    generation: u64,
    entries: HashMap<(Query, usize, usize), Found>,
    order: VecDeque<(Query, usize, usize)>,
    /// Ряды выдачи по запросу без `limit` и `offset`.
    ranked: HashMap<Query, Arc<Ranked>>,
    ranked_order: VecDeque<Query>,
    people: HashMap<PeopleKey, (Vec<String>, usize)>,
    /// Счётчики ролей людей по тексту запроса (фасет «известен по»).
    role_counts: HashMap<String, BTreeMap<String, i64>>,
}

impl Cache {
    fn at(&mut self, generation: u64) -> &mut Self {
        if self.generation != generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
        self
    }
}

/// Номера, которые есть в Tantivy: слой Б может знать сущность, а текст ещё не дописан —
/// пересечение с фильтром не даёт пустую строку выдачи. Строится на каждое состояние индекса.
struct Present {
    fingerprint: u64,
    entities: RoaringBitmap,
    people: RoaringBitmap,
}

impl Present {
    fn build(searcher: &Searcher, fingerprint: u64) -> Result<Self, IndexError> {
        let mut entities = RoaringBitmap::new();
        let mut people = RoaringBitmap::new();
        for segment in searcher.segment_readers() {
            let ords = segment
                .fast_fields()
                .u64("ord")?
                .first_or_default_col(u64::MAX);
            for doc in segment.doc_ids_alive() {
                let raw = ords.get_val(doc);
                if let Some(ord) = Space::Entity.local(raw) {
                    entities.insert(ord);
                } else if let Some(ord) = Space::Person.local(raw) {
                    people.insert(ord);
                }
            }
        }
        Ok(Self {
            fingerprint,
            entities,
            people,
        })
    }
}

/// Отпечаток содержимого индекса: сегменты и их удаления. Номер поколения читателя для этого не
/// годится — он растёт на каждой перезагрузке читателя, даже когда индекс не менялся.
fn fingerprint(searcher: &Searcher) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (segment, deletes) in searcher.generation().segments() {
        segment.uuid_string().hash(&mut hasher);
        deletes.hash(&mut hasher);
    }
    hasher.finish()
}

/// Шаг ответа на запрос ([`CatalogIndex::explain`]).
#[derive(Debug, Clone, Default)]
pub struct ExplainStep {
    pub name: String,
    /// Сколько сущностей осталось после шага.
    pub count: u64,
    pub micros: u64,
}

/// Место в выдаче с разбором (по релевантности) или без (готовый порядок).
#[derive(Debug, Clone, Default)]
pub struct ExplainItem {
    pub ord: u32,
    pub title: String,
    pub kind: String,
    pub slug: String,
    pub position: u32,
    pub detail: Option<sidecar::RankDetail>,
}

/// Ответ на запрос по шагам.
#[derive(Debug, Clone, Default)]
pub struct Explain {
    pub steps: Vec<ExplainStep>,
    /// Термы фильтров слоя А (`studio:…`, `person:…`).
    pub facet_terms: Vec<String>,
    pub approximate: bool,
    /// Порядок — релевантность (у мест есть разбор).
    pub relevance: bool,
    pub total: u64,
    pub items: Vec<ExplainItem>,
}

/// Открытый единый индекс: один на каталог в процессе.
pub struct CatalogIndex {
    dir: PathBuf,
    index: Index,
    reader: IndexReader,
    f: schema::Fields,
    cache: Mutex<Cache>,
    /// Кэш ответов включён. Выключается только для замеров (`nexus search bench`).
    cached: AtomicBool,
    entities: ArcSwap<EntitySidecar>,
    people: ArcSwap<PeopleSidecar>,
    present: Mutex<Option<Arc<Present>>>,
}

/// Открытые индексы по каталогу (в тестах у каждого свой).
static OPEN: Mutex<Option<HashMap<PathBuf, Arc<CatalogIndex>>>> = Mutex::new(None);

impl CatalogIndex {
    /// Индекс в каталоге `dir`; нет или собран по другой схеме — создаётся пустой.
    pub fn open(dir: &Path) -> Result<Arc<Self>, IndexError> {
        let mut open = OPEN.lock().unwrap();
        let open = open.get_or_insert_with(HashMap::new);
        if let Some(index) = open.get(dir) {
            return Ok(index.clone());
        }
        std::fs::create_dir_all(dir).map_err(TantivyError::from)?;
        let (schema, fields) = schema::build();
        let version = dir.join(SCHEMA_FILE);
        let current = std::fs::read_to_string(&version).is_ok_and(|v| v.trim() == SCHEMA_VERSION);
        if !current {
            // Старую схему читать нельзя: файлы индекса удаляются, индекс строится заново из БД.
            for entry in std::fs::read_dir(dir).map_err(TantivyError::from)? {
                let path = entry.map_err(TantivyError::from)?.path();
                if path.is_file() {
                    std::fs::remove_file(&path).map_err(TantivyError::from)?;
                }
            }
        }
        let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
        let index = if Index::exists(&directory).map_err(TantivyError::from)? {
            Index::open(directory)?
        } else {
            Index::create(directory, schema, schema::settings())?
        };
        if !current {
            std::fs::write(&version, SCHEMA_VERSION).map_err(TantivyError::from)?;
        }
        shared::tantivy::register_tokenizers(&index);
        // Коммиты другого процесса (`nexus search rebuild`) читатель подхватывает сам.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .doc_store_cache_num_blocks(DOC_STORE_CACHE_BLOCKS)
            .try_into()?;
        let index = Arc::new(Self {
            dir: dir.to_path_buf(),
            index,
            reader,
            f: fields,
            cache: Mutex::new(Cache::default()),
            cached: AtomicBool::new(true),
            // Снимки слоя Б с диска: поиск работает сразу, до первой сборки из БД.
            entities: ArcSwap::from_pointee(EntitySidecar::load(dir).unwrap_or_default()),
            people: ArcSwap::from_pointee(PeopleSidecar::load(dir).unwrap_or_default()),
            present: Mutex::new(None),
        });
        open.insert(dir.to_path_buf(), index.clone());
        Ok(index)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entities(&self) -> Arc<EntitySidecar> {
        self.entities.load_full()
    }

    pub fn people(&self) -> Arc<PeopleSidecar> {
        self.people.load_full()
    }

    /// Подменить слой Б сущностей целиком: читатели видят старый или новый снимок целиком, кэш
    /// выдачи сбрасывается сам (другое поколение). Снимок пишется на диск.
    pub fn set_entities(&self, sidecar: EntitySidecar) -> Result<(), IndexError> {
        // Записать файлом и подхватить отображённую версию: собранная копия в куче сразу
        // освобождается, в памяти остаются только фасеты.
        let sidecar = match sidecar.save(&self.dir) {
            Ok(()) => EntitySidecar::load(&self.dir).unwrap_or(sidecar),
            Err(error) => {
                tracing::warn!(%error, "catalog sidecar snapshot was not saved");
                sidecar
            }
        };
        self.entities.store(Arc::new(sidecar));
        self.warm()
    }

    /// Подменить слой Б людей.
    pub fn set_people(&self, sidecar: PeopleSidecar) -> Result<(), IndexError> {
        let sidecar = match sidecar.save(&self.dir) {
            Ok(()) => PeopleSidecar::load(&self.dir).unwrap_or(sidecar),
            Err(error) => {
                tracing::warn!(%error, "people sidecar snapshot was not saved");
                sidecar
            }
        };
        self.people.store(Arc::new(sidecar));
        Ok(())
    }

    /// Только ручные места (`ord → место`): фасеты и прочие порядки остаются как есть.
    /// Новые баллы «Рекомендуемые» в слое Б ([`EntitySidecar::with_scores`]).
    pub fn set_scores(&self, scores: &HashMap<u32, f64>) -> Result<(), IndexError> {
        let next = self.entities.load().with_scores(scores);
        self.set_entities(next)
    }

    pub fn set_ranks(&self, ranks: &HashMap<u32, u16>) -> Result<(), IndexError> {
        let next = self.entities.load().with_ranks(ranks);
        self.set_entities(next)
    }

    /// Включить или выключить кэш ответов (для замеров: сколько стоит сам запрос).
    pub fn set_cached(&self, cached: bool) {
        self.cached.store(cached, Ordering::Relaxed);
    }

    /// Поколение выдачи: меняется и с индексом, и со слоем Б.
    fn generation(&self, present: &Present) -> u64 {
        present.fingerprint
            ^ self.entities.load().generation().rotate_left(32)
            ^ self.people.load().generation().rotate_left(16)
    }

    /// Номера, которые читает `searcher`.
    fn present(&self, searcher: &Searcher) -> Result<Arc<Present>, IndexError> {
        let fingerprint = fingerprint(searcher);
        let mut current = self.present.lock().unwrap();
        if let Some(present) = current
            .as_ref()
            .filter(|present| present.fingerprint == fingerprint)
        {
            return Ok(present.clone());
        }
        let present = Arc::new(Present::build(searcher, fingerprint)?);
        *current = Some(present.clone());
        Ok(present)
    }

    /// Состояние индекса для админки: документы, сегменты и место на диске по полям.
    pub fn stats(&self) -> Result<IndexStats, IndexError> {
        let searcher = self.reader.searcher();
        let usage = searcher.space_usage().map_err(TantivyError::from)?;
        let mut fields: BTreeMap<String, FieldBytes> = BTreeMap::new();
        let mut store = 0u64;
        let mut deletes = 0u64;
        for segment in usage.segments() {
            let parts: [FieldPart; 5] = [
                (segment.termdict(), |f| &mut f.termdict),
                (segment.postings(), |f| &mut f.postings),
                (segment.positions(), |f| &mut f.positions),
                (segment.fast_fields(), |f| &mut f.fast),
                (segment.fieldnorms(), |f| &mut f.fieldnorms),
            ];
            for (per_field, slot) in parts {
                for field in per_field.fields() {
                    let entry = fields.entry(field.field_name().to_string()).or_default();
                    *slot(entry) += field.total().get_bytes();
                }
            }
            store += segment.store().total().get_bytes();
            deletes += segment.deletes().get_bytes();
        }
        let deleted_docs = searcher
            .segment_readers()
            .iter()
            .map(|r| u64::from(r.num_deleted_docs()))
            .sum();
        let file = |name: &str| std::fs::metadata(self.dir.join(name)).map_or(0, |m| m.len());
        let disk_bytes = std::fs::read_dir(&self.dir)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter_map(|e| e.metadata().ok())
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0);
        let mut kinds = Vec::new();
        for kind in EntityKind::ALL
            .iter()
            .map(|k| k.as_str())
            .chain([PERSON_KIND])
        {
            let n = searcher.doc_freq(&Term::from_field_text(self.f.kind, kind))?;
            if n > 0 {
                kinds.push((kind.to_string(), n));
            }
        }
        let cache = self.cache.lock().unwrap();
        Ok(IndexStats {
            schema_version: SCHEMA_VERSION,
            docs: searcher.num_docs(),
            kinds,
            deleted_docs,
            segments: searcher.segment_readers().len(),
            index_bytes: usage.total().get_bytes(),
            store_bytes: store,
            deletes_bytes: deletes,
            disk_bytes,
            sidecar_file_bytes: file(sidecar::SNAPSHOT_FILE) + file(people_sidecar::SNAPSHOT_FILE),
            fields: fields.into_iter().collect(),
            cache_entries: cache.entries.len(),
            cache_ranked: cache.ranked.len(),
        })
    }

    /// Слить сегменты в один и удалить файлы, на которые индекс больше не ссылается: место
    /// удалённых документов освобождается, поиск по одному сегменту быстрее. Занят — `Busy`.
    pub fn merge(&self) -> Result<usize, IndexError> {
        let mut writer = self.writer()?;
        let ids = self.index.searchable_segment_ids()?;
        if ids.len() > 1 {
            writer.merge(&ids).wait()?;
        }
        writer.garbage_collect_files().wait()?;
        drop(writer);
        self.reader.reload()?;
        self.warm()?;
        Ok(ids.len())
    }

    /// Сбросить кэш выдачи и заново прогреть списки раздела.
    pub fn clear_cache(&self) -> Result<(), IndexError> {
        *self.cache.lock().unwrap() = Cache::default();
        self.warm()
    }

    /// Сколько документов в индексе (все виды).
    pub fn len(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Сколько документов вида `kind` (`person` — люди).
    pub fn kind_docs(&self, kind: &str) -> u64 {
        self.reader
            .searcher()
            .doc_freq(&Term::from_field_text(self.f.kind, kind))
            .unwrap_or(0)
    }

    fn writer(&self) -> Result<IndexWriter<TantivyDocument>, IndexError> {
        Ok(self.index.writer_with_num_threads(1, WRITER_BUDGET)?)
    }

    fn document(&self, doc: &TextDoc) -> TantivyDocument {
        let f = &self.f;
        let mut out = TantivyDocument::default();
        out.add_text(f.kind, doc.kind);
        out.add_u64(f.ord, doc.ord);
        for title in &doc.titles {
            out.add_text(f.title, title);
            let key = normalize_key(title);
            if !key.is_empty() {
                out.add_bytes(f.title_key, &schema::title_key_term(&key));
            }
        }
        for name in &doc.people {
            out.add_text(f.people, name);
        }
        for text in &doc.extra {
            out.add_text(f.extra, text);
        }
        for term in &doc.facets {
            out.add_text(f.facet, term);
        }
        out.add_text(f.card, &doc.card);
        out
    }

    /// Обновить документы: `remove` (значения `ord`) удаляются, `docs` добавляются вместо
    /// прежних с тем же `ord`. Один коммит.
    pub fn update(&self, remove: &[u64], docs: &[TextDoc]) -> Result<(), IndexError> {
        if remove.is_empty() && docs.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer()?;
        for ord in remove.iter().chain(docs.iter().map(|doc| &doc.ord)) {
            writer.delete_term(Term::from_field_u64(self.f.ord, *ord));
        }
        for doc in docs {
            writer.add_document(self.document(doc))?;
        }
        writer.commit()?;
        drop(writer);
        self.reader.reload()?;
        self.warm()
    }

    /// Есть ли в индексе хоть один из этих документов (значения `ord`).
    pub fn has_any(&self, ords: &[u64]) -> Result<bool, IndexError> {
        let searcher = self.reader.searcher();
        for ord in ords {
            if searcher.doc_freq(&Term::from_field_u64(self.f.ord, *ord))? > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Начать перестройку: прежние документы области удаляются тем же коммитом, что добавит
    /// новые, — до него поиск читает старый индекс целиком.
    pub fn rebuild(self: &Arc<Self>, scope: Scope) -> Result<Rebuild, IndexError> {
        let writer = self.writer()?;
        let before = match &scope {
            Scope::All => {
                writer.delete_all_documents()?;
                self.len()
            }
            Scope::Kinds(kinds) => {
                let mut before = 0;
                for kind in kinds {
                    writer.delete_term(Term::from_field_text(self.f.kind, kind));
                    before += self.kind_docs(kind);
                }
                before
            }
        };
        Ok(Rebuild {
            index: self.clone(),
            writer,
            before,
            added: 0,
        })
    }

    // ------------------------------------------------------------ сущности

    fn builder(&self) -> QueryBuilder<'_> {
        QueryBuilder {
            index: &self.index,
            f: &self.f,
        }
    }

    /// Поиск: страница `limit` сущностей с `offset` и сколько всего найдено.
    pub fn search(&self, q: &Query, limit: usize, offset: usize) -> Result<Found, IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.entities.load_full();
        let generation = self.generation(&present);
        let offset = offset.min(MAX_OFFSET);
        let key = (q.clone(), limit, offset);
        let cached = self.cached.load(Ordering::Relaxed);
        if cached {
            if let Some(found) = self.cache.lock().unwrap().at(generation).entries.get(&key) {
                return Ok(found.clone());
            }
        }
        let ranked = if cached {
            self.ranked(&searcher, &present, &sidecar, generation, q, limit, offset)?
        } else {
            Arc::new(self.find(&searcher, &present, &sidecar, q, offset + limit)?)
        };
        let today = chrono::Utc::now().date_naive();
        let mut items = Vec::with_capacity(limit.min(ranked.total));
        for &ord in ranked.ords.iter().skip(offset).take(limit) {
            if let Some(card) = self.card(&searcher, Space::Entity.raw(ord))? {
                if let Ok(card) = serde_json::from_str::<Card>(&card) {
                    items.push(card.summary(today));
                }
            }
        }
        let found = Found {
            items,
            total: ranked.total,
            approximate: ranked.approximate,
        };
        if cached && found.items.len() <= CACHE_MAX_ITEMS {
            let mut cache = self.cache.lock().unwrap();
            let cache = cache.at(generation);
            if cache.entries.insert(key.clone(), found.clone()).is_none() {
                cache.order.push_back(key);
                if cache.order.len() > CACHE_ENTRIES {
                    if let Some(oldest) = cache.order.pop_front() {
                        cache.entries.remove(&oldest);
                    }
                }
            }
        }
        Ok(found)
    }

    /// Ряд выдачи запроса, в котором есть страница `offset..offset + limit`: из памяти или
    /// собранный заново (с запасом, чтобы следующие страницы были срезами).
    #[allow(clippy::too_many_arguments)]
    fn ranked(
        &self,
        searcher: &Searcher,
        present: &Present,
        sidecar: &EntitySidecar,
        generation: u64,
        q: &Query,
        limit: usize,
        offset: usize,
    ) -> Result<Arc<Ranked>, IndexError> {
        if let Some(ranked) = self.cache.lock().unwrap().at(generation).ranked.get(q) {
            if ranked.covers(limit, offset) {
                return Ok(ranked.clone());
            }
        }
        let len = (offset + limit).next_power_of_two().max(RANKED_LEN);
        let ranked = Arc::new(self.find(searcher, present, sidecar, q, len)?);
        if ranked.ords.len() > RANKED_MAX_LEN {
            return Ok(ranked);
        }
        let mut cache = self.cache.lock().unwrap();
        let cache = cache.at(generation);
        if cache.ranked.insert(q.clone(), ranked.clone()).is_none() {
            cache.ranked_order.push_back(q.clone());
            if cache.ranked_order.len() > RANKED_ENTRIES {
                if let Some(oldest) = cache.ranked_order.pop_front() {
                    cache.ranked.remove(&oldest);
                }
            }
        }
        Ok(ranked)
    }

    /// Подготовить списки раздела без текста и фильтров ([`WARM_SORTS`] по видам). Вызывается
    /// после открытия индекса и после каждой записи в него или в слой Б.
    pub fn warm(&self) -> Result<(), IndexError> {
        if self.entities.load().is_empty() {
            return Ok(());
        }
        for kind in WARM_KINDS {
            for sort in WARM_SORTS {
                let query = Query {
                    sort,
                    kinds: vec![kind],
                    ..Query::default()
                };
                self.search(&query, 24, 0)?;
            }
        }
        // Галерея людей: первая страница каждой роли. Первый запрос строит позиции по 1,2 млн
        // людей (`Order::pos`) и поднимает с диска карточки — без прогрева это ~1,3 с у первого
        // посетителя, потом 5–10 мс.
        let people = self.people.load_full();
        self.search_people("", None, PeopleSort::Fame, 24, 0)?;
        for role in people.roles() {
            self.search_people("", Some(&role), PeopleSort::Fame, 24, 0)?;
        }
        Ok(())
    }

    /// Первые `len` номеров выдачи, сколько всего найдено и признак «приблизительно» (нашлось
    /// только с исправлением опечатки).
    ///
    /// - Без текста Tantivy не вызывается: фасеты слоя Б пересекаются, порядок — готовый.
    /// - С текстом Tantivy отдаёт совпадения из фильтра ([`OrdHits`]); релевантность с ручными
    ///   местами строит [`EntitySidecar::rank_hits`], остальные сортировки — готовые порядки.
    fn find(
        &self,
        searcher: &Searcher,
        present: &Present,
        sidecar: &EntitySidecar,
        q: &Query,
        len: usize,
    ) -> Result<Ranked, IndexError> {
        let query: String = q.q.chars().take(MAX_QUERY_CHARS).collect();
        // Одна-две буквы — ещё не запрос, но только у книг (как в настольной библиотеке): у
        // остальных видов поиск начинается с первой буквы. Пустой запрос — список по фильтрам.
        let typed = query.trim().chars().count();
        if q.kinds == [EntityKind::Book] && (1..MIN_QUERY_CHARS).contains(&typed) {
            return Ok(Ranked::empty());
        }
        let mut filter = sidecar.filter(q);
        filter &= &present.entities;
        // Студия, издатель, музей, награда, человек — из инвертированного индекса (поле
        // `facet`), не из RAM: короткие списки номеров, пересечение внутри Tantivy.
        let Some(terms) = sidecar::text_facet_terms(q) else {
            return Ok(Ranked::empty());
        };
        if !terms.is_empty() && !filter.is_empty() {
            let clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = terms
                .iter()
                .map(|t| {
                    let term = Term::from_field_text(self.f.facet, t);
                    let query: Box<dyn TantivyQuery> =
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                    (Occur::Must, query)
                })
                .collect();
            let works = searcher.search(
                &BooleanQuery::new(clauses),
                &OrdHits {
                    filter: Arc::new(filter),
                    space: Space::Entity,
                },
            )?;
            filter = works.into_iter().map(|(ord, _)| ord).collect();
        }
        let today = chrono::Utc::now().date_naive();
        let Some((strict, fallback, title)) = self.builder().build(searcher, &query)? else {
            // Слов в запросе нет (пусто или одни знаки): все сущности под фильтры.
            let (ords, total) = sidecar.page(q.sort, &filter, today, len);
            return Ok(Ranked {
                ords,
                total,
                approximate: false,
            });
        };
        let filter = Arc::new(filter);
        let collect = |query: &dyn TantivyQuery| {
            searcher.search(
                query,
                &OrdHits {
                    filter: filter.clone(),
                    space: Space::Entity,
                },
            )
        };
        let mut hits = collect(strict.as_ref())?;
        let mut approximate = false;
        if hits.is_empty() {
            if let Some(fallback) = fallback {
                hits = collect(fallback.as_ref())?;
                approximate = true;
            }
        }
        if hits.is_empty() {
            return Ok(Ranked::empty());
        }
        let total = hits.len();
        if q.sort == Sort::Relevance {
            let (exact, titled) = self.title_sets(searcher, &query, title.as_ref(), &hits)?;
            return Ok(Ranked {
                ords: sidecar.rank_hits(&hits, &exact, &titled, len),
                total,
                approximate,
            });
        }
        let matches: RoaringBitmap = hits.into_iter().map(|(ord, _)| ord).collect();
        let (ords, total) = sidecar.page(q.sort, &matches, today, len);
        Ok(Ranked {
            ords,
            total,
            approximate,
        })
    }

    /// Среди найденных `hits`: точное название запроса (выше любого ручного места) и совпавшие
    /// в названии (выше, чем только по людям или серии). Без оценок BM25 и только по
    /// найденным: это проверки «да/нет».
    fn title_sets(
        &self,
        searcher: &Searcher,
        query: &str,
        title: &dyn TantivyQuery,
        hits: &[(u32, f32)],
    ) -> Result<(RoaringBitmap, RoaringBitmap), IndexError> {
        let found: Arc<RoaringBitmap> = Arc::new(hits.iter().map(|&(ord, _)| ord).collect());
        let collect = |query: &dyn TantivyQuery| {
            searcher.search(
                query,
                &OrdSet {
                    filter: found.clone(),
                    space: Space::Entity,
                },
            )
        };
        let key = normalize_key(query);
        let exact = if key.is_empty() {
            RoaringBitmap::new()
        } else {
            collect(self.builder().title_key(&key).as_ref())?
        };
        let titled = collect(title)?;
        Ok((exact, titled))
    }

    /// Как поиск отвечает на запрос — по шагам, теми же функциями, что [`Self::find`]
    /// (без кэша): сколько осталось после каждого шага, сколько он занял, и первые `len` с
    /// разбором места.
    pub fn explain(&self, q: &Query, len: usize) -> Result<Explain, IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.entities.load_full();
        let mut out = Explain::default();
        let mut clock = std::time::Instant::now();
        let mut step = |out: &mut Explain, name: &str, count: u64| {
            out.steps.push(ExplainStep {
                name: name.to_string(),
                count,
                micros: clock.elapsed().as_micros() as u64,
            });
            clock = std::time::Instant::now();
        };
        let mut filter = sidecar.filter(q);
        filter &= &present.entities;
        step(&mut out, "Фильтры слоя Б (битовые карты в RAM)", filter.len());
        let Some(terms) = sidecar::text_facet_terms(q) else {
            step(&mut out, "Фильтр с пустым названием — ничего", 0);
            return Ok(out);
        };
        if !terms.is_empty() {
            let clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = terms
                .iter()
                .map(|t| {
                    let term = Term::from_field_text(self.f.facet, t);
                    let query: Box<dyn TantivyQuery> =
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                    (Occur::Must, query)
                })
                .collect();
            let works = searcher.search(
                &BooleanQuery::new(clauses),
                &OrdHits {
                    filter: Arc::new(filter),
                    space: Space::Entity,
                },
            )?;
            filter = works.into_iter().map(|(ord, _)| ord).collect();
            out.facet_terms = terms;
            step(&mut out, "Фильтры слоя А (термы поля facet в Tantivy)", filter.len());
        }
        let query: String = q.q.chars().take(MAX_QUERY_CHARS).collect();
        let today = chrono::Utc::now().date_naive();
        let card = |ord: u32| -> (String, String, String) {
            self.card(&searcher, Space::Entity.raw(ord))
                .ok()
                .flatten()
                .and_then(|c| serde_json::from_str::<Card>(&c).ok())
                .map(|c| (c.title, c.kind.as_str().to_string(), c.slug))
                .unwrap_or_default()
        };
        let built = self.builder().build(&searcher, &query)?;
        if built.is_some() {
            step(&mut out, "Разбор запроса (стемы, автодополнение по словарю)", 0);
        }
        let Some((strict, fallback, title)) = built else {
            let (ords, total) = sidecar.page(q.sort, &filter, today, len);
            out.total = total as u64;
            step(&mut out, "Готовый порядок слоя Б (без текста)", total as u64);
            for (at, ord) in ords.into_iter().enumerate() {
                let (title, kind, slug) = card(ord);
                out.items.push(ExplainItem {
                    ord,
                    title,
                    kind,
                    slug,
                    position: at as u32,
                    detail: None,
                });
            }
            return Ok(out);
        };
        let filter = Arc::new(filter);
        let collect = |query: &dyn TantivyQuery| {
            searcher.search(
                query,
                &OrdHits {
                    filter: filter.clone(),
                    space: Space::Entity,
                },
            )
        };
        let mut hits = collect(strict.as_ref())?;
        step(&mut out, "Текст: все слова (название, люди, серия)", hits.len() as u64);
        if hits.is_empty() {
            if let Some(fallback) = fallback {
                hits = collect(fallback.as_ref())?;
                out.approximate = true;
                step(&mut out, "Текст: с опечаткой (строгий запрос пуст)", hits.len() as u64);
            }
        }
        out.total = hits.len() as u64;
        if q.sort != Sort::Relevance {
            let matches: RoaringBitmap = hits.into_iter().map(|(ord, _)| ord).collect();
            let (ords, _) = sidecar.page(q.sort, &matches, today, len);
            step(&mut out, "Совпавшие — по готовому порядку слоя Б", matches.len());
            for (at, ord) in ords.into_iter().enumerate() {
                let (title, kind, slug) = card(ord);
                out.items.push(ExplainItem {
                    ord,
                    title,
                    kind,
                    slug,
                    position: at as u32,
                    detail: None,
                });
            }
            return Ok(out);
        }
        out.relevance = true;
        let (exact, titled) = self.title_sets(&searcher, &query, title.as_ref(), &hits)?;
        step(
            &mut out,
            &format!("Точное название (title_key): {}; совпало в названии (title)", exact.len()),
            titled.len(),
        );
        let ranked = sidecar.ranked(&hits, &exact, &titled, len);
        step(&mut out, "Ранжирование: уровень, затем текст + популярность", hits.len() as u64);
        for (at, detail) in ranked.into_iter().enumerate() {
            let (title, kind, slug) = card(detail.ord);
            out.items.push(ExplainItem {
                ord: detail.ord,
                title,
                kind,
                slug,
                position: at as u32,
                detail: Some(detail),
            });
        }
        Ok(out)
    }

    /// Первые `n` каждого готового порядка слоя Б: (имя, длина, номера с названиями).
    pub fn order_heads(&self, n: usize) -> Vec<(&'static str, u64, Vec<(u32, String)>)> {
        let searcher = self.reader.searcher();
        let title = |ord: u32| -> String {
            self.card(&searcher, Space::Entity.raw(ord))
                .ok()
                .flatten()
                .and_then(|c| serde_json::from_str::<Card>(&c).ok())
                .map(|c| c.title)
                .unwrap_or_default()
        };
        self.entities
            .load()
            .order_heads(n)
            .into_iter()
            .map(|(name, len, ords)| (name, len, ords.into_iter().map(|o| (o, title(o))).collect()))
            .collect()
    }

    /// Термы поля `facet` по группам (`person`, `role`, `studio`…): сколько термов (сумма по
    /// сегментам) и самые частые значения (по числу документов в каждом сегменте, сложенному
    /// по совпавшим; приблизительно).
    pub fn facet_term_counts(
        &self,
        top: usize,
    ) -> Result<Vec<(String, u64, Vec<(String, u64)>)>, IndexError> {
        let searcher = self.reader.searcher();
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut best: BTreeMap<String, HashMap<String, u64>> = BTreeMap::new();
        for segment in searcher.segment_readers() {
            let inverted = segment.inverted_index(self.f.facet)?;
            let mut stream = inverted.terms().stream().map_err(TantivyError::from)?;
            // Лучшие `top` группы в сегменте: (документов, терм).
            let mut local: HashMap<String, Vec<(u64, String)>> = HashMap::new();
            while stream.advance() {
                let key = stream.key();
                let end = key.iter().position(|&b| b == b':').unwrap_or(key.len());
                let group = String::from_utf8_lossy(&key[..end]).into_owned();
                *counts.entry(group.clone()).or_default() += 1;
                let df = u64::from(stream.value().doc_freq);
                let list = local.entry(group).or_default();
                if list.len() < top || list.last().is_some_and(|l| l.0 < df) {
                    let value = String::from_utf8_lossy(key.get(end + 1..).unwrap_or_default())
                        .into_owned();
                    let at = list.partition_point(|x| x.0 >= df);
                    list.insert(at, (df, value));
                    list.truncate(top);
                }
            }
            for (group, list) in local {
                let merged = best.entry(group).or_default();
                for (df, value) in list {
                    *merged.entry(value).or_default() += df;
                }
            }
        }
        Ok(counts
            .into_iter()
            .map(|(group, n)| {
                let mut list: Vec<(String, u64)> = best.remove(&group).unwrap_or_default().into_iter().collect();
                list.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                list.truncate(top);
                (group, n, list)
            })
            .collect())
    }

    /// Сохранённая в Tantivy карточка (поле `card`) сущности или человека — для админки.
    pub fn stored_card(&self, person: bool, ord: u32) -> Result<Option<String>, IndexError> {
        let space = if person { Space::Person } else { Space::Entity };
        self.card(&self.reader.searcher(), space.raw(ord))
    }

    /// Записей в кэше ответов: (страницы, ряды, люди).
    pub fn cache_sizes(&self) -> (usize, usize, usize) {
        let cache = self.cache.lock().unwrap();
        (cache.entries.len(), cache.ranked.len(), cache.people.len())
    }

    /// Карточка документа в JSON по значению колонки `ord`.
    fn card(&self, searcher: &Searcher, raw: u64) -> Result<Option<String>, IndexError> {
        let query = TermQuery::new(
            Term::from_field_u64(self.f.ord, raw),
            IndexRecordOption::Basic,
        );
        let hits = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, address)) = hits.into_iter().next() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(address)?;
        Ok(doc
            .get_first(self.f.card)
            .and_then(|value| value.as_str())
            .map(str::to_owned))
    }

    /// Сколько сущностей под запрос (без самой выдачи).
    pub fn count(&self, q: &Query) -> Result<usize, IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.entities.load_full();
        // Через кэш рядов: общая выдача (`blended`) спрашивает число книг на каждый запрос, а
        // страница того же запроса потом берётся из того же ряда.
        if self.cached.load(Ordering::Relaxed) {
            let generation = self.generation(&present);
            return Ok(self
                .ranked(&searcher, &present, &sidecar, generation, q, 1, 0)?
                .total);
        }
        Ok(self.find(&searcher, &present, &sidecar, q, 0)?.total)
    }

    /// Первые `cap` номеров выдачи по порядку — ряд для «предыдущая / следующая».
    pub fn ords(&self, q: &Query, cap: usize) -> Result<Vec<u32>, IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.entities.load_full();
        Ok(self.find(&searcher, &present, &sidecar, q, cap)?.ords)
    }

    /// Карточки сущностей по номерам (в порядке `ords`; неизвестные пропускаются).
    pub fn by_ords(&self, ords: &[u32]) -> Result<Vec<EntitySummary>, IndexError> {
        let searcher = self.reader.searcher();
        let today = chrono::Utc::now().date_naive();
        let mut out = Vec::with_capacity(ords.len());
        for &ord in ords {
            if let Some(card) = self.card(&searcher, Space::Entity.raw(ord))? {
                if let Ok(card) = serde_json::from_str::<Card>(&card) {
                    out.push(card.summary(today));
                }
            }
        }
        Ok(out)
    }

    /// Счётчики значений фасета (`tag:`, `lang:`, `person:`…) среди сущностей под запрос: для
    /// панели фильтров.
    pub fn facet_counts(&self, q: &Query, prefix: &str) -> Result<Vec<(String, u64)>, IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.entities.load_full();
        let ranked = self.find(&searcher, &present, &sidecar, q, usize::MAX)?;
        let found: RoaringBitmap = ranked.ords.into_iter().collect();
        Ok(sidecar.facet_counts(&found, prefix))
    }

    /// Подсказки значений группы (`studio`, `publisher`, `museum`, `country`, `tag`).
    pub fn hints(
        &self,
        group: &str,
        q: &str,
        kind: Option<EntityKind>,
        limit: usize,
    ) -> Vec<(String, u64)> {
        self.entities.load().hints(group, q, kind, limit)
    }

    // -------------------------------------------------------------- люди

    /// Люди под текст и роль: множество и (с текстом) совпадения с оценками. Строгий запрос
    /// ничего не нашёл — повтор с опечатками.
    fn people_matches(
        &self,
        searcher: &Searcher,
        present: &Present,
        sidecar: &PeopleSidecar,
        q: &str,
        role: Option<&str>,
    ) -> Result<(RoaringBitmap, Option<Vec<(u32, f32)>>), IndexError> {
        let query: String = q.chars().take(MAX_QUERY_CHARS).collect();
        let mut filter = sidecar.filter(role);
        filter &= &present.people;
        let Some((strict, fallback, _title)) = self.builder().build(searcher, &query)? else {
            return Ok((filter, None));
        };
        let filter = Arc::new(filter);
        let collect = |query: &dyn TantivyQuery| {
            searcher.search(
                query,
                &OrdHits {
                    filter: filter.clone(),
                    space: Space::Person,
                },
            )
        };
        let mut hits = collect(strict.as_ref())?;
        if hits.is_empty() {
            if let Some(fallback) = fallback {
                hits = collect(fallback.as_ref())?;
            }
        }
        let found = hits.iter().map(|(ord, _)| *ord).collect();
        Ok((found, Some(hits)))
    }

    /// Страница карточек людей (JSON) и общее число найденных. Порядок: с текстом — релевантность
    /// (при равной — известность), иначе известность или имя.
    pub fn search_people(
        &self,
        q: &str,
        role: Option<&str>,
        sort: PeopleSort,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<serde_json::Value>, usize), IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.people.load_full();
        let generation = self.generation(&present);
        let key: PeopleKey = (q.to_owned(), role.map(str::to_owned), sort, limit, offset);
        let cached = self.cached.load(Ordering::Relaxed);
        let to_values = |cards: &[String]| -> Vec<serde_json::Value> {
            cards
                .iter()
                .filter_map(|c| serde_json::from_str(c).ok())
                .collect()
        };
        if cached {
            if let Some((cards, total)) = self.cache.lock().unwrap().at(generation).people.get(&key)
            {
                return Ok((to_values(cards), *total));
            }
        }
        let (found, hits) = self.people_matches(&searcher, &present, &sidecar, q, role)?;
        let total = found.len() as usize;
        let mut cards = Vec::with_capacity(limit);
        if offset < MAX_OFFSET.min(total) {
            let want = offset + limit;
            let ords = match hits {
                Some(hits) if sort == PeopleSort::Relevance => sidecar.rank_hits(&hits, want),
                _ => sidecar.page(sort, &found, want).0,
            };
            for &ord in ords.iter().skip(offset).take(limit) {
                if let Some(card) = self.card(&searcher, Space::Person.raw(ord))? {
                    cards.push(card);
                }
            }
        }
        if cached {
            let mut cache = self.cache.lock().unwrap();
            let cache = cache.at(generation);
            if cache.people.len() >= PEOPLE_CACHE_ENTRIES {
                cache.people.clear();
            }
            cache.people.insert(key, (cards.clone(), total));
        }
        Ok((to_values(&cards), total))
    }

    /// Людей по основным ролям среди найденных по `q` (роли без людей нет).
    pub fn people_role_counts(&self, q: &str) -> Result<BTreeMap<String, i64>, IndexError> {
        let searcher = self.reader.searcher();
        let present = self.present(&searcher)?;
        let sidecar = self.people.load_full();
        let generation = self.generation(&present);
        let cached = self.cached.load(Ordering::Relaxed);
        if cached {
            if let Some(counts) = self.cache.lock().unwrap().at(generation).role_counts.get(q) {
                return Ok(counts.clone());
            }
        }
        // Без кэша это полный поиск людей (а без текста — роли 1,2 млн людей) на каждый запрос.
        let (found, _) = self.people_matches(&searcher, &present, &sidecar, q, None)?;
        let counts = sidecar.role_counts(&found);
        if cached {
            let mut cache = self.cache.lock().unwrap();
            let cache = cache.at(generation);
            if cache.role_counts.len() >= PEOPLE_CACHE_ENTRIES {
                cache.role_counts.clear();
            }
            cache.role_counts.insert(q.to_owned(), counts.clone());
        }
        Ok(counts)
    }
}

/// Идущая перестройка: документы добавляются пачками, [`Rebuild::commit`] заменяет ими прежние
/// документы области. Брошенная без коммита (или [`Rebuild::rollback`]) — прежний индекс
/// остаётся как был.
pub struct Rebuild {
    index: Arc<CatalogIndex>,
    writer: IndexWriter<TantivyDocument>,
    /// Сколько документов области было до перестройки.
    before: u64,
    added: u64,
}

impl Rebuild {
    pub fn add(&mut self, docs: &[TextDoc]) -> Result<(), IndexError> {
        for doc in docs {
            self.writer.add_document(self.index.document(doc))?;
        }
        self.added += docs.len() as u64;
        Ok(())
    }

    /// Сколько документов области было до перестройки.
    pub fn before(&self) -> u64 {
        self.before
    }

    /// Сколько документов добавлено.
    pub fn added(&self) -> u64 {
        self.added
    }

    /// Отказаться: прежний индекс остаётся.
    pub fn rollback(mut self) -> Result<(), IndexError> {
        self.writer.rollback()?;
        Ok(())
    }

    /// Коммит и ожидание слияния сегментов. Возвращает число добавленных документов.
    pub fn commit(mut self) -> Result<u64, IndexError> {
        self.writer.commit()?;
        self.writer.wait_merging_threads()?;
        self.index.reader.reload()?;
        self.index.warm()?;
        Ok(self.added)
    }
}
