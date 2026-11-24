//! Книжный поиск на Tantivy: индекс книг каталога.
//!
//! Источник правды — PostgreSQL, индекс производный: его целиком строит [`super::sync::rebuild`]
//! (`nexus books index`), а правки в админке и новые оценки досылает [`super::sync::update`].
//! Каталог — `BOOKS_INDEX_DIR` (по умолчанию `<SEARCH_INDEX_DIR>/books`).
//! **Два слоя.** Tantivy (слой А) держит только то, что меняется редко: текст для поиска
//! (название, авторы, серия), номер книги `ord` и хранимое (`STORED`) поле `card` — готовую
//! строку выдачи (`EntitySummary` в JSON), так что список книг отдаётся без похода в БД.
//! Фильтры (жанры и теги, люди по ролям, издательство, год, язык, обложка), сортировки и ручной
//! порядок — в слое Б ([`super::sidecar`]): он в RAM, обновляется ежедневно и подменяется
//! атомарно, не трогая индекс. Запрос без текста Tantivy вообще не вызывает; с текстом Tantivy
//! отдаёт номера и оценки совпадений (сборщик [`OrdHits`] сразу отсеивает книги не из фильтра),
//! а порядок строит слой Б. Аннотации в индексе нет: карточка книги читается из PostgreSQL и
//! раздаётся фронтендом как статическая страница.
//!
//! **Как ищет** (логика перенесена из настольной библиотеки Cassandra):
//! - слова запроса объединяются через И, поля одного слова — через ИЛИ;
//! - русский стеммер («преступления» находит «Преступление»), `ё` = `е`;
//! - последнее недописанное слово дополняется по словарю («прест» → «преступление»): не более
//!   [`PREFIX_MAX_TERMS`] самых частых слов, поэтому цена запроса ограничена при любом префиксе;
//! - опечатки — только если строгий запрос ничего не нашёл: одна правка в одном слове;
//! - точное название целиком и точная фраза — выше совпадения по отдельным словам.
//!
//! Индекс открывается через mmap (данные в page cache ОС, не в куче), писатель живёт только на
//! время записи и берёт [`WRITER_BUDGET`]. Повторные запросы отвечает кэш в памяти — он
//! сбрасывается сам, когда меняется содержимое индекса.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use chrono::{Datelike, NaiveDate};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use tantivy::collector::{Collector, SegmentCollector, TopDocs};
use tantivy::columnar::ColumnValues;
use tantivy::directory::error::LockError;
use tantivy::directory::MmapDirectory;
use tantivy::query::{
    BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, STRING,
};
use tantivy::{
    DocAddress, DocId, Index, IndexReader, IndexSettings, IndexWriter, ReloadPolicy, Score,
    Searcher, SegmentOrdinal, SegmentReader, TantivyDocument, TantivyError, Term,
};
use uuid::Uuid;

use shared::tantivy::{ru_raw_analyzer, ru_stem_analyzer, RU_RAW, RU_STEM, STOP_WORDS_RU};

use super::sidecar::Sidecar;
use crate::models::{EntityKind, EntitySummary, ReleaseStatus, SummaryAuthor};

/// Память писателя: 50 МБ, один поток. На сервере с малой RAM перестройка индекса из сотен
/// тысяч книг не должна мешать поиску и доходить до OOM killer.
pub const WRITER_BUDGET: usize = 50_000_000;

/// Глубже в выдачу не листаем: Tantivy собирает `offset + limit` лучших.
pub const MAX_OFFSET: usize = 100_000;

// --- Разбор запроса ---------------------------------------------------------

/// Максимум слов запроса: каждое — группа из нескольких клауз, длинный ввод дал бы запрос в
/// сотни клауз без пользы. Лишние слова отбрасываются.
const MAX_QUERY_TERMS: usize = 16;
/// Длина запроса в символах: дальше обрезается.
const MAX_QUERY_CHARS: usize = 200;
/// Опечатка в имени автора — только для слов не короче (у трёхбуквенного слова при одной
/// правке соседом оказывается почти любое).
const FUZZY_MIN_TERM_LEN: usize = 4;
/// Запрос короче (в символах, без пробелов по краям) не ищется вовсе — как в настольной
/// библиотеке: пока не набрано три символа, выдача пуста. Одна-две буквы совпадают с тысячами
/// слов, и на каждое нажатие шёл бы тяжёлый запрос ради бесполезной выдачи.
pub const MIN_QUERY_CHARS: usize = 3;
/// Автодополнение — только для слов не короче: префикс из двух букв раскрывается в тысячи слов.
const PREFIX_MIN_LEN: usize = 3;
/// Сколько слов словаря максимум подставляется вместо префикса (самые частые).
const PREFIX_MAX_TERMS: usize = 64;
/// Сколько слов словаря просматривать на сегмент в поисках частых.
const PREFIX_SCAN_LIMIT: usize = 4_000;
/// Токены длиннее (в байтах) — мусор (хеши, склейки слов): в индекс не идут.

// Веса клауз. BM25 Tantivy считает сам; константы задают только взаимный приоритет: точное
// название (несколько слов) > точное имя автора > фраза > точное название (одно слово) > слово
// в названии > автодополнение > слово в имени автора > серия > опечатка.
const BOOST_TITLE_KEY_MULTI: Score = 12.0;
const BOOST_AUTHOR_KEY: Score = 6.0;
const BOOST_TITLE_PHRASE: Score = 4.0;
const BOOST_TITLE_KEY_SINGLE: Score = 4.0;
const BOOST_FALLBACK_STRICT: Score = 4.0;
const BOOST_TITLE_TERM: Score = 2.0;
const BOOST_PREFIX: Score = 1.5;
const BOOST_AUTHOR_TERM: Score = 1.0;
const BOOST_SERIES_TERM: Score = 0.8;
const BOOST_TITLE_FUZZY: Score = 0.5;
const BOOST_AUTHOR_FUZZY: Score = 0.3;

// --- Кэш --------------------------------------------------------------------

/// Сколько ответов поиска держать в памяти. Страница — до 100 коротких карточек, так что кэш
/// ограничен десятками мегабайт в худшем случае и единицами — в обычном.
const CACHE_ENTRIES: usize = 512;
/// Страницы длиннее не кэшируются (служебные обходы — соседи, похожие — и так редки).
const CACHE_MAX_ITEMS: usize = 100;

/// Колонка номера книги (`ord`): индекс в массивах слоя Б.
const ORD: &str = "ord";

/// Ошибка индекса. `Busy` — писатель занят (идёт перестройка или другая запись).
#[derive(Debug)]
pub enum IndexError {
    Busy,
    Index(TantivyError),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "books index is locked by another writer"),
            Self::Index(error) => write!(f, "books index: {error}"),
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

/// Книга для индекса: всё, что о ней нужно поиску. Собирается из БД ([`super::sync`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookDoc {
    pub id: Uuid,
    /// Плотный номер книги (`catalog_book_ords.ord`): ключ слоя Б.
    pub ord: u32,
    /// Ручное место в общем списке книг (`catalog_manual_ranks`), `0` — нет.
    pub manual_rank: u16,
    pub slug: String,
    pub title: String,
    pub original_title: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
    pub cover_width: Option<i32>,
    pub cover_height: Option<i32>,
    /// Авторы по порядку: slug и имя (в карточку выдачи и в текст поиска).
    pub authors: Vec<SummaryAuthor>,
    /// Все участники: роль и slug человека (фильтр `person` + `role`).
    pub credits: Vec<(String, String)>,
    /// Slug'и тегов и жанров.
    pub tags: Vec<String>,
    pub publisher: Option<String>,
    pub series: Option<String>,
    pub language: Option<String>,
    pub has_description: bool,
    /// Сколько работ у самого плодовитого из авторов книги: мера известности автора. Своей
    /// статистики популярности у книг нет, а у классиков и популярных авторов изданий сотни.
    pub author_works: u32,
    /// Оценки Nexus: число и средняя.
    pub ratings_count: i64,
    pub rating_avg: Option<f64>,
    /// Что это за текст: книга, сборник, статья, документ (`metadata.kind` / `.collection`).
    pub form: BookForm,
}

/// Вид текста. Обычная выдача — только книги; остальное — по фильтру `book_form`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BookForm {
    #[default]
    Book,
    /// Несколько разных произведений в одном файле (`metadata.collection`).
    Collection,
    /// Статья, рецензия, интервью (`metadata.kind = article`).
    Article,
    /// Журнал, газета, документ (`metadata.kind = document`).
    Document,
}

