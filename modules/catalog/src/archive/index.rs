//! Индекс Tantivy холодного архива: схема, открытие, поиск, запись.
//!
//! Индекс открывается через mmap: данные читает page cache ОС, в куче процесса индекса нет —
//! миллион документов не съедает RAM. Писатель берёт ровно [`WRITER_BUDGET`] и живёт только на
//! время операции (сборка, промоушен): Tantivy пускает одного писателя на каталог, второй
//! получает `LockBusy` — так сборка и промоушен не пишут одновременно.
//!
//! Схема только растёт: новые поля добавляются в конец (`Field` — номер поля), поэтому индекс
//! старой схемы читается новым кодом (нового поля в документах просто нет). Перейти на новую
//! схему — `nexus archive build --fresh` ([`Archive::recreate`]).

use crate::models::EntityKind;
use chrono::NaiveDate;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tantivy::collector::Count;
use tantivy::collector::TopDocs;
use tantivy::directory::error::LockError;
use tantivy::directory::MmapDirectory;
use tantivy::query::{QueryParser, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, Value, FAST, INDEXED, STORED, STRING, TEXT,
};
use tantivy::{
    DocAddress, DocId, Index, IndexReader, IndexSettings, IndexWriter, ReloadPolicy, Score,
    SegmentReader, TantivyDocument, TantivyError, Term,
};
use utoipa::ToSchema;

/// Память писателя: 50 МБ на все потоки индексации. Без явного бюджета Tantivy берёт больше,
/// а на сервере с 4 ГБ RAM сборка архива из миллиона строк не должна дойти до OOM killer.
pub const WRITER_BUDGET: usize = 50_000_000;

/// Поле популярности (fast field): им взвешивается релевантность в [`Archive::search`].
const POPULARITY: &str = "popularity";

/// Поля схемы. `id` — slug (публичный id сущности, как в URL), `STRING`: один терм целиком,
/// по нему точный поиск и `delete_term`. `orig_title` хранится (в ТЗ — только индекс): без него
/// промоушен вернул бы фильм в каталог без оригинального названия. `cover_url` (постер) —
/// последнее поле, добавлено 2026-10-14.
#[derive(Debug, Clone, Copy)]
struct Fields {
    id: Field,
    title: Field,
    orig_title: Field,
    kind: Field,
    tmdb_id: Field,
    release_date: Field,
    popularity: Field,
    cover_url: Field,
}

/// Поле постера: его нет в индексах, собранных до 2026-10-14.
const COVER_URL: &str = "cover_url";

fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let fields = Fields {
        id: builder.add_text_field("id", STRING | STORED),
        title: builder.add_text_field("title", TEXT | STORED),
        orig_title: builder.add_text_field("orig_title", TEXT | STORED),
        kind: builder.add_text_field("type", STRING | STORED),
        tmdb_id: builder.add_u64_field("tmdb_id", INDEXED | STORED),
        // `YYYY-MM-DD`: при возврате в каталог фильм остаётся «вышедшим» (VISIBLE).
        release_date: builder.add_text_field("release_date", STORED),
        popularity: builder.add_f64_field(POPULARITY, STORED | FAST),
        // URL постера (как `entities.cover_url`), только хранится.
        cover_url: builder.add_text_field(COVER_URL, STORED),
    };
    (builder.build(), fields)
}

/// Документ архива — «скелет» фильма: то, что нужно для поиска, страницы фильма в архиве и
/// возврата в каталог.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ArchiveDoc {
    #[schema(example = "the-night-visitor-1971")]
    pub slug: String,
    #[schema(example = "Ночной посетитель")]
    pub title: String,
    #[schema(example = "The Night Visitor")]
    pub original_title: Option<String>,
    pub kind: EntityKind,
    /// Id фильма в TMDB: по нему импорт пропускает фильмы архива.
    #[schema(example = 42491)]
    pub tmdb_id: Option<i32>,
    #[schema(example = "1971-02-04")]
    pub release_date: Option<NaiveDate>,
    /// TMDB popularity на момент переноса в архив.
    #[schema(example = 1.84)]
    pub popularity: f64,
    /// Постер (TMDB `w780`, как `cover_url` карточки); нет у TMDB — `null`.
    #[schema(example = "https://image.tmdb.org/t/p/w780/kQJ1ZJ3ws3ixsNLE4d3rAc2Yh0C.jpg")]
    pub cover_url: Option<String>,
}

