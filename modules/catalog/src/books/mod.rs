//! Импорт метаданных русских книг из SQL-дампа LibGen (`nexus books import`): `entities`
//! (kind = 'book'), источник — `metadata.libgen` (ключ произведения и файлы: md5, формат, id),
//! авторы — `people` + `entity_credits` (role = 'author'). Своих таблиц нет. Сами файлы книг не
//! скачиваются и не хранятся.
//!
//! Дамп читается потоково ([`dump::Statements`]), можно `.gz`. Колонки — по `CREATE TABLE` из
//! дампа (без него — порядок таблицы `updated` LibGen, [`UPDATED_COLUMNS`]). Берутся строки с
//! русским языком, видимые (`Visible` пустой), с корректным md5 и названием. Файлы одной книги
//! (одно название и авторы, разные форматы) — одна сущность. Повторный запуск пропускает известные
//! md5; у существующей книги дописывает только новые файлы в `metadata.libgen.files`, остальное
//! (могли править в админке) не трогает.

pub mod dump;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde_json::{json, Value};
use shared::AppState;
use uuid::Uuid;

use crate::archive::enrich::slugify;
use crate::metadata;
use crate::models::EntityKind;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Обложки LibGen в дампе — относительные пути (`1234000/<md5>.jpg`).
pub const COVERS: &str = "https://libgen.is/covers/";

/// Колонки таблицы `updated` (libgen.sql), если в дампе нет `CREATE TABLE`.
pub const UPDATED_COLUMNS: &[&str] = &[
    "ID",
    "Title",
    "VolumeInfo",
    "Series",
    "Periodical",
    "Author",
    "Year",
    "Edition",
    "Publisher",
    "City",
    "Pages",
    "PagesInFile",
    "Language",
    "Topic",
    "Library",
    "Issue",
    "Identifier",
    "ISSN",
    "ASIN",
    "UDC",
    "LBC",
    "DDC",
    "LCC",
    "Doi",
    "Googlebookid",
    "OpenLibraryID",
    "Commentary",
    "DPI",
    "Color",
    "Cleaned",
    "Orientation",
    "Paginated",
    "Scanned",
    "Bookmarked",
    "Searchable",
    "Filesize",
    "Extension",
    "MD5",
    "Generic",
    "Visible",
    "Locator",
    "Local",
    "TimeAdded",
    "TimeLastModified",
    "Coverurl",
    "Tags",
    "IdentifierWODash",
];

const MAX_TITLE: usize = 300;
const MAX_NAME: usize = 200;
const MAX_AUTHORS: usize = 5;
const MAX_SLUG_BASE: usize = 80;
const MAX_WORK_KEY: usize = 1000;
const MAX_EXTENSION: usize = 20;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub dump: PathBuf,
    /// Таблица дампа: `updated` (libgen.sql) или `fiction` (fiction.sql).
    pub table: String,
    pub batch_size: usize,
    /// Не больше стольких подходящих (русских) строк дампа.
    pub limit: Option<u64>,
}

impl ImportOptions {
    pub fn new(dump: PathBuf) -> Self {
        Self {
            dump,
            table: "updated".into(),
            batch_size: 2000,
            limit: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ImportReport {
    /// Строки таблицы в дампе.
    pub rows: u64,
    /// Из них подошли: русский язык, видимые, md5 и название есть.
    pub russian: u64,
    /// md5 уже были в `metadata.libgen.files` какой-то книги.
    pub known: u64,
    pub books_created: u64,
    /// Новые файлы (md5), привязанные к книгам, — новым и уже существующим.
    pub files_linked: u64,
    pub people_created: u64,
    pub credits_created: u64,
}

impl fmt::Display for ImportReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "dump rows:       {}", self.rows)?;
        writeln!(f, "russian rows:    {}", self.russian)?;
        writeln!(f, "already known:   {}", self.known)?;
        writeln!(f, "books created:   {}", self.books_created)?;
        writeln!(f, "files linked:    {}", self.files_linked)?;
        writeln!(f, "people created:  {}", self.people_created)?;
        writeln!(f, "author credits:  {}", self.credits_created)
    }
}