impl BookForm {
    pub const ALL: [BookForm; 4] = [Self::Book, Self::Collection, Self::Article, Self::Document];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Book => "book",
            Self::Collection => "collection",
            Self::Article => "article",
            Self::Document => "document",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == value)
    }
}

/// Сортировка выдачи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BookSort {
    /// По совпадению с запросом; без запроса — как [`BookSort::Recommended`].
    #[default]
    Relevance,
    /// С оценками, обложкой и аннотацией — выше; при равенстве новее.
    Recommended,
    /// Только вышедшие, новые сверху.
    New,
    /// Только не вышедшие, ближайшие сверху.
    Soon,
    Title,
    /// Больше оценок Nexus — выше.
    Popular,
    /// Выше средняя оценка Nexus.
    Rating,
}

/// Запрос к индексу: текст, фильтры (через И) и сортировка.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct BookQuery {
    pub q: String,
    pub sort: BookSort,
    /// Slug тега или жанра.
    pub tag: Option<String>,
    /// Хотя бы один из тегов (похожие).
    pub any_tags: Vec<String>,
    /// Кроме этих книг (похожие).
    pub exclude_slugs: Vec<String>,
    pub year_from: Option<i32>,
    pub year_to: Option<i32>,
    /// Издательство — точное название без учёта регистра.
    pub publisher: Option<String>,
    /// Slug человека и его роль (`None` — любая).
    pub person: Option<String>,
    pub role: Option<String>,
    pub has_cover: Option<bool>,
    /// Код языка книги (`ru`).
    pub language: Option<String>,
    /// Виды текста (ИЛИ). Пусто — только [`BookForm::Book`]: сборники, статьи и документы в
    /// обычную выдачу не попадают.
    pub forms: Vec<BookForm>,
}

/// Часть размера сегмента по полям и куда её прибавить в [`FieldBytes`].
type FieldPart<'a> = (
    &'a tantivy::space_usage::PerFieldSpaceUsage,
    fn(&mut FieldBytes) -> &mut u64,
);

/// Место одного поля индекса на диске (сумма по сегментам), байты.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldBytes {
    /// Словарь термов.
    pub termdict: u64,
    /// Списки документов по термам.
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

/// Состояние слоя А ([`BookIndex::stats`]).
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub schema_version: &'static str,
    pub docs: u64,
    /// Удалённые, но ещё лежащие в сегментах документы (уйдут при слиянии).
    pub deleted_docs: u64,
    pub segments: usize,
    /// Всё, что занимает индекс по данным Tantivy.
    pub index_bytes: u64,
    /// Хранилище документов (поле `card`, сжато целиком — по полям не делится).
    pub store_bytes: u64,
    pub deletes_bytes: u64,
    /// Все файлы каталога (с неубранными старыми сегментами и снимком слоя Б).
    pub disk_bytes: u64,
    pub sidecar_file_bytes: u64,
    /// Поле → место.
    pub fields: Vec<(String, FieldBytes)>,
    pub cache_entries: usize,
    pub cache_ranked: usize,
}

/// Страница выдачи.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub items: Vec<EntitySummary>,
    pub total: usize,
    /// Строгий запрос ничего не нашёл — выдача собрана с исправлением опечатки.
    pub approximate: bool,
}

/// Строка выдачи, как она хранится в индексе (поле `card`).
#[derive(Debug, Serialize, Deserialize)]
struct Card {
    id: Uuid,
    slug: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    release_date: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover_width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover_height: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    authors: Vec<SummaryAuthor>,
}

impl Card {
    fn summary(self, today: NaiveDate) -> EntitySummary {
        EntitySummary {
            id: self.id,
            kind: EntityKind::Book,
            slug: self.slug,
            title: self.title,
            original_title: self.original_title,
            // Статус считается при чтении: «вышла» не должно зависеть от дня сборки индекса.
            release_status: match self.release_date {
                Some(date) if date > today => ReleaseStatus::Announced,
                _ => ReleaseStatus::Released,
            },
            release_date: self.release_date,
            cover_url: self.cover_url,
            cover_width: self.cover_width,
            cover_height: self.cover_height,
            imdb_rating: None,
            imdb_votes: None,
            authors: self.authors,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    id: Field,
    slug: Field,
    card: Field,
    title: Field,
    title_raw: Field,
    title_key: Field,
    author: Field,
    author_raw: Field,
    author_key: Field,
    series: Field,
    ord: Field,
}

/// Схема индекса: только текст, карточка и номер книги — то, что меняется редко. Фильтры и
/// сортировки — в слое Б ([`super::sidecar`]). Меняется схема — индекс на диске пересоздаётся
/// пустым ([`BookIndex::open`]) и строится заново из БД.
fn schema() -> (Schema, Fields) {
    let text = |tokenizer: &str, record: IndexRecordOption| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(tokenizer)
                .set_index_option(record),
        )
    };
    let mut b = Schema::builder();
    let fields = Fields {
        // uuid книги одним термом: по нему `delete_term` при обновлении.
        id: b.add_text_field("id", STRING),
        // Slug — терм (исключения, поиск соседа) и колонка (ряд выдачи без чтения документов).
        slug: b.add_text_field("slug", STRING | FAST),
        card: b.add_text_field("card", STORED),
        // Название со стеммером: позиции нужны точной фразе.
        title: b.add_text_field(
            "title",
            text(RU_STEM, IndexRecordOption::WithFreqsAndPositions),
        ),
        // Те же слова без стеммера: автодополнение и опечатки сравниваются с тем, что человек
        // напечатал, а не с обрезанной основой.
        title_raw: b.add_text_field("title_raw", text(RU_RAW, IndexRecordOption::Basic)),
        // Название целиком одним термом: «книга называется ровно так».
        title_key: b.add_text_field("title_key", STRING),
        author: b.add_text_field("author", text(RU_STEM, IndexRecordOption::WithFreqs)),
        author_raw: b.add_text_field("author_raw", text(RU_RAW, IndexRecordOption::Basic)),
        author_key: b.add_text_field("author_key", STRING),
        series: b.add_text_field("series", text(RU_STEM, IndexRecordOption::WithFreqs)),
        // Номер книги — колонка: сборщик совпадений ([`OrdHits`]) читает его без документа.
        ord: b.add_u64_field(ORD, FAST),
    };
    (b.build(), fields)
}

pub use shared::tantivy::normalize_key;

/// Ключ сортировки по названию: первые 9 знаков ключа названия, по 7 бит — порядок «пробел,
/// цифры, латиница, кириллица». Книги с общим началом длиннее 9 знаков идут в порядке индекса.
pub(crate) fn title_sort_key(title: &str) -> u64 {
    let mut key = 0u64;
    let mut chars = normalize_key(title).chars().collect::<Vec<_>>().into_iter();
    for _ in 0..9 {
        let code = match chars.next() {
            None => 0,
            Some(' ') => 1,
            Some(c @ '0'..='9') => 2 + (c as u64 - '0' as u64),
            Some(c @ 'a'..='z') => 12 + (c as u64 - 'a' as u64),
            Some(c @ 'а'..='я') => 38 + (c as u64 - 'а' as u64),
            Some(_) => 70,
        };
        key = (key << 7) | code;
    }
    key
}

pub(crate) fn day_of(date: NaiveDate) -> u64 {
    date.num_days_from_ce().max(0) as u64
}

impl BookDoc {
    /// Балл «Рекомендуемые»: книги с оценками выше, затем с обложкой, аннотацией и автором;
    /// затем известнее автор; при равенстве — новее. Своей статистики (как IMDb у фильмов) у
    /// книг нет.
    pub(crate) fn rank(&self) -> u64 {
        let rated = self.ratings_count.clamp(0, 1_000_000) as u64;
        let quality = u64::from(self.cover_url.is_some()) * 4
            + u64::from(self.has_description) * 2
            + u64::from(!self.authors.is_empty());
        let year = self
            .release_date
            .map_or(0, |date| date.year().clamp(0, 9999) as u64);
        let known = u64::from(self.author_works.min(0xFFFF));
        (rated << 40) | (quality << 32) | (known << 16) | year
    }