/// Ошибка индекса. `Busy` — писатель занят (идёт сборка или другой промоушен).
#[derive(Debug)]
pub enum ArchiveError {
    Busy,
    Index(TantivyError),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "archive index is locked by another writer"),
            Self::Index(error) => write!(f, "archive index: {error}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<TantivyError> for ArchiveError {
    fn from(error: TantivyError) -> Self {
        match error {
            TantivyError::LockFailure(LockError::LockBusy, _) => Self::Busy,
            error => Self::Index(error),
        }
    }
}

/// Открытый индекс архива: один на каталог в процессе (читатель следит за коммитами сам).
pub struct Archive {
    dir: PathBuf,
    index: Index,
    reader: IndexReader,
    fields: Fields,
    /// Схема индекса на диске знает `cover_url` (иначе постер при записи не сохраняется).
    covers: bool,
}

/// Открытые индексы по каталогу (в тестах у каждого свой).
static OPEN: Mutex<Option<HashMap<PathBuf, Arc<Archive>>>> = Mutex::new(None);

impl Archive {
    /// Индекс в каталоге `dir`; нет — создаётся пустой.
    pub fn open(dir: &Path) -> Result<Arc<Self>, ArchiveError> {
        let mut open = OPEN.lock().unwrap();
        let open = open.get_or_insert_with(HashMap::new);
        if let Some(archive) = open.get(dir) {
            return Ok(archive.clone());
        }
        std::fs::create_dir_all(dir).map_err(TantivyError::from)?;
        let (schema, fields) = schema();
        let directory = MmapDirectory::open(dir).map_err(TantivyError::from)?;
        // Индекс старой схемы открывается как есть (`open_or_create` требует точного совпадения).
        let index = if Index::exists(&directory).map_err(TantivyError::from)? {
            Index::open(directory)?
        } else {
            Index::create(directory, schema, IndexSettings::default())?
        };
        let archive = Arc::new(Self::with_index(dir, index, fields)?);
        open.insert(dir.to_path_buf(), archive.clone());
        Ok(archive)
    }