/// Строка дампа, пригодная для импорта.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub libgen_id: Option<i64>,
    pub md5: String,
    pub title: String,
    pub authors: Vec<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
    pub extension: Option<String>,
    /// Провалидированный `metadata` книги (`isbn`, `pages`, `publisher`).
    pub metadata: Value,
}

impl Book {
    /// Ключ «произведения» (`metadata.libgen.work`): файлы с одинаковыми названием и авторами —
    /// одна книга.
    pub fn work_key(&self) -> String {
        let authors: Vec<String> = self.authors.iter().map(|a| name_key(a)).collect();
        let key = format!("{}|{}", name_key(&self.title), authors.join(";"));
        match key.char_indices().nth(MAX_WORK_KEY) {
            Some((i, _)) => key[..i].to_string(),
            None => key,
        }
    }

    fn file(&self) -> Value {
        let mut file = json!({ "md5": self.md5 });
        if let Some(extension) = &self.extension {
            file["extension"] = json!(extension);
        }
        if let Some(id) = self.libgen_id {
            file["id"] = json!(id);
        }
        file
    }
}

/// Позиции нужных колонок в строке дампа.
struct Columns {
    id: Option<usize>,
    title: usize,
    author: Option<usize>,
    year: Option<usize>,
    language: usize,
    extension: Option<usize>,
    md5: usize,
    coverurl: Option<usize>,
    identifier: Option<usize>,
    publisher: Option<usize>,
    pages: Option<usize>,
    visible: Option<usize>,
}

impl Columns {
    fn new<S: AsRef<str>>(names: &[S]) -> Result<Self, BoxError> {
        let find = |name: &str| {
            names
                .iter()
                .position(|n| n.as_ref().eq_ignore_ascii_case(name))
        };
        let need = |name: &str| find(name).ok_or_else(|| format!("dump has no column {name}"));
        Ok(Self {
            id: find("ID"),
            title: need("Title")?,
            author: find("Author"),
            year: find("Year"),
            language: need("Language")?,
            extension: find("Extension"),
            md5: need("MD5")?,
            coverurl: find("Coverurl"),
            identifier: find("Identifier"),
            publisher: find("Publisher"),
            pages: find("Pages"),
            visible: find("Visible"),
        })
    }

    fn book(&self, row: &[Option<String>]) -> Option<Book> {
        let get = |i: Option<usize>| {
            i.and_then(|i| row.get(i))
                .and_then(|v| v.as_deref())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        if !is_russian(get(Some(self.language))?) || get(self.visible).is_some() {
            return None;
        }
        let md5 = get(Some(self.md5))?.to_ascii_lowercase();
        if md5.len() != 32 || !md5.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let title = clean(get(Some(self.title))?, MAX_TITLE)?;
        Some(Book {
            libgen_id: get(self.id).and_then(|v| v.parse().ok()),
            md5,
            title,
            authors: get(self.author).map(split_authors).unwrap_or_default(),
            release_date: get(self.year).and_then(year),
            cover_url: get(self.coverurl).map(cover_url),
            extension: get(self.extension)
                .map(|e| e.to_lowercase())
                .filter(|e| e.chars().count() <= MAX_EXTENSION),
            metadata: book_metadata(get(self.identifier), get(self.pages), get(self.publisher)),
        })
    }
}

/// `Russian`, `rus`, `ru`, `Русский`, в том числе в списке `Russian, English`.
pub fn is_russian(language: &str) -> bool {
    language
        .split([',', ';', '/', '+'])
        .map(|l| l.trim().to_lowercase())
        .any(|l| matches!(l.as_str(), "ru" | "rus" | "russian" | "русский"))
}

/// Схлопывает пробелы, обрезает до `max` символов; пустая — `None`.
fn clean(text: &str, max: usize) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = match text.char_indices().nth(max) {
        Some((i, _)) => text[..i].trim_end().to_string(),
        None => text,
    };
    (!text.is_empty()).then_some(text)
}