    fn card(&self) -> Card {
        Card {
            id: self.id,
            slug: self.slug.clone(),
            title: self.title.clone(),
            original_title: self.original_title.clone(),
            release_date: self.release_date,
            cover_url: self.cover_url.clone(),
            cover_width: self.cover_width,
            cover_height: self.cover_height,
            authors: self.authors.clone(),
        }
    }

    /// Отпечаток того, что книга кладёт в Tantivy (карточка, серия, номер). Совпал с прошлым
    /// (`catalog_book_ords.text_hash`) — переписывать документ незачем: новые оценки, теги и
    /// ручные места идут только в слой Б. FNV-1a: стабилен между версиями Rust (хранится в БД).
    pub fn text_hash(&self) -> i64 {
        let text =
            serde_json::to_string(&(self.card(), &self.series, self.ord)).unwrap_or_default();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in text.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash as i64
    }

    fn document(&self, f: &Fields) -> TantivyDocument {
        let mut doc = TantivyDocument::default();
        doc.add_text(f.id, self.id.to_string());
        doc.add_text(f.slug, &self.slug);
        doc.add_text(
            f.card,
            serde_json::to_string(&self.card()).unwrap_or_default(),
        );
        doc.add_u64(f.ord, u64::from(self.ord));

        for title in std::iter::once(&self.title).chain(&self.original_title) {
            doc.add_text(f.title, title);
            doc.add_text(f.title_raw, title);
            let key = normalize_key(title);
            if !key.is_empty() {
                doc.add_text(f.title_key, key);
            }
        }
        for author in &self.authors {
            doc.add_text(f.author, &author.name);
            doc.add_text(f.author_raw, &author.name);
            let key = normalize_key(&author.name);
            if !key.is_empty() {
                doc.add_text(f.author_key, key);
            }
        }
        if let Some(series) = &self.series {
            doc.add_text(f.series, series);
        }
        doc
    }
}

/// Сборщик совпадений: пары (номер книги, оценка) только для книг из `filter` (фасеты слоя Б
/// проверяются здесь же, а не после «лучших N»: узкий фильтр не теряет совпадений). Без
/// чтения документов — номер берётся из колонки `ord`.
pub struct OrdHits {
    filter: Arc<RoaringBitmap>,
}

pub struct OrdHitsSegment {
    ords: Arc<dyn ColumnValues<u64>>,
    filter: Arc<RoaringBitmap>,
    hits: Vec<(u32, Score)>,
}

impl Collector for OrdHits {
    type Fruit = Vec<(u32, Score)>;
    type Child = OrdHitsSegment;

    fn for_segment(
        &self,
        _segment: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        Ok(OrdHitsSegment {
            ords: reader
                .fast_fields()
                .u64(ORD)?
                .first_or_default_col(u64::MAX),
            filter: self.filter.clone(),
            hits: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(u32, Score)>>) -> tantivy::Result<Self::Fruit> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for OrdHitsSegment {
    type Fruit = Vec<(u32, Score)>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let ord = self.ords.get_val(doc);
        if let Ok(ord) = u32::try_from(ord) {
            if self.filter.contains(ord) {
                self.hits.push((ord, score));
            }
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.hits
    }
}

/// Строгий запрос по тексту и резервный — с одной опечаткой.
type TextQueries = (Box<dyn Query>, Option<Box<dyn Query>>);

/// Где в Tantivy лежит каждая книга: номер → адрес документа. Адреса меняются при слиянии
/// сегментов, поэтому карта строится заново на каждое состояние индекса ([`fingerprint`]).
struct TextMap {
    fingerprint: u64,
    addresses: Vec<DocAddress>,
    /// Книги, которые есть в индексе: пересекается с фильтром, чтобы книга, которую слой Б
    /// уже знает, а текст ещё не дописан, не дала пустую строку выдачи.
    indexed: RoaringBitmap,
}

const NO_ADDRESS: DocAddress = DocAddress {
    segment_ord: u32::MAX,
    doc_id: 0,
};

impl TextMap {
    fn build(searcher: &Searcher, fingerprint: u64) -> Result<Self, IndexError> {
        let mut addresses = Vec::new();
        let mut indexed = RoaringBitmap::new();
        for (segment_ord, segment) in searcher.segment_readers().iter().enumerate() {
            let ords = segment
                .fast_fields()
                .u64(ORD)?
                .first_or_default_col(u64::MAX);
            for doc in segment.doc_ids_alive() {
                let Ok(ord) = u32::try_from(ords.get_val(doc)) else {
                    continue;
                };
                if addresses.len() <= ord as usize {
                    addresses.resize(ord as usize + 1, NO_ADDRESS);
                }
                addresses[ord as usize] = DocAddress::new(segment_ord as u32, doc);
                indexed.insert(ord);
            }
        }
        Ok(Self {
            fingerprint,
            addresses,
            indexed,
        })
    }

    fn address(&self, ord: u32) -> Option<DocAddress> {
        self.addresses
            .get(ord as usize)
            .copied()
            .filter(|address| address.segment_ord != u32::MAX)
    }
}

/// Слово запроса: как напечатано (нижний регистр, `ё` → `е`) и после стеммера.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryTerm {
    raw: String,
    stem: String,
}

struct ParsedQuery {
    terms: Vec<QueryTerm>,
    /// Автодополнение последнего слова — `None`, если слово дописано или коротко.
    prefix: Option<Box<dyn Query>>,
}

fn is_stop_word(raw: &str) -> bool {
    STOP_WORDS_RU.contains(&raw)
}

/// Отпечаток содержимого индекса: сегменты и их удаления. Номер поколения читателя для этого
/// не годится — он растёт на каждой перезагрузке читателя, даже когда индекс не менялся
/// (наблюдатель за `meta.json` перезагружает его и сразу после открытия).
fn fingerprint(searcher: &Searcher) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (segment, deletes) in searcher.generation().segments() {
        segment.uuid_string().hash(&mut hasher);
        deletes.hash(&mut hasher);
    }
    hasher.finish()
}

/// Начало выдачи одного запроса в порядке сортировки: адреса документов и сколько всего
/// найдено. Страницы одного списка («Показать ещё», переход с первой страницы на вторую) —
/// срезы этого ряда: полная сортировка раздела в сотни тысяч книг делается один раз на список,
/// а не на каждую страницу.
struct Ranked {
    ords: Vec<u32>,
    total: usize,
    approximate: bool,
}

impl Ranked {
    /// В ряду есть вся страница (или выдача кончилась раньше).
    fn covers(&self, limit: usize, offset: usize) -> bool {
        self.ords.len() >= (offset + limit).min(self.total)
    }
}

/// Сколько первых книг списка держать в ряду: 1000 — это 40 страниц по 24. Глубже — ряд
/// пересчитывается с удвоенной длиной.
const RANKED_LEN: usize = 1000;
/// Сколько рядов держать: номер — 4 байта, так что это единицы мегабайт.
const RANKED_ENTRIES: usize = 256;

/// Списки раздела без текста и фильтров, которые открывают чаще всего: их ряды готовятся
/// заранее ([`BookIndex::warm`]), и первый посетитель не ждёт сортировки всего раздела.
const WARM_SORTS: [BookSort; 5] = [
    BookSort::Recommended,
    BookSort::New,
    BookSort::Title,
    BookSort::Popular,
    BookSort::Rating,
];

/// Ответы поиска по ключу запроса; живёт, пока не изменились индекс и слой Б.
#[derive(Default)]
struct Cache {
    /// Поколение ([`BookIndex::generation`]: отпечаток индекса и поколение слоя Б), для
    /// которого собраны записи.
    generation: u64,
    entries: HashMap<(BookQuery, usize, usize), Found>,
    order: VecDeque<(BookQuery, usize, usize)>,
    /// Ряды выдачи ([`Ranked`]) по запросу без `limit` и `offset`.
    ranked: HashMap<BookQuery, Arc<Ranked>>,
    ranked_order: VecDeque<BookQuery>,
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

/// Открытый книжный индекс: один на каталог в процессе.
pub struct BookIndex {
    dir: PathBuf,
    index: Index,
    reader: IndexReader,
    f: Fields,
    cache: Mutex<Cache>,
    /// Кэш ответов включён. Выключается только для замеров (`nexus search bench`).
    cached: std::sync::atomic::AtomicBool,
    /// Слой Б: фасеты, сортировки и ручной порядок. Читатели берут снимок без блокировок.
    sidecar: ArcSwap<Sidecar>,
    /// Номер книги → адрес документа для текущего состояния индекса.
    text_map: Mutex<Option<Arc<TextMap>>>,
}

/// Открытые индексы по каталогу (в тестах у каждого свой).
static OPEN: Mutex<Option<HashMap<PathBuf, Arc<BookIndex>>>> = Mutex::new(None);

/// Файл с версией схемы рядом с индексом: другая версия — индекс пересоздаётся.
const SCHEMA_FILE: &str = shared::tantivy::SCHEMA_FILE;
/// Версия схемы и анализаторов. Увеличить при любом их изменении.
const SCHEMA_VERSION: &str = "books-4";

impl BookIndex {
    /// Индекс в каталоге `dir`; нет или собран по другой схеме — создаётся пустой.
    pub fn open(dir: &Path) -> Result<Arc<Self>, IndexError> {
        let mut open = OPEN.lock().unwrap();
        let open = open.get_or_insert_with(HashMap::new);
        if let Some(index) = open.get(dir) {
            return Ok(index.clone());
        }
        std::fs::create_dir_all(dir).map_err(TantivyError::from)?;
        let (schema, fields) = schema();
        let version = dir.join(SCHEMA_FILE);
        let current = std::fs::read_to_string(&version).is_ok_and(|v| v.trim() == SCHEMA_VERSION);
        if !current {
            // Старую схему читать нельзя (другие поля и анализаторы): файлы индекса удаляются,
            // сервер построит индекс заново из БД.
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
            Index::create(directory, schema, IndexSettings::default())?
        };
        if !current {
            std::fs::write(&version, SCHEMA_VERSION).map_err(TantivyError::from)?;
        }
        // Tantivy хранит в meta.json только имя анализатора: регистрировать после открытия.
        index.tokenizers().register(RU_STEM, ru_stem_analyzer());
        index.tokenizers().register(RU_RAW, ru_raw_analyzer());
        // Коммиты другого процесса (`nexus books index`) читатель подхватывает сам.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let index = Arc::new(Self {
            dir: dir.to_path_buf(),
            index,
            reader,
            f: fields,
            cache: Mutex::new(Cache::default()),
            cached: std::sync::atomic::AtomicBool::new(true),
            // Снимок слоя Б с диска: поиск работает сразу, до первой сборки из БД.
            sidecar: ArcSwap::from_pointee(Sidecar::load(dir).unwrap_or_default()),
            text_map: Mutex::new(None),
        });
        open.insert(dir.to_path_buf(), index.clone());
        Ok(index)
    }