    fn with_index(dir: &Path, index: Index, fields: Fields) -> Result<Self, ArchiveError> {
        // Коммиты другого процесса (`nexus archive build`) читатель подхватывает сам.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            dir: dir.to_path_buf(),
            covers: index.schema().get_field(COVER_URL).is_ok(),
            index,
            reader,
            fields,
        })
    }

    /// Пустой индекс текущей схемы на месте этого (`build --fresh`): файлы индекса удаляются,
    /// файлы блокировок остаются. Пока идёт удаление, писатель держится — промоушен получит 409.
    /// Сервер, открывший индекс раньше, подхватит новый на коммите и прочтёт его целиком, с
    /// постерами: номера полей старой схемы те же, новое поле — последнее.
    pub fn recreate(&self) -> Result<Arc<Self>, ArchiveError> {
        let mut open = OPEN.lock().unwrap();
        let writer = self.writer()?;
        for entry in std::fs::read_dir(&self.dir).map_err(TantivyError::from)? {
            let path = entry.map_err(TantivyError::from)?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.starts_with(".tantivy-") && name.ends_with(".lock") {
                continue;
            }
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            removed.map_err(TantivyError::from)?;
        }
        drop(writer);
        let (schema, fields) = schema();
        let directory = MmapDirectory::open(&self.dir).map_err(TantivyError::from)?;
        let index = Index::create(directory, schema, IndexSettings::default())?;
        let archive = Arc::new(Self::with_index(&self.dir, index, fields)?);
        open.get_or_insert_with(HashMap::new)
            .insert(self.dir.clone(), archive.clone());
        Ok(archive)
    }

    /// Схема индекса на диске — текущая (есть `cover_url`).
    pub fn is_current(&self) -> bool {
        self.covers
    }

    /// Документов в архиве (без удалённых).
    pub fn len(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Поиск по `title` и `orig_title`: все слова запроса (синтаксис Tantivy разбирается
    /// снисходительно — кривой ввод не даёт ошибку). BM25 умножается на `1 + ln(1+popularity)/10`:
    /// из одинаково подходящих «Гамлетов» выше известный. Возвращает страницу и число всех
    /// найденных.
    pub fn search(
        &self,
        q: &str,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<ArchiveDoc>, usize), ArchiveError> {
        let fields = self.fields;
        let searcher = self.reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, vec![fields.title, fields.orig_title]);
        parser.set_conjunction_by_default();
        let (query, _errors) = parser.parse_query_lenient(q);
        let collector =
            TopDocs::with_limit(limit)
                .and_offset(offset)
                .tweak_score(|segment: &SegmentReader| {
                    let popularity = segment
                        .fast_fields()
                        .f64(POPULARITY)
                        .ok()
                        .map(|column| column.first_or_default_col(0.0));
                    move |doc: DocId, score: Score| {
                        let popularity = popularity.as_ref().map_or(0.0, |c| c.get_val(doc));
                        score * (1.0 + (popularity.max(0.0) as f32).ln_1p() / 10.0)
                    }
                });
        let (top, total): (Vec<(Score, DocAddress)>, usize) =
            searcher.search(&query, &(collector, Count))?;
        let docs = top
            .into_iter()
            .map(|(_, address)| Ok(self.parse(&searcher.doc(address)?)))
            .collect::<Result<_, ArchiveError>>()?;
        Ok((docs, total))
    }

    /// Документ по slug.
    pub fn get(&self, slug: &str) -> Result<Option<ArchiveDoc>, ArchiveError> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(self.slug_term(slug), IndexRecordOption::Basic);
        let top = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        match top.first() {
            Some((_, address)) => Ok(Some(self.parse(&searcher.doc(*address)?))),
            None => Ok(None),
        }
    }

    /// Все документы по порядку хранения (выгрузка в дамп): по одному, без загрузки в память.
    pub fn for_each(
        &self,
        mut f: impl FnMut(ArchiveDoc) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        let searcher = self.reader.searcher();
        let mut count = 0;
        for segment in searcher.segment_readers() {
            let store = segment.get_store_reader(1)?;
            for doc in store.iter::<TantivyDocument>(segment.alive_bitset()) {
                f(self.parse(&doc?))?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Писатель с бюджетом [`WRITER_BUDGET`]. Занят — [`ArchiveError::Busy`].
    pub fn writer(&self) -> Result<IndexWriter, ArchiveError> {
        Ok(self.index.writer(WRITER_BUDGET)?)
    }

    /// Добавить или заменить документ (по slug).
    pub fn upsert(&self, writer: &IndexWriter, doc: &ArchiveDoc) -> Result<(), ArchiveError> {
        let fields = self.fields;
        writer.delete_term(self.slug_term(&doc.slug));
        let mut document = TantivyDocument::new();
        document.add_text(fields.id, &doc.slug);
        document.add_text(fields.title, &doc.title);
        if let Some(original_title) = &doc.original_title {
            document.add_text(fields.orig_title, original_title);
        }
        document.add_text(fields.kind, doc.kind.as_str());
        if let Some(tmdb_id) = doc.tmdb_id.and_then(|id| u64::try_from(id).ok()) {
            document.add_u64(fields.tmdb_id, tmdb_id);
        }
        if let Some(date) = doc.release_date {
            document.add_text(fields.release_date, date.format("%Y-%m-%d").to_string());
        }
        document.add_f64(fields.popularity, doc.popularity);
        if let (true, Some(cover_url)) = (self.covers, &doc.cover_url) {
            document.add_text(fields.cover_url, cover_url);
        }
        writer.add_document(document)?;
        Ok(())
    }

    /// Пометить документ удалённым («ленивое удаление»: место освободит слияние сегментов).
    pub fn delete(&self, writer: &IndexWriter, slug: &str) {
        writer.delete_term(self.slug_term(slug));
    }

    /// Сразу показать поиску последний коммит, не дожидаясь `OnCommitWithDelay`.
    pub fn reload(&self) -> Result<(), ArchiveError> {
        Ok(self.reader.reload()?)
    }

    fn slug_term(&self, slug: &str) -> Term {
        Term::from_field_text(self.fields.id, slug)
    }

    fn parse(&self, doc: &TantivyDocument) -> ArchiveDoc {
        let fields = self.fields;
        let text = |field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        ArchiveDoc {
            slug: text(fields.id).unwrap_or_default(),
            title: text(fields.title).unwrap_or_default(),
            original_title: text(fields.orig_title),
            kind: text(fields.kind)
                .as_deref()
                .and_then(EntityKind::parse)
                .unwrap_or(EntityKind::Movie),
            tmdb_id: doc
                .get_first(fields.tmdb_id)
                .and_then(|v| v.as_u64())
                .and_then(|id| i32::try_from(id).ok()),
            release_date: text(fields.release_date)
                .and_then(|date| NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()),
            popularity: doc
                .get_first(fields.popularity)
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            cover_url: text(fields.cover_url),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::SchemaBuilder;

    /// Схема до 2026-10-14: без `cover_url`.
    fn legacy_schema() -> Schema {
        let mut builder: SchemaBuilder = Schema::builder();
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("title", TEXT | STORED);
        builder.add_text_field("orig_title", TEXT | STORED);
        builder.add_text_field("type", STRING | STORED);
        builder.add_u64_field("tmdb_id", INDEXED | STORED);
        builder.add_text_field("release_date", STORED);
        builder.add_f64_field(POPULARITY, STORED | FAST);
        builder.build()
    }

    fn doc(slug: &str, title: &str, cover_url: Option<&str>) -> ArchiveDoc {
        ArchiveDoc {
            slug: slug.into(),
            title: title.into(),
            original_title: None,
            kind: EntityKind::Movie,
            tmdb_id: Some(1),
            release_date: None,
            popularity: 1.0,
            cover_url: cover_url.map(str::to_owned),
        }
    }

    /// Индекс старой схемы читается и пишется (без постера); `recreate` даёт текущую схему, и
    /// открытый раньше объект (сервер) после коммита читает постеры.
    #[test]
    fn legacy_index_is_read_and_recreated() {
        let dir = std::env::temp_dir().join(format!("nexus-archive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let index = Index::create(
                MmapDirectory::open(&dir).unwrap(),
                legacy_schema(),
                Default::default(),
            )
            .unwrap();
            let mut writer: IndexWriter = index.writer(WRITER_BUDGET).unwrap();
            let mut document = TantivyDocument::new();
            let field = |name| index.schema().get_field(name).unwrap();
            document.add_text(field("id"), "old-film");
            document.add_text(field("title"), "Old Film");
            document.add_text(field("type"), "movie");
            document.add_f64(field(POPULARITY), 2.0);
            writer.add_document(document).unwrap();
            writer.commit().unwrap();
        }

        let legacy = Archive::open(&dir).unwrap();
        assert!(!legacy.is_current());
        let (found, total) = legacy.search("old", 20, 0).unwrap();
        assert_eq!(
            (found[0].slug.as_str(), found[0].cover_url.as_deref(), total),
            ("old-film", None, 1)
        );
        let mut writer = legacy.writer().unwrap();
        legacy
            .upsert(&writer, &doc("other", "Other", Some("https://x/a.jpg")))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        legacy.reload().unwrap();
        assert_eq!(legacy.get("other").unwrap().unwrap().cover_url, None);

        let current = legacy.recreate().unwrap();
        assert!(current.is_current());
        assert!(Arc::ptr_eq(&current, &Archive::open(&dir).unwrap()));
        assert!(current.is_empty());
        let mut writer = current.writer().unwrap();
        current
            .upsert(
                &writer,
                &doc("new-film", "New Film", Some("https://x/b.jpg")),
            )
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        current.reload().unwrap();
        assert_eq!(
            current
                .get("new-film")
                .unwrap()
                .unwrap()
                .cover_url
                .as_deref(),
            Some("https://x/b.jpg")
        );

        legacy.reload().unwrap();
        assert_eq!(legacy.len(), 1);
        assert!(legacy.get("old-film").unwrap().is_none());
        let (found, total) = legacy.search("film", 20, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(found[0].cover_url.as_deref(), Some("https://x/b.jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