/// Первые 4 цифры года (`1998`, `1998-2001`, `[1998]`) → 1 января; вне 1000–2100 — `None`.
fn year(value: &str) -> Option<NaiveDate> {
    let digits: String = value
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    let y: i32 = digits.parse().ok()?;
    if digits.len() != 4 || !(1000..=2100).contains(&y) {
        return None;
    }
    NaiveDate::from_ymd_opt(y, 1, 1)
}

fn cover_url(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{COVERS}{}", path.trim_start_matches('/'))
    }
}

/// `metadata` книги через общую валидацию каталога: первый корректный ISBN из `Identifier`
/// (`5-17-012345-6,9785170123456`), страницы (`352[360]` → 352), издательство. Невалидное поле
/// отбрасывается — карточку потом можно сохранить в админке.
fn book_metadata(identifier: Option<&str>, pages: Option<&str>, publisher: Option<&str>) -> Value {
    let mut base = serde_json::Map::new();
    if let Some(pages) = pages
        .map(|p| {
            p.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| (1..=100_000).contains(p))
    {
        base.insert("pages".into(), json!(pages));
    }
    if let Some(publisher) = publisher.and_then(|p| clean(p, 200)) {
        base.insert("publisher".into(), json!(publisher));
    }
    let base = metadata::validate(EntityKind::Book, Value::Object(base.clone()))
        .unwrap_or_else(|_| json!({}));
    for isbn in identifier.unwrap_or("").split([',', ';', ' ']) {
        let isbn = isbn.trim();
        if isbn.is_empty() {
            continue;
        }
        let mut candidate = base.clone();
        candidate["isbn"] = json!(isbn);
        if let Ok(valid) = metadata::validate(EntityKind::Book, candidate) {
            return valid;
        }
    }
    base
}

/// Авторы из поля `Author`: через `,` или `;`. `Толстой, Лев Николаевич` и `Толстой, Л.Н.` —
/// один автор (фамилия, затем имя); `Ильф, Петров` — два. Не больше [`MAX_AUTHORS`].
pub fn split_authors(field: &str) -> Vec<String> {
    let mut authors: Vec<String> = Vec::new();
    let mut merge_next = false;
    for part in field.split([';', ',']) {
        let Some(part) = clean(part, MAX_NAME) else {
            continue;
        };
        let words = part.split(' ').count();
        let initials =
            part.contains('.') && part.chars().filter(|c| c.is_alphabetic()).count() <= 3;
        match authors.last_mut() {
            Some(last) if merge_next && (words >= 2 || initials) => {
                last.push(' ');
                last.push_str(&part);
                merge_next = false;
                continue;
            }
            _ => {}
        }
        merge_next = words == 1 && !initials;
        if !authors.iter().any(|a| name_key(a) == name_key(&part)) {
            authors.push(part);
        }
    }
    authors.retain(|a| a.chars().filter(|c| c.is_alphabetic()).count() >= 2);
    authors.truncate(MAX_AUTHORS);
    authors
}

/// Ключ сравнения имён и названий: регистр, `ё` и лишние пробелы не важны. Совпадает с
/// `replace(lower(full_name), 'ё', 'е')` (индекс `people_name_key_idx`) для очищенных имён.
pub fn name_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace('ё', "е")
}

/// Детерминированный короткий суффикс slug'а (FNV-1a).
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", hash as u32)
}

/// Кандидаты slug'а: `voyna-i-mir`, затем `voyna-i-mir-<hash>`; без латиницы и кириллицы —
/// `<prefix>-<hash>`.
fn slug_candidates(prefix: &str, text: &str, key: &str) -> [String; 2] {
    let mut base = slugify(text);
    if let Some((i, _)) = base.char_indices().nth(MAX_SLUG_BASE) {
        base = base[..i].trim_end_matches('-').to_string();
    }
    let hash = short_hash(key);
    if base.is_empty() {
        let slug = format!("{prefix}-{hash}");
        [slug.clone(), slug]
    } else {
        [base.clone(), format!("{base}-{hash}")]
    }
}