    /// Текущий слой Б.
    pub fn sidecar(&self) -> Arc<Sidecar> {
        self.sidecar.load_full()
    }

    /// Подменить слой Б целиком (ежедневная сборка): читатели видят старый или новый снимок
    /// целиком, кэш выдачи сбрасывается сам (другое поколение). Снимок пишется на диск.
    pub fn set_sidecar(&self, sidecar: Sidecar) -> Result<(), IndexError> {
        if let Err(error) = sidecar.save(&self.dir) {
            tracing::warn!(%error, "books sidecar snapshot was not saved");
        }
        self.sidecar.store(Arc::new(sidecar));
        self.warm()
    }

    /// Только ручные места (`ord → место`): фасеты и прочие порядки остаются как есть.
    pub fn set_ranks(&self, ranks: &HashMap<u32, u16>) -> Result<(), IndexError> {
        let next = self.sidecar.load().with_ranks(ranks);
        self.set_sidecar(next)
    }

    /// Поколение выдачи: меняется и с индексом, и со слоем Б.
    fn generation(&self, text: &TextMap, sidecar: &Sidecar) -> u64 {
        text.fingerprint ^ sidecar.generation().rotate_left(32)
    }

    /// Карта номер → адрес для состояния индекса, которое читает `searcher`.
    fn text_map(&self, searcher: &Searcher) -> Result<Arc<TextMap>, IndexError> {
        let fingerprint = fingerprint(searcher);
        let mut current = self.text_map.lock().unwrap();
        if let Some(map) = current
            .as_ref()
            .filter(|map| map.fingerprint == fingerprint)
        {
            return Ok(map.clone());
        }
        let map = Arc::new(TextMap::build(searcher, fingerprint)?);
        *current = Some(map.clone());
        Ok(map)
    }

    /// Включить или выключить кэш ответов (для замеров: сколько стоит сам запрос).
    pub fn set_cached(&self, cached: bool) {
        self.cached
            .store(cached, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Состояние индекса для админки: документы, сегменты и место на диске по полям.
    pub fn stats(&self) -> Result<IndexStats, IndexError> {
        let searcher = self.reader.searcher();
        let usage = searcher.space_usage().map_err(TantivyError::from)?;
        let mut fields: std::collections::BTreeMap<String, FieldBytes> = Default::default();
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
        let cache = self.cache.lock().unwrap();
        Ok(IndexStats {
            schema_version: SCHEMA_VERSION,
            docs: searcher.num_docs(),
            deleted_docs,
            segments: searcher.segment_readers().len(),
            index_bytes: usage.total().get_bytes(),
            store_bytes: store,
            deletes_bytes: deletes,
            disk_bytes,
            sidecar_file_bytes: file(super::sidecar::SNAPSHOT_FILE),
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

    /// Сколько книг в индексе.
    pub fn len(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn writer(&self) -> Result<IndexWriter<TantivyDocument>, IndexError> {
        Ok(self.index.writer_with_num_threads(1, WRITER_BUDGET)?)
    }

    /// Обновить книги: документы `ids` удаляются, `docs` добавляются (книга, которой в `docs`
    /// нет, — скрыта или удалена из БД). Один коммит.
    pub fn update(&self, ids: &[Uuid], docs: &[BookDoc]) -> Result<(), IndexError> {
        if ids.is_empty() && docs.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer()?;
        for id in ids.iter().chain(docs.iter().map(|doc| &doc.id)) {
            writer.delete_term(Term::from_field_text(self.f.id, &id.to_string()));
        }
        for doc in docs {
            writer.add_document(doc.document(&self.f))?;
        }
        writer.commit()?;
        self.reader.reload()?;
        self.warm()
    }

    /// Есть ли в индексе хоть одна из этих книг.
    pub fn has_any(&self, ids: &[Uuid]) -> Result<bool, IndexError> {
        let searcher = self.reader.searcher();
        for id in ids {
            if searcher.doc_freq(&Term::from_field_text(self.f.id, &id.to_string()))? > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Начать полную перестройку: прежние документы удаляются тем же коммитом, что добавит
    /// новые, — до него поиск читает старый индекс целиком.
    pub fn rebuild(self: &Arc<Self>) -> Result<Rebuild, IndexError> {
        let writer = self.writer()?;
        writer.delete_all_documents()?;
        Ok(Rebuild {
            index: self.clone(),
            writer,
            added: 0,
        })
    }

    // ------------------------------------------------------------ запрос

    /// Слова запроса: стемы — анализатором `title`, исходные — `title_raw`. Стеммер не
    /// добавляет и не удаляет токены, поэтому списки выравниваются один к одному.
    fn query_terms(&self, query: &str) -> Vec<QueryTerm> {
        let tokens = |name: &str| -> Vec<String> {
            let Some(mut analyzer) = self.index.tokenizers().get(name) else {
                return Vec::new();
            };
            let mut stream = analyzer.token_stream(query);
            let mut out = Vec::new();
            while let Some(token) = stream.next() {
                if out.len() == MAX_QUERY_TERMS {
                    break;
                }
                out.push(token.text.clone());
            }
            out
        };
        let raws = tokens(RU_RAW);
        tokens(RU_STEM)
            .into_iter()
            .enumerate()
            .map(|(index, stem)| QueryTerm {
                raw: raws.get(index).cloned().unwrap_or_else(|| stem.clone()),
                stem,
            })
            .collect()
    }

    fn parse(&self, searcher: &Searcher, query: &str) -> Result<ParsedQuery, IndexError> {
        let terms = self.query_terms(query);
        let prefix = match terms.last() {
            // Слово считается недописанным, если запрос не кончается пробелом или знаком.
            Some(last)
                if query.chars().last().is_some_and(char::is_alphanumeric)
                    && !is_stop_word(&last.raw)
                    && last.raw.chars().count() >= PREFIX_MIN_LEN =>
            {
                self.prefix_query(searcher, &last.raw)?
            }
            _ => None,
        };
        Ok(ParsedQuery { terms, prefix })
    }

    /// Автодополнение: `prefix` → ИЛИ точных слов `title_raw` / `author_raw`, которые с него
    /// начинаются. Словарь (FST) просматривается диапазоном `[prefix, …)` не дальше
    /// [`PREFIX_SCAN_LIMIT`] слов на сегмент, в запрос идут [`PREFIX_MAX_TERMS`] самых частых.
    /// Не edge n-gram (раздувает индекс в разы) и не `RegexQuery` (подставляет все слова).
    fn prefix_query(
        &self,
        searcher: &Searcher,
        prefix: &str,
    ) -> Result<Option<Box<dyn Query>>, IndexError> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for field in [self.f.title_raw, self.f.author_raw] {
            let mut freq: HashMap<Vec<u8>, u64> = HashMap::new();
            for segment in searcher.segment_readers() {
                let inverted = segment.inverted_index(field)?;
                let mut stream = inverted
                    .terms()
                    .range()
                    .ge(prefix.as_bytes())
                    .into_stream()
                    .map_err(TantivyError::from)?;
                let mut scanned = 0;
                while scanned < PREFIX_SCAN_LIMIT && stream.advance() {
                    let key = stream.key();
                    if !key.starts_with(prefix.as_bytes()) {
                        break;
                    }
                    scanned += 1;
                    *freq.entry(key.to_vec()).or_default() += u64::from(stream.value().doc_freq);
                }
            }
            let mut words: Vec<(Vec<u8>, u64)> = freq.into_iter().collect();
            words.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            words.truncate(PREFIX_MAX_TERMS);
            for (word, _) in words {
                let Ok(word) = std::str::from_utf8(&word) else {
                    continue;
                };
                clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, word),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        Ok(Some(Box::new(BoostQuery::new(
            Box::new(BooleanQuery::new(clauses)),
            BOOST_PREFIX,
        ))))
    }

    /// Обязательная группа одного слова: ИЛИ точных совпадений по названию, автору и серии,
    /// опечатка в имени автора, автодополнение (`prefix`) и — только в резервном запросе
    /// (`title_fuzzy`) — опечатка в названии.
    fn per_term(
        &self,
        term: &QueryTerm,
        title_fuzzy: bool,
        prefix: Option<&dyn Query>,
    ) -> (Occur, Box<dyn Query>) {
        let f = &self.f;
        let stem = term.stem.as_str();
        let boosted = |query: Box<dyn Query>, boost: Score| -> (Occur, Box<dyn Query>) {
            (Occur::Should, Box::new(BoostQuery::new(query, boost)))
        };
        let mut clauses = vec![
            boosted(
                Box::new(TermQuery::new(
                    Term::from_field_text(f.title, stem),
                    IndexRecordOption::WithFreqsAndPositions,
                )),
                BOOST_TITLE_TERM,
            ),
            boosted(
                Box::new(TermQuery::new(
                    Term::from_field_text(f.author, stem),
                    IndexRecordOption::WithFreqs,
                )),
                BOOST_AUTHOR_TERM,
            ),
            boosted(
                Box::new(TermQuery::new(
                    Term::from_field_text(f.series, stem),
                    IndexRecordOption::WithFreqs,
                )),
                BOOST_SERIES_TERM,
            ),
        ];
        if !is_stop_word(&term.raw) && term.raw.chars().count() >= FUZZY_MIN_TERM_LEN {
            // `true` — перестановка двух соседних букв стоит одну правку.
            clauses.push(boosted(
                Box::new(FuzzyTermQuery::new(
                    Term::from_field_text(f.author_raw, &term.raw),
                    1,
                    true,
                )),
                BOOST_AUTHOR_FUZZY,
            ));
            if title_fuzzy {
                clauses.push(boosted(
                    Box::new(FuzzyTermQuery::new(
                        Term::from_field_text(f.title_raw, &term.raw),
                        1,
                        true,
                    )),
                    BOOST_TITLE_FUZZY,
                ));
            }
        }
        if let Some(prefix) = prefix {
            clauses.push((Occur::Should, prefix.box_clone()));
        }
        (Occur::Must, Box::new(BooleanQuery::new(clauses)))
    }

    /// Строгий запрос: все слова обязательны; сверху — точная фраза и точные ключи.
    fn strict_query(&self, query: &str, parsed: &ParsedQuery) -> Option<Box<dyn Query>> {
        let terms = &parsed.terms;
        if terms.is_empty() {
            return None;
        }
        let f = &self.f;
        let last = terms.len() - 1;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
            .iter()
            .enumerate()
            .map(|(index, term)| {
                let prefix = (index == last)
                    .then_some(parsed.prefix.as_deref())
                    .flatten();
                self.per_term(term, false, prefix)
            })
            .collect();
        // Фраза — только от двух слов: для одного она задвоила бы вклад терма.
        if terms.len() >= 2 {
            let phrase: Vec<Term> = terms
                .iter()
                .map(|term| Term::from_field_text(f.title, &term.stem))
                .collect();
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(PhraseQuery::new(phrase)),
                    BOOST_TITLE_PHRASE,
                )),
            ));
        }
        let key = normalize_key(query);
        if !key.is_empty() {
            // Однословному точному названию вес меньше: «Маргарита» не должна подавить
            // «Мастер и Маргарита».
            let title_boost = if terms.len() >= 2 {
                BOOST_TITLE_KEY_MULTI
            } else {
                BOOST_TITLE_KEY_SINGLE
            };
            for (field, boost) in [(f.title_key, title_boost), (f.author_key, BOOST_AUTHOR_KEY)] {
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            Term::from_field_text(field, &key),
                            IndexRecordOption::Basic,
                        )),
                        boost,
                    )),
                ));
            }
        }
        Some(Box::new(BooleanQuery::new(clauses)))
    }

    /// Резервный запрос: тот же строгий, но одному слову названия разрешена опечатка. На каждое
    /// подходящее слово — свой вариант (в нём опечатка разрешена только ему), варианты — через
    /// ИЛИ; сам строгий запрос добавлен ради ранга. `None` — исправлять нечего.
    fn fallback_query(&self, parsed: &ParsedQuery, strict: &dyn Query) -> Option<Box<dyn Query>> {
        let terms = &parsed.terms;
        let fuzzy: Vec<usize> = terms
            .iter()
            .enumerate()
            .filter(|(_, term)| {
                !is_stop_word(&term.raw) && term.raw.chars().count() >= FUZZY_MIN_TERM_LEN
            })
            .map(|(index, _)| index)
            .collect();
        if fuzzy.is_empty() {
            return None;
        }
        let last = terms.len() - 1;
        let mut variants: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(fuzzy.len() + 1);
        for &fixed in &fuzzy {
            let clauses: Vec<(Occur, Box<dyn Query>)> = terms
                .iter()
                .enumerate()
                .map(|(index, term)| {
                    let prefix = (index == last)
                        .then_some(parsed.prefix.as_deref())
                        .flatten();
                    self.per_term(term, index == fixed, prefix)
                })
                .collect();
            variants.push((Occur::Should, Box::new(BooleanQuery::new(clauses))));
        }
        variants.push((
            Occur::Should,
            Box::new(BoostQuery::new(strict.box_clone(), BOOST_FALLBACK_STRICT)),
        ));
        Some(Box::new(BooleanQuery::new(variants)))
    }

    /// Номера книг из `slugs` (для «кроме этих»): slug — терм индекса, номер — его колонка.
    fn ords_of(&self, searcher: &Searcher, slugs: &[String]) -> Result<Vec<u32>, IndexError> {
        let mut out = Vec::with_capacity(slugs.len());
        for slug in slugs {
            let query = TermQuery::new(
                Term::from_field_text(self.f.slug, slug),
                IndexRecordOption::Basic,
            );
            let hits = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
            for (_, address) in hits {
                let segment = searcher.segment_reader(address.segment_ord);
                let ords = segment
                    .fast_fields()
                    .u64(ORD)?
                    .first_or_default_col(u64::MAX);
                if let Ok(ord) = u32::try_from(ords.get_val(address.doc_id)) {
                    out.push(ord);
                }
            }
        }
        Ok(out)
    }

    /// Поиск: страница `limit` книг с `offset` и сколько всего найдено.
    pub fn search(&self, q: &BookQuery, limit: usize, offset: usize) -> Result<Found, IndexError> {
        let searcher = self.reader.searcher();
        let text = self.text_map(&searcher)?;
        let sidecar = self.sidecar.load_full();
        let generation = self.generation(&text, &sidecar);
        let offset = offset.min(MAX_OFFSET);
        let key = (q.clone(), limit, offset);
        let cached = self.cached.load(std::sync::atomic::Ordering::Relaxed);
        if cached {
            if let Some(found) = self.cache.lock().unwrap().at(generation).entries.get(&key) {
                return Ok(found.clone());
            }
        }
        let ranked = if cached {
            self.ranked(&searcher, &text, &sidecar, generation, q, limit, offset)?
        } else {
            Arc::new(self.find(&searcher, &text, &sidecar, q, offset + limit)?)
        };
        let today = chrono::Utc::now().date_naive();
        let mut items = Vec::with_capacity(limit.min(ranked.total));
        for &ord in ranked.ords.iter().skip(offset).take(limit) {
            let Some(address) = text.address(ord) else {
                continue;
            };
            if let Some(card) = self.card(&searcher, address)? {
                items.push(card.summary(today));
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
    /// собранный заново (и тогда — с запасом, чтобы следующие страницы были срезами).
    #[allow(clippy::too_many_arguments)]
    fn ranked(
        &self,
        searcher: &Searcher,
        text: &TextMap,
        sidecar: &Sidecar,
        generation: u64,
        q: &BookQuery,
        limit: usize,
        offset: usize,
    ) -> Result<Arc<Ranked>, IndexError> {
        if let Some(ranked) = self.cache.lock().unwrap().at(generation).ranked.get(q) {
            if ranked.covers(limit, offset) {
                return Ok(ranked.clone());
            }
        }
        let len = (offset + limit).next_power_of_two().max(RANKED_LEN);
        let ranked = Arc::new(self.find(searcher, text, sidecar, q, len)?);
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

    /// Подготовить списки раздела без текста и фильтров ([`WARM_SORTS`]). Вызывается после
    /// открытия индекса и после каждой записи в него или в слой Б.
    pub fn warm(&self) -> Result<(), IndexError> {
        for sort in WARM_SORTS {
            let query = BookQuery {
                sort,
                ..BookQuery::default()
            };
            self.search(&query, 24, 0)?;
        }
        Ok(())
    }

    /// Запросы Tantivy по тексту: основной (строгий) и резервный с опечаткой — его выполняют,
    /// только если основной ничего не нашёл. Фильтров в них нет: их проверяет [`OrdHits`].
    fn text_queries(
        &self,
        searcher: &Searcher,
        text: &str,
    ) -> Result<Option<TextQueries>, IndexError> {
        let parsed = self.parse(searcher, text)?;
        let Some(strict) = self.strict_query(text, &parsed) else {
            return Ok(None);
        };
        let fallback = self.fallback_query(&parsed, strict.as_ref());
        Ok(Some((strict, fallback)))
    }

    /// Первые `len` номеров выдачи, сколько всего найдено и признак «приблизительно» (нашлось
    /// только с исправлением опечатки).
    ///
    /// - Без текста Tantivy не вызывается: фасеты слоя Б пересекаются, порядок — готовый.
    /// - С текстом Tantivy отдаёт совпадения из фильтра ([`OrdHits`]); релевантность с ручными
    ///   местами строит [`Sidecar::rank_hits`], остальные сортировки — готовые порядки слоя Б.
    fn find(
        &self,
        searcher: &Searcher,
        text: &TextMap,
        sidecar: &Sidecar,
        q: &BookQuery,
        len: usize,
    ) -> Result<Ranked, IndexError> {
        let empty = Ranked {
            ords: Vec::new(),
            total: 0,
            approximate: false,
        };
        let query: String = q.q.chars().take(MAX_QUERY_CHARS).collect();
        // Одна-две буквы — ещё не запрос (см. [`MIN_QUERY_CHARS`]); пустой — список по фильтрам.
        let typed = query.trim().chars().count();
        if (1..MIN_QUERY_CHARS).contains(&typed) {
            return Ok(empty);
        }
        let exclude = self.ords_of(searcher, &q.exclude_slugs)?;
        let mut filter = sidecar.filter(q, &exclude);
        filter &= &text.indexed;
        let today = chrono::Utc::now().date_naive();
        let Some((strict, fallback)) = self.text_queries(searcher, &query)? else {
            // Слов в запросе нет (пусто или одни знаки): все книги под фильтры.
            let (ords, total) = sidecar.page(q.sort, &filter, today, len);
            return Ok(Ranked {
                ords,
                total,
                approximate: false,
            });
        };
        let filter = Arc::new(filter);
        let collect = |query: &dyn Query| {
            searcher.search(
                query,
                &OrdHits {
                    filter: filter.clone(),
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
            return Ok(empty);
        }
        let total = hits.len();
        if q.sort == BookSort::Relevance {
            // Точное название запроса — выше любого ручного места.
            let key = normalize_key(&query);
            let exact: RoaringBitmap = if key.is_empty() {
                RoaringBitmap::new()
            } else {
                let title = TermQuery::new(
                    Term::from_field_text(self.f.title_key, &key),
                    IndexRecordOption::Basic,
                );
                collect(&title)?.into_iter().map(|(ord, _)| ord).collect()
            };
            let ords = sidecar.rank_hits(&hits, &exact, len);
            return Ok(Ranked {
                ords,
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

    fn card(&self, searcher: &Searcher, address: DocAddress) -> Result<Option<Card>, IndexError> {
        let doc: TantivyDocument = searcher.doc(address)?;
        Ok(doc
            .get_first(self.f.card)
            .and_then(|value| value.as_str())
            .and_then(|json| serde_json::from_str(json).ok()))
    }

    /// Сколько книг под запрос (без самой выдачи).
    pub fn count(&self, q: &BookQuery) -> Result<usize, IndexError> {
        let searcher = self.reader.searcher();
        let text = self.text_map(&searcher)?;
        let sidecar = self.sidecar.load_full();
        Ok(self.find(&searcher, &text, &sidecar, q, 0)?.total)
    }

    /// Slug'и первых `cap` книг выдачи по порядку — ряд для «предыдущая / следующая». Читается
    /// колонка `slug`, документы не распаковываются.
    pub fn slugs(&self, q: &BookQuery, cap: usize) -> Result<Vec<String>, IndexError> {
        let searcher = self.reader.searcher();
        let text = self.text_map(&searcher)?;
        let sidecar = self.sidecar.load_full();
        let ranked = self.find(&searcher, &text, &sidecar, q, cap)?;
        let mut columns = Vec::with_capacity(searcher.segment_readers().len());
        for segment in searcher.segment_readers() {
            columns.push(segment.fast_fields().str("slug")?);
        }
        let mut out = Vec::with_capacity(ranked.ords.len());
        let mut buffer = String::new();
        for ord in ranked.ords {
            let Some(address) = text.address(ord) else {
                continue;
            };
            let Some(column) = &columns[address.segment_ord as usize] else {
                continue;
            };
            if let Some(term) = column.term_ords(address.doc_id).next() {
                buffer.clear();
                column
                    .ord_to_str(term, &mut buffer)
                    .map_err(TantivyError::from)?;
                out.push(buffer.clone());
            }
        }
        Ok(out)
    }

    /// Карточки книг по slug'ам (в произвольном порядке; неизвестные пропускаются).
    pub fn by_slugs(&self, slugs: &[String]) -> Result<Vec<EntitySummary>, IndexError> {
        let searcher = self.reader.searcher();
        let today = chrono::Utc::now().date_naive();
        let mut out = Vec::with_capacity(slugs.len());
        for slug in slugs {
            let query = TermQuery::new(
                Term::from_field_text(self.f.slug, slug),
                IndexRecordOption::Basic,
            );
            let hits = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
            if let Some((_, address)) = hits.into_iter().next() {
                if let Some(card) = self.card(&searcher, address)? {
                    out.push(card.summary(today));
                }
            }
        }
        Ok(out)
    }

    // ---------------------------------------------------------- подсказки

    /// Теги и жанры книг со счётчиками, самые частые сверху (готовые в слое Б).
    pub fn tags(&self) -> Result<Arc<Vec<(String, u64)>>, IndexError> {
        Ok(Arc::new(self.sidecar.load().facets().tags.clone()))
    }

    /// Счётчики значений фасета (`tag:`, `lang:`, `person:`…) среди книг под запрос: для
    /// панели фильтров.
    pub fn facet_counts(
        &self,
        q: &BookQuery,
        prefix: &str,
    ) -> Result<Vec<(String, u64)>, IndexError> {
        let searcher = self.reader.searcher();
        let text = self.text_map(&searcher)?;
        let sidecar = self.sidecar.load_full();
        let ranked = self.find(&searcher, &text, &sidecar, q, usize::MAX)?;
        let found: RoaringBitmap = ranked.ords.into_iter().collect();
        Ok(sidecar.facet_counts(&found, prefix))
    }

    /// Издательства по части названия (без учёта регистра и `ё`): сначала те, что с неё
    /// начинаются, затем по числу книг. Пустой `q` — самые частые.
    pub fn publishers(&self, q: &str, limit: usize) -> Result<Vec<(String, u64)>, IndexError> {
        let sidecar = self.sidecar.load_full();
        let all = &sidecar.facets().publishers;
        let needle = normalize_key(q);
        if needle.is_empty() {
            return Ok(all.iter().take(limit).cloned().collect());
        }
        let mut hits: Vec<(bool, &(String, u64))> = all
            .iter()
            .filter_map(|hit| {
                let key = normalize_key(&hit.0);
                key.contains(&needle)
                    .then(|| (key.starts_with(&needle), hit))
            })
            .collect();
        // Список уже по убыванию счётчика; стабильная сортировка только поднимает «с начала».
        hits.sort_by_key(|(starts, _)| !*starts);
        Ok(hits
            .into_iter()
            .take(limit)
            .map(|(_, hit)| hit.clone())
            .collect())
    }
}

/// Идущая перестройка индекса: документы добавляются пачками, [`Rebuild::commit`] заменяет ими
/// прежний индекс. Брошенная без коммита — прежний индекс остаётся как был.
pub struct Rebuild {
    index: Arc<BookIndex>,
    writer: IndexWriter<TantivyDocument>,
    added: u64,
}

impl Rebuild {
    pub fn add(&mut self, docs: &[BookDoc]) -> Result<(), IndexError> {
        for doc in docs {
            self.writer.add_document(doc.document(&self.index.f))?;
        }
        self.added += docs.len() as u64;
        Ok(())
    }

    /// Коммит и ожидание слияния сегментов. Возвращает число книг.
    pub fn commit(mut self) -> Result<u64, IndexError> {
        self.writer.commit()?;
        self.writer.wait_merging_threads()?;
        self.index.reader.reload()?;
        self.index.warm()?;
        Ok(self.added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nexus-books-index-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn book(slug: &str, title: &str, author: &str) -> BookDoc {
        // Номер — из slug: у одной и той же книги он один при любом переиндексировании.
        let ord = slug
            .bytes()
            .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
            % 1_000_000;
        BookDoc {
            id: Uuid::new_v4(),
            ord,
            slug: slug.into(),
            title: title.into(),
            authors: vec![SummaryAuthor {
                slug: crate::archive::enrich::slugify(author),
                name: author.into(),
            }],
            credits: vec![("author".into(), crate::archive::enrich::slugify(author))],
            ..BookDoc::default()
        }
    }

    fn library(name: &str) -> Arc<BookIndex> {
        let index = BookIndex::open(&scratch(name)).unwrap();
        let mut crime = book(
            "prestuplenie",
            "Преступление и наказание",
            "Фёдор Достоевский",
        );
        crime.tags = vec!["klassika".into()];
        crime.cover_url = Some("http://x/c.avif".into());
        crime.release_date = NaiveDate::from_ymd_opt(1866, 1, 1);
        crime.publisher = Some("АСТ".into());
        let mut master = book("master", "Мастер и Маргарита", "Михаил Булгаков");
        master.tags = vec!["klassika".into(), "mistika".into()];
        master.release_date = NaiveDate::from_ymd_opt(1967, 1, 1);
        master
            .credits
            .push(("translator".into(), "ivan-ivanov".into()));
        let mut heart = book("serdtse", "Собачье сердце", "Михаил Булгаков");
        heart.series = Some("Московские повести".into());
        heart.ratings_count = 5;
        heart.rating_avg = Some(9.0);
        let hedgehog = book("ezhik", "Ёжик в тумане", "Сергей Козлов");
        let essay = book("razbor", "Мастер и Маргарита: краткий разбор", "Критик");
        put(&index, &[crime, master, heart, hedgehog, essay]);
        index
    }

    /// Книги — в индекс (слой А) и они же целиком — в слой Б.
    fn put(index: &BookIndex, docs: &[BookDoc]) {
        index.update(&[], docs).unwrap();
        index.set_sidecar(sidecar_of(docs)).unwrap();
    }

    fn sidecar_of(docs: &[BookDoc]) -> Sidecar {
        let mut builder = super::super::sidecar::SidecarBuilder::default();
        for doc in docs {
            builder.add(doc);
        }
        builder.finish()
    }

    fn ord_of(index: &BookIndex, slug: &str) -> u32 {
        let searcher = index.reader.searcher();
        index.ords_of(&searcher, &[slug.to_string()]).unwrap()[0]
    }

    fn slugs(index: &BookIndex, q: BookQuery) -> Vec<String> {
        index
            .search(&q, 20, 0)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.slug)
            .collect()
    }

    fn text(q: &str) -> BookQuery {
        BookQuery {
            q: q.into(),
            ..BookQuery::default()
        }
    }

    #[test]
    fn words_are_joined_with_and_and_stemmed() {
        let index = library("and");
        assert_eq!(
            slugs(&index, text("преступления наказание")),
            ["prestuplenie"]
        );
        // «и» есть везде, но второе слово обязано совпасть тоже.
        assert_eq!(
            slugs(&index, text("мастер и наказание")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn exact_title_ranks_above_longer_title() {
        let index = library("exact");
        assert_eq!(
            slugs(&index, text("мастер и маргарита")),
            ["master", "razbor"]
        );
    }

    #[test]
    fn unfinished_last_word_is_completed() {
        let index = library("prefix");
        assert_eq!(slugs(&index, text("прест")), ["prestuplenie"]);
        assert_eq!(slugs(&index, text("собачье сер")), ["serdtse"]);
        // Пробел в конце — слово дописано: «прест» целиком в названиях нет.
        assert!(slugs(&index, text("прест ")).is_empty());
    }

    #[test]
    fn nothing_is_searched_before_three_characters() {
        let index = library("short");
        assert!(slugs(&index, text("ёж")).is_empty());
        assert!(slugs(&index, text(" е ")).is_empty());
        assert_eq!(slugs(&index, text("ёжи")), ["ezhik"]);
        // Пустой запрос — не «короткий»: это список по фильтрам.
        assert_eq!(slugs(&index, BookQuery::default()).len(), 5);
    }

    #[test]
    fn yo_and_typos_are_forgiven() {
        let index = library("typo");
        assert_eq!(slugs(&index, text("ежик")), ["ezhik"]);
        assert_eq!(slugs(&index, text("ёжик")), ["ezhik"]);
        let found = index.search(&text("приступление"), 20, 0).unwrap();
        assert!(found.approximate, "found only through the typo fallback");
        assert_eq!(found.items[0].slug, "prestuplenie");
        assert!(
            !index
                .search(&text("преступление"), 20, 0)
                .unwrap()
                .approximate
        );
    }

    #[test]
    fn author_and_series_are_searched() {
        let index = library("author");
        let mut found = slugs(&index, text("булгаков"));
        found.sort();
        assert_eq!(found, ["master", "serdtse"]);
        assert_eq!(slugs(&index, text("московские повести")), ["serdtse"]);
        let authors = &index.search(&text("собачье"), 20, 0).unwrap().items[0].authors;
        assert_eq!(authors[0].name, "Михаил Булгаков");
    }

    #[test]
    fn filters_narrow_the_result() {
        let index = library("filters");
        let q = |change: fn(&mut BookQuery)| {
            let mut q = BookQuery::default();
            change(&mut q);
            let mut found = slugs(&index, q);
            found.sort();
            found
        };
        assert_eq!(q(|q| q.tag = Some("mistika".into())), ["master"]);
        assert_eq!(q(|q| q.has_cover = Some(true)), ["prestuplenie"]);
        assert_eq!(q(|q| q.publisher = Some("аст".into())), ["prestuplenie"]);
        assert_eq!(
            q(|q| q.person = Some("mikhail-bulgakov".into())),
            ["master", "serdtse"]
        );
        assert_eq!(
            q(|q| {
                q.person = Some("ivan-ivanov".into());
                q.role = Some("translator".into());
            }),
            ["master"]
        );
        assert!(q(|q| {
            q.person = Some("ivan-ivanov".into());
            q.role = Some("author".into());
        })
        .is_empty());
        assert_eq!(q(|q| q.year_from = Some(1900)), ["master"]);
        assert_eq!(q(|q| q.year_to = Some(1900)), ["prestuplenie"]);
        assert_eq!(
            q(|q| {
                q.any_tags = vec!["klassika".into()];
                q.exclude_slugs = vec!["master".into()];
            }),
            ["prestuplenie"]
        );
    }

    #[test]
    fn sorts_order_the_whole_result() {
        let index = library("sorts");
        let sorted = |sort| {
            slugs(
                &index,
                BookQuery {
                    sort,
                    ..BookQuery::default()
                },
            )
        };
        assert_eq!(sorted(BookSort::Rating)[0], "serdtse");
        assert_eq!(sorted(BookSort::Popular)[0], "serdtse");
        assert_eq!(sorted(BookSort::New), ["master", "prestuplenie"]);
        assert_eq!(
            sorted(BookSort::Title),
            ["ezhik", "master", "razbor", "prestuplenie", "serdtse"]
        );
        // Без запроса и сортировки — «Рекомендуемые»: с оценками выше.
        assert_eq!(sorted(BookSort::Relevance)[0], "serdtse");
    }

    #[test]
    fn pages_partition_the_result_and_total_is_exact() {
        let index = library("pages");
        let all = slugs(&index, BookQuery::default());
        assert_eq!(all.len(), 5);
        let mut paged = Vec::new();
        for offset in [0, 2, 4] {
            let found = index.search(&BookQuery::default(), 2, offset).unwrap();
            assert_eq!(found.total, 5);
            paged.extend(found.items.into_iter().map(|item| item.slug));
        }
        assert_eq!(paged, all);
        assert_eq!(index.slugs(&BookQuery::default(), 100).unwrap(), all);
    }

    #[test]
    fn deep_pages_are_slices_of_one_ranked_list() {
        let index = library("ranked");
        let all = slugs(&index, BookQuery::default());
        // Ряд собран первой страницей; вторая и «за концом» — его срезы.
        let second = index.search(&BookQuery::default(), 2, 2).unwrap();
        let slugs_of = |found: &Found| -> Vec<String> {
            found.items.iter().map(|item| item.slug.clone()).collect()
        };
        assert_eq!(slugs_of(&second), all[2..4]);
        assert_eq!(second.total, 5);
        let past = index.search(&BookQuery::default(), 2, 10).unwrap();
        assert!(past.items.is_empty());
        assert_eq!(past.total, 5);
        // С кэшем и без него выдача одна и та же.
        index.set_cached(false);
        assert_eq!(slugs(&index, BookQuery::default()), all);
        assert_eq!(
            slugs_of(&index.search(&BookQuery::default(), 2, 2).unwrap()),
            all[2..4]
        );
    }

    #[test]
    fn update_replaces_and_removes_books() {
        let index = library("update");
        let old = index.search(&text("ёжик"), 1, 0).unwrap().items.remove(0);
        let mut renamed = book("ezhik", "Ёжик и медвежонок", "Сергей Козлов");
        renamed.id = old.id;
        index.update(&[old.id], &[renamed]).unwrap();
        assert!(
            slugs(&index, text("тумане")).is_empty(),
            "cache is dropped on commit"
        );
        assert_eq!(slugs(&index, text("медвежонок")), ["ezhik"]);
        index.update(&[old.id], &[]).unwrap();
        assert!(slugs(&index, text("медвежонок")).is_empty());
        assert_eq!(index.len(), 4);
    }

    #[test]
    fn stats_report_fields_and_merge_drops_deleted_docs() {
        let index = library("stats");
        let old = index.search(&text("ёжик"), 1, 0).unwrap().items.remove(0);
        index.update(&[old.id], &[]).unwrap();
        let before = index.stats().unwrap();
        assert_eq!(before.docs, 4);
        assert!(before
            .fields
            .iter()
            .any(|(name, b)| name == "title" && b.total() > 0));
        assert!(before.store_bytes > 0);
        index.merge().unwrap();
        let after = index.stats().unwrap();
        assert_eq!((after.docs, after.deleted_docs, after.segments), (4, 0, 1));
        assert_eq!(slugs(&index, text("тумане")), Vec::<String>::new());
        index.clear_cache().unwrap();
    }

    #[test]
    fn rebuild_swaps_the_whole_index_in_one_commit() {
        let index = library("rebuild");
        let mut rebuild = index.rebuild().unwrap();
        let novaya = book("novaya", "Новая книга", "Автор");
        rebuild.add(std::slice::from_ref(&novaya)).unwrap();
        assert_eq!(index.len(), 5, "old index is served until commit");
        assert_eq!(rebuild.commit().unwrap(), 1);
        index.set_sidecar(sidecar_of(&[novaya])).unwrap();
        assert_eq!(slugs(&index, BookQuery::default()), ["novaya"]);
    }

    #[test]
    fn manual_rank_orders_listings_and_lifts_strong_text_matches() {
        let index = library("ranks");
        let ranks = HashMap::from([(ord_of(&index, "ezhik"), 1), (ord_of(&index, "razbor"), 2)]);
        index.set_ranks(&ranks).unwrap();
        // Без текста: ручные места сверху, кэш прежнего порядка сброшен.
        assert_eq!(
            slugs(&index, BookQuery::default())[..2],
            ["ezhik", "razbor"]
        );
        // Внутри фасета — относительный порядок общего списка.
        let classic = BookQuery {
            tag: Some("klassika".into()),
            ..BookQuery::default()
        };
        assert_eq!(slugs(&index, classic).len(), 2);
        // Точное название выше ручного места; «разбор» совпал сильно — второй.
        assert_eq!(
            slugs(&index, text("мастер и маргарита")),
            ["master", "razbor"]
        );
        // «Мастер» одним словом: точного названия нет, ручное место поднимает «разбор».
        assert_eq!(slugs(&index, text("мастер")), ["razbor", "master"]);
    }

    #[test]
    fn facet_filter_is_applied_inside_the_text_collector() {
        let index = library("collector");
        let q = BookQuery {
            q: "мастер".into(),
            tag: Some("mistika".into()),
            ..BookQuery::default()
        };
        let found = index.search(&q, 1, 0).unwrap();
        assert_eq!(found.total, 1);
        assert_eq!(found.items[0].slug, "master");
        let counts = index.facet_counts(&text("булгаков"), "tag:").unwrap();
        assert_eq!(counts[0], ("klassika".to_string(), 1));
    }

    #[test]
    fn hidden_in_sidecar_means_not_found() {
        let index = library("hidden");
        let docs: Vec<BookDoc> = vec![book("ezhik", "Ёжик в тумане", "Сергей Козлов")];
        index.set_sidecar(sidecar_of(&docs)).unwrap();
        assert!(slugs(&index, text("преступление")).is_empty());
        assert_eq!(slugs(&index, BookQuery::default()), ["ezhik"]);
    }

    #[test]
    fn sidecar_snapshot_is_loaded_on_open() {
        let dir = scratch("snapshot");
        {
            let index = BookIndex::open(&dir).unwrap();
            put(&index, &[book("ezhik", "Ёжик в тумане", "Сергей Козлов")]);
        }
        let loaded = Sidecar::load(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn facets_come_from_the_term_dictionary() {
        let index = library("facets");
        assert_eq!(index.tags().unwrap()[0], ("klassika".to_string(), 2));
        assert_eq!(index.publishers("", 10).unwrap(), [("АСТ".to_string(), 1)]);
        assert_eq!(index.publishers("ас", 10).unwrap().len(), 1);
        assert!(index.publishers("эксмо", 10).unwrap().is_empty());
    }

    #[test]
    fn keys_fold_case_punctuation_and_yo() {
        assert_eq!(normalize_key("  «Ёжик» — в тумане!  "), "ежик в тумане");
        assert!(title_sort_key("Азбука") < title_sort_key("Яблоко"));
        assert!(title_sort_key("1984") < title_sort_key("Azbuka"));
        assert!(title_sort_key("Zebra") < title_sort_key("Азбука"));
    }
}