fn open(path: &Path) -> Result<Box<dyn BufRead + Send>, BoxError> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let gz = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"));
    Ok(if gz {
        Box::new(BufReader::with_capacity(
            1 << 20,
            flate2::read::MultiGzDecoder::new(file),
        ))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, file))
    })
}

/// Импорт дампа. Чтение файла — в отдельном потоке, запись — пачками по `batch_size`, каждая в
/// своей транзакции (прерванный импорт продолжается повторным запуском).
pub async fn import(state: &AppState, options: &ImportOptions) -> Result<ImportReport, BoxError> {
    let input = open(&options.dump)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u64, Vec<Book>), String>>(2);
    let table = options.table.clone();
    let batch_size = options.batch_size.max(1);
    let limit = options.limit;
    let reader = tokio::task::spawn_blocking(move || {
        let send = |message| tx.blocking_send(message).is_ok();
        let mut statements = dump::Statements::new(input);
        let mut columns: Option<Columns> = None;
        let mut batch = Vec::with_capacity(batch_size);
        let (mut rows, mut taken) = (0u64, 0u64);
        loop {
            let statement = match statements.next_statement() {
                Ok(Some(s)) => s,
                Ok(None) => break,
                Err(e) => {
                    send(Err(format!("read dump: {e}")));
                    return;
                }
            };
            if let Some((name, names)) = dump::create_table(&statement) {
                if name.eq_ignore_ascii_case(&table) {
                    match Columns::new(&names) {
                        Ok(c) => columns = Some(c),
                        Err(e) => {
                            send(Err(e.to_string()));
                            return;
                        }
                    }
                }
                continue;
            }
            let Some(insert) = dump::insert(&statement) else {
                continue;
            };
            if !insert.table.eq_ignore_ascii_case(&table) {
                continue;
            }
            let own;
            let cols = match (&insert.columns, &columns) {
                (Some(names), _) => match Columns::new(names) {
                    Ok(c) => {
                        own = c;
                        &own
                    }
                    Err(e) => {
                        send(Err(e.to_string()));
                        return;
                    }
                },
                (None, Some(c)) => c,
                (None, None) => {
                    columns = Some(Columns::new(UPDATED_COLUMNS).expect("known columns"));
                    columns.as_ref().expect("just set")
                }
            };
            for row in &insert.rows {
                rows += 1;
                if limit.is_some_and(|l| taken >= l) {
                    break;
                }
                if let Some(book) = cols.book(row) {
                    taken += 1;
                    batch.push(book);
                    if batch.len() >= batch_size && !send(Ok((rows, std::mem::take(&mut batch)))) {
                        return;
                    }
                }
            }
            if limit.is_some_and(|l| taken >= l) {
                break;
            }
        }
        send(Ok((rows, batch)));
    });

    let mut report = ImportReport::default();
    while let Some(message) = rx.recv().await {
        let (rows, batch) = message?;
        report.rows = rows;
        report.russian += batch.len() as u64;
        write_batch(state, &batch, &mut report).await?;
        tracing::info!(
            rows = report.rows,
            russian = report.russian,
            books = report.books_created,
            people = report.people_created,
            "books import progress"
        );
    }
    reader.await?;
    Ok(report)
}

/// Свободные slug'и из кандидатов: занятые в `table` и уже выданные в этой пачке пропускаются.
async fn pick_slugs(
    tx: &mut sqlx::PgConnection,
    table: &str,
    candidates: &[[String; 2]],
) -> sqlx::Result<Vec<Option<String>>> {
    let all: Vec<&String> = candidates.iter().flatten().collect();
    let sql = format!("SELECT slug FROM {table} WHERE slug = ANY($1)");
    let mut taken: HashSet<String> = sqlx::query_scalar(&sql)
        .bind(all)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
    Ok(candidates
        .iter()
        .map(|c| {
            let slug = c.iter().find(|s| !taken.contains(*s))?.clone();
            taken.insert(slug.clone());
            Some(slug)
        })
        .collect())
}

/// Пачка в одной транзакции: новые файлы существующих книг, новые книги, их авторы и титры.
async fn write_batch(
    state: &AppState,
    batch: &[Book],
    report: &mut ImportReport,
) -> Result<(), BoxError> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;

    // 1. Известные файлы пропускаем; дубли md5 внутри пачки — тоже.
    let md5s: Vec<&str> = batch.iter().map(|b| b.md5.as_str()).collect();
    let known: HashSet<String> = sqlx::query_scalar(
        "SELECT m FROM entities e,
              jsonb_array_elements_text(jsonb_path_query_array(e.metadata, '$.libgen.files[*].md5')) m
         WHERE e.kind = 'book'
           AND jsonb_path_query_array(e.metadata, '$.libgen.files[*].md5') ?| $1
           AND m = ANY($1)",
    )
    .bind(&md5s)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let mut seen = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut works: HashMap<String, Vec<&Book>> = HashMap::new();
    for book in batch {
        if known.contains(&book.md5) || !seen.insert(book.md5.as_str()) {
            report.known += 1;
            continue;
        }
        let key = book.work_key();
        works
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push(book);
    }
    if order.is_empty() {
        return Ok(tx.commit().await?);
    }

    // 2. Книги, уже созданные по другим файлам того же произведения: дописываем файлы.
    let existing: HashMap<String, (Uuid, i32)> = sqlx::query_as::<_, (String, Uuid, i32)>(
        "SELECT DISTINCT ON (metadata #>> '{libgen,work}') metadata #>> '{libgen,work}', id,
                coalesce(jsonb_array_length(metadata #> '{libgen,files}'), 0)
         FROM entities
         WHERE kind = 'book' AND metadata ? 'libgen' AND metadata #>> '{libgen,work}' = ANY($1)
         ORDER BY metadata #>> '{libgen,work}', created_at",
    )
    .bind(&order)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(key, id, files)| (key, (id, files)))
    .collect();
    let (mut ids, mut files) = (Vec::new(), Vec::new());
    for key in &order {
        let Some((id, count)) = existing.get(key) else {
            continue;
        };
        let room = metadata::MAX_LIBGEN_FILES.saturating_sub(*count as usize);
        let add: Vec<Value> = works[key].iter().take(room).map(|b| b.file()).collect();
        if !add.is_empty() {
            report.files_linked += add.len() as u64;
            ids.push(*id);
            files.push(Value::Array(add));
        }
    }
    sqlx::query(
        "UPDATE entities e
         SET metadata = jsonb_set(e.metadata, '{libgen,files}', e.metadata #> '{libgen,files}' || u.f)
         FROM unnest($1::uuid[], $2::jsonb[]) AS u(id, f)
         WHERE e.id = u.id",
    )
    .bind(&ids)
    .bind(&files)
    .execute(&mut *tx)
    .await?;

    // 3. Новые книги: карточку задаёт первый файл произведения, `metadata.libgen` — все файлы.
    let new_works: Vec<(&String, &Vec<&Book>)> = order
        .iter()
        .filter(|key| !existing.contains_key(*key))
        .map(|key| (key, &works[key]))
        .collect();
    let candidates: Vec<[String; 2]> = new_works
        .iter()
        .map(|(key, books)| slug_candidates("book", &books[0].title, key))
        .collect();
    let slugs = pick_slugs(&mut tx, "entities", &candidates).await?;
    let mut rows: Vec<(String, &Book, Value)> = Vec::new();
    let mut slug_work: HashMap<String, (&Book, usize)> = HashMap::new();
    for ((key, books), slug) in new_works.iter().zip(slugs) {
        let Some(slug) = slug else { continue };
        let files: Vec<Value> = books
            .iter()
            .take(metadata::MAX_LIBGEN_FILES)
            .map(|b| b.file())
            .collect();
        let mut meta = books[0].metadata.clone();
        meta["libgen"] = json!({ "work": key, "files": files });
        slug_work.insert(slug.clone(), (books[0], files.len()));
        rows.push((slug, books[0], meta));
    }
    let created: Vec<(Uuid, String)> = sqlx::query_as(
        "INSERT INTO entities (kind, slug, title, release_date, cover_url, metadata)
         SELECT 'book', * FROM unnest($1::text[], $2::text[], $3::date[], $4::text[], $5::jsonb[])
         ON CONFLICT (slug) DO NOTHING RETURNING id, slug",
    )
    .bind(rows.iter().map(|(s, _, _)| s.as_str()).collect::<Vec<_>>())
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.title.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.release_date)
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.cover_url.as_deref())
            .collect::<Vec<_>>(),
    )
    .bind(rows.iter().map(|(_, _, m)| m.clone()).collect::<Vec<_>>())
    .fetch_all(&mut *tx)
    .await?;
    report.books_created += created.len() as u64;
    let mut new_books: Vec<(Uuid, &Book)> = Vec::with_capacity(created.len());
    for (id, slug) in &created {
        if let Some((book, files)) = slug_work.get(slug) {
            report.files_linked += *files as u64;
            new_books.push((*id, book));
        }
    }

    // 4. Авторы новых книг: человек с тем же именем, уже записанный автором, иначе — новый.
    let mut names: HashMap<String, &str> = HashMap::new();
    for (_, book) in &new_books {
        for author in &book.authors {
            names.entry(name_key(author)).or_insert(author);
        }
    }
    let name_keys: Vec<&String> = names.keys().collect();
    let mut people: HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT DISTINCT ON (replace(lower(p.full_name), 'ё', 'е'))
                replace(lower(p.full_name), 'ё', 'е'), p.id
         FROM people p
         WHERE replace(lower(p.full_name), 'ё', 'е') = ANY($1)
           AND EXISTS (SELECT 1 FROM entity_credits c WHERE c.person_id = p.id AND c.role = 'author')
         ORDER BY replace(lower(p.full_name), 'ё', 'е'), p.created_at",
    )
    .bind(&name_keys)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let missing: Vec<(&String, &str)> = names
        .iter()
        .filter(|(k, _)| !people.contains_key(*k))
        .map(|(k, n)| (k, *n))
        .collect();
    let candidates: Vec<[String; 2]> = missing
        .iter()
        .map(|(key, name)| slug_candidates("author", name, key))
        .collect();
    let slugs = pick_slugs(&mut tx, "people", &candidates).await?;
    let (mut p_slugs, mut p_names, mut slug_key) = (Vec::new(), Vec::new(), HashMap::new());
    for ((key, name), slug) in missing.iter().zip(slugs) {
        let Some(slug) = slug else { continue };
        p_slugs.push(slug.clone());
        p_names.push(name.to_string());
        slug_key.insert(slug, (*key).clone());
    }
    let created_people: Vec<(Uuid, String)> = sqlx::query_as(
        "INSERT INTO people (slug, full_name)
         SELECT * FROM unnest($1::text[], $2::text[])
         ON CONFLICT (slug) DO NOTHING RETURNING id, slug",
    )
    .bind(&p_slugs)
    .bind(&p_names)
    .fetch_all(&mut *tx)
    .await?;
    report.people_created += created_people.len() as u64;
    for (id, slug) in created_people {
        if let Some(key) = slug_key.remove(&slug) {
            people.insert(key, id);
        }
    }

    // 5. Титры: автор, порядок как на обложке.
    let (mut c_entity, mut c_person, mut c_pos) = (Vec::new(), Vec::new(), Vec::new());
    for (entity_id, book) in &new_books {
        let mut added = HashSet::new();
        for author in &book.authors {
            if let Some(person) = people.get(&name_key(author)) {
                if added.insert(*person) {
                    c_entity.push(*entity_id);
                    c_person.push(*person);
                    c_pos.push(added.len() as i32 - 1);
                }
            }
        }
    }
    let credits = sqlx::query(
        "INSERT INTO entity_credits (entity_id, person_id, role, position)
         SELECT e, p, 'author', pos FROM unnest($1::uuid[], $2::uuid[], $3::int[]) AS u(e, p, pos)",
    )
    .bind(&c_entity)
    .bind(&c_person)
    .bind(&c_pos)
    .execute(&mut *tx)
    .await?;
    report.credits_created += credits.rows_affected();

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_languages() {
        for l in ["Russian", "rus", "RU", "Русский", "English, Russian"] {
            assert!(is_russian(l), "{l}");
        }
        for l in ["English", "Belarusian", "", "Russian-English"] {
            assert!(!is_russian(l), "{l}");
        }
    }

    #[test]
    fn authors_are_split_and_surname_name_pairs_merged() {
        assert_eq!(
            split_authors("Толстой, Лев Николаевич"),
            ["Толстой Лев Николаевич"]
        );
        assert_eq!(split_authors("Толстой, Л.Н."), ["Толстой Л.Н."]);
        assert_eq!(split_authors("Ильф, Петров"), ["Ильф", "Петров"]);
        assert_eq!(
            split_authors("Стругацкий А.; Стругацкий Б.; стругацкий  а."),
            ["Стругацкий А.", "Стругацкий Б."]
        );
        assert!(split_authors(" ; , ").is_empty());
    }

    #[test]
    fn rows_map_by_column_names() {
        let columns = Columns::new(UPDATED_COLUMNS).unwrap();
        let mut row: Vec<Option<String>> = vec![Some(String::new()); UPDATED_COLUMNS.len()];
        let set = |row: &mut Vec<Option<String>>, name: &str, value: &str| {
            let i = UPDATED_COLUMNS.iter().position(|c| *c == name).unwrap();
            row[i] = Some(value.into());
        };
        set(&mut row, "ID", "42");
        set(&mut row, "Title", "  Мастер и   Маргарита ");
        set(&mut row, "Author", "Булгаков М.А.");
        set(&mut row, "Year", "[1990]");
        set(&mut row, "Language", "Russian");
        set(&mut row, "Extension", "FB2");
        set(&mut row, "MD5", "0123456789ABCDEF0123456789ABCDEF");
        set(&mut row, "Coverurl", "42000/0123.jpg");
        set(&mut row, "Identifier", "bad,9785170123458");
        set(&mut row, "Pages", "480[490]");
        let book = columns.book(&row).unwrap();
        assert_eq!(book.libgen_id, Some(42));
        assert_eq!(book.md5, "0123456789abcdef0123456789abcdef");
        assert_eq!(book.title, "Мастер и Маргарита");
        assert_eq!(book.authors, ["Булгаков М.А."]);
        assert_eq!(book.release_date, NaiveDate::from_ymd_opt(1990, 1, 1));
        assert_eq!(
            book.cover_url.as_deref(),
            Some("https://libgen.is/covers/42000/0123.jpg")
        );
        assert_eq!(book.extension.as_deref(), Some("fb2"));
        assert_eq!(book.metadata["pages"], 480);

        set(&mut row, "Visible", "ban");
        assert!(columns.book(&row).is_none(), "hidden rows are skipped");
        set(&mut row, "Visible", "");
        set(&mut row, "Language", "English");
        assert!(columns.book(&row).is_none());
    }

    #[test]
    fn invalid_isbn_is_dropped_not_the_book() {
        let m = book_metadata(Some("123,abc"), Some("0"), Some("АСТ"));
        assert_eq!(m, json!({ "publisher": "АСТ" }));
    }
}
