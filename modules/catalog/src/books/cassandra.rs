//! Импорт книг из библиотеки Cassandra (`nexus books cassandra DIR`).
//!
//! Библиотека — каталог настольного приложения: `metadata/*.jsonl` (книга на строку) и `covers/`
//! (обложки AVIF, `<пачка>/<id>.avif`). Импорт пишет:
//! - `entities` (kind = 'book'): название, аннотация → `description`, год → `release_date`,
//!   обложка, `metadata` (серия, язык, ключевые слова, издательство, ISBN, объём, файл),
//!   `external_ids.cassandra` — номер книги в библиотеке;
//! - `people` + `entity_credits`: авторы (`author`) и переводчики (`translator`);
//! - `tags` (kind = 'genre') + `entity_tags`: жанры FB2 по таблице [`super::genres`];
//! - файлы обложек — копией в `BOOK_COVERS_DIR` (раздаёт gateway по `/covers/books/...`).
//!
//! Не переносятся: оглавление, программа, версия и дата документа, город, ID FB2, источник, OCR.
//!
//! Повторный запуск безопасен: книга с известным `external_ids.cassandra` пропускается целиком
//! (её могли править в админке). Пачка — одна транзакция, прерванный импорт продолжается
//! повторным запуском. Своих таблиц нет.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use shared::AppState;
use uuid::Uuid;

use super::{clean, genres, name_key, short_hash, year, BoxError};
use crate::archive::enrich::slugify;
use crate::metadata;
use crate::models::EntityKind;
use crate::validate::{MAX_LONG_TEXT, MAX_TITLE};

const MAX_NAME: usize = 200;
const MAX_AUTHORS: usize = 10;
const MAX_TRANSLATORS: usize = 5;
const MAX_SLUG_BASE: usize = 80;
/// Жанров у книги: в FB2 их бывает десяток, фильтру хватает первых.
const MAX_GENRES: usize = 8;
/// Сколько начала файла читать в поисках размеров картинки (бокс `ispe` — в заголовке AVIF).
const COVER_HEADER: u64 = 64 * 1024;

/// «Авторы», которых нет: человек из них не заводится.
const NOBODY: &[&str] = &[
    "неизвестен автор",
    "автор неизвестен",
    "неизвестный автор",
    "автор неизвестный",
    "unknown author",
    "author unknown",
    "unknown",
];

#[derive(Debug, Clone)]
pub struct Options {
    /// Каталог библиотеки: в нём `metadata/` и `covers/`.
    pub dir: PathBuf,
    pub batch_size: usize,
    /// Не больше стольких книг из файлов (для пробного запуска).
    pub limit: Option<u64>,
}

impl Options {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            batch_size: 1000,
            limit: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    /// Строк в файлах метаданных.
    pub rows: u64,
    /// Строки, которые не разобрались (битый JSON, пустое название).
    pub skipped: u64,
    /// Книги, уже загруженные раньше.
    pub known: u64,
    pub books_created: u64,
    pub people_created: u64,
    pub credits_created: u64,
    /// Связей «книга — жанр».
    pub genres_linked: u64,
    pub covers_copied: u64,
    /// У книги записана обложка, а файла нет.
    pub covers_missing: u64,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "metadata rows:   {}", self.rows)?;
        writeln!(f, "skipped rows:    {}", self.skipped)?;
        writeln!(f, "already known:   {}", self.known)?;
        writeln!(f, "books created:   {}", self.books_created)?;
        writeln!(f, "people created:  {}", self.people_created)?;
        writeln!(f, "credits:         {}", self.credits_created)?;
        writeln!(f, "genre links:     {}", self.genres_linked)?;
        writeln!(f, "covers copied:   {}", self.covers_copied)?;
        writeln!(f, "covers missing:  {}", self.covers_missing)
    }
}

/// Строка `metadata/*.jsonl`. Лишние поля (оглавление, программа, ID FB2, …) не читаются.
#[derive(Debug, Default, Deserialize)]
struct Row {
    id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    genre_codes: Vec<String>,
    #[serde(default)]
    archive: String,
    #[serde(default)]
    internal_path: String,
    #[serde(default)]
    cover: Option<String>,
    #[serde(default)]
    details: Details,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    words: u64,
}

#[derive(Debug, Default, Deserialize)]
struct Details {
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    annotation: String,
    #[serde(default)]
    keywords: String,
    #[serde(default)]
    lang: String,
    #[serde(default)]
    src_lang: String,
    #[serde(default)]
    translators: Vec<String>,
    #[serde(default)]
    series: String,
    #[serde(default)]
    series_number: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    publisher: String,
    #[serde(default)]
    year: String,
    #[serde(default)]
    isbn: String,
}

/// Книга, готовая к записи.
#[derive(Debug, Clone, PartialEq)]
pub struct Book {
    pub cassandra_id: i64,
    pub title: String,
    pub description: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub authors: Vec<String>,
    pub translators: Vec<String>,
    /// Названия жанров ([`genres::names`]).
    pub genres: Vec<&'static str>,
    /// Путь обложки внутри `covers/`: `<пачка>/<id>.avif`.
    pub cover: Option<String>,
    /// Провалидированный `metadata` книги.
    pub metadata: Value,
}

impl Row {
    fn book(self) -> Option<Book> {
        let title = clean(&self.title, MAX_TITLE)?;
        let d = self.details;
        let authors = if d.authors.is_empty() {
            // Старые записи без `details.authors`: авторы одной строкой через запятую.
            people(self.author.split(','), MAX_AUTHORS)
        } else {
            people(d.authors.iter().map(String::as_str), MAX_AUTHORS)
        };
        let translators = people(d.translators.iter().map(String::as_str), MAX_TRANSLATORS);
        let mut genres = genres::names(&self.genre_codes);
        genres.truncate(MAX_GENRES);
        let description = Some(d.annotation.trim())
            .filter(|text| !text.is_empty())
            .map(|text| match text.char_indices().nth(MAX_LONG_TEXT) {
                Some((i, _)) => text[..i].trim_end().to_string(),
                None => text.to_string(),
            });

        let mut meta = serde_json::Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                meta.insert(key.into(), value);
            }
        };
        put("publisher", clean(&d.publisher, 200).map(Value::from));
        put(
            "series",
            clean(&d.series, metadata::MAX_SERIES).map(Value::from),
        );
        put(
            "series_number",
            clean(&d.series_number, 20).map(Value::from),
        );
        put("language", language(&d.lang).map(Value::from));
        put("original_language", language(&d.src_lang).map(Value::from));
        put("written", clean(&d.date, 50).map(Value::from));
        let keywords = keywords(&d.keywords);
        put("keywords", (!keywords.is_empty()).then(|| json!(keywords)));
        put("words", (self.words > 0).then(|| json!(self.words)));
        if let (Some(archive), Some(path)) =
            (clean(&self.archive, 200), clean(&self.internal_path, 300))
        {
            let mut file = json!({ "archive": archive, "path": path });
            if self.size > 0 {
                file["size"] = json!(self.size);
            }
            put("file", Some(file));
        }
        // ISBN проверяется контрольной цифрой: в библиотеке хватает опечаток. Невалидный
        // отбрасывается, книга остаётся.
        let base = Value::Object(meta);
        let mut metadata = None;
        for isbn in d.isbn.split([',', ';', ' ']) {
            let isbn = isbn.trim();
            if isbn.is_empty() {
                continue;
            }
            let mut candidate = base.clone();
            candidate["isbn"] = json!(isbn);
            if let Ok(valid) = metadata::validate(EntityKind::Book, candidate) {
                metadata = Some(valid);
                break;
            }
        }
        let metadata = match metadata {
            Some(valid) => valid,
            None => metadata::validate(EntityKind::Book, base).unwrap_or_else(|_| json!({})),
        };

        Some(Book {
            cassandra_id: self.id,
            title,
            description,
            release_date: year(&d.year).or_else(|| year(&d.date)),
            authors,
            translators,
            genres,
            cover: self.cover.filter(|path| safe_cover_path(path)),
            metadata,
        })
    }
}

/// Имена людей: очищенные, без повторов и «неизвестных авторов», не больше `max`.
fn people<'a>(names: impl Iterator<Item = &'a str>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let Some(name) = clean(name, MAX_NAME) else {
            continue;
        };
        let key = name_key(&name);
        if name.chars().filter(|c| c.is_alphabetic()).count() < 2
            || NOBODY.contains(&key.as_str())
            || out.iter().any(|known| name_key(known) == key)
        {
            continue;
        }
        out.push(name);
        if out.len() == max {
            break;
        }
    }
    out
}

/// Код языка: `RU`, `rus`, `ru-RU` → `ru`; длиннее 20 символов или с пробелами — мусор.
fn language(value: &str) -> Option<String> {
    let value = value.trim().to_lowercase();
    let code = value.split(['-', '_']).next().unwrap_or_default();
    let code = match code {
        "rus" | "russian" => "ru",
        "eng" | "english" => "en",
        other => other,
    };
    let ok = (2..=20).contains(&code.chars().count()) && code.chars().all(char::is_alphabetic);
    ok.then(|| code.to_string())
}

/// Ключевые слова из строки через запятую: без повторов, в пределах `metadata`.
fn keywords(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in value.split([',', ';']) {
        let Some(word) = clean(word, metadata::MAX_KEYWORD) else {
            continue;
        };
        if !out.iter().any(|known| name_key(known) == name_key(&word)) {
            out.push(word);
        }
        if out.len() == metadata::MAX_KEYWORDS {
            break;
        }
    }
    out
}

/// Путь обложки — только `<каталог>/<файл>` из букв, цифр, `-`, `_`, `.`: он уходит в URL и в
/// путь на диске.
fn safe_cover_path(path: &str) -> bool {
    let mut parts = path.split('/');
    let ok = |part: Option<&str>| {
        part.is_some_and(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Размеры AVIF из бокса `ispe` (ширина и высота, по 4 байта после версии и флагов). В файле
/// их может быть несколько (миниатюра, альфа-канал) — берётся самая большая картинка.
pub fn avif_size(header: &[u8]) -> Option<(i32, i32)> {
    let mut best: Option<(i32, i32)> = None;
    let mut at = 0;
    while let Some(found) = header[at..].windows(4).position(|w| w == b"ispe") {
        let start = at + found + 8;
        at += found + 4;
        let Some(bytes) = header.get(start..start + 8) else {
            break;
        };
        let width = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let height = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let max = crate::validate::MAX_COVER_PX as u32;
        if (1..=max).contains(&width) && (1..=max).contains(&height) {
            let size = (width as i32, height as i32);
            if best.is_none_or(|(w, h)| {
                i64::from(w) * i64::from(h) < i64::from(size.0) * i64::from(size.1)
            }) {
                best = Some(size);
            }
        }
    }
    best
}

/// Обложка книги после копирования: адрес и размеры.
#[derive(Debug, Clone, PartialEq)]
struct Cover {
    url: String,
    size: Option<(i32, i32)>,
}

/// Копирует обложку в `target` (если её там ещё нет) и читает размеры. `None` — файла нет.
#[allow(clippy::type_complexity)]
fn copy_cover(source: &Path, target: &Path) -> std::io::Result<Option<(Option<(i32, i32)>, bool)>> {
    let Ok(meta) = std::fs::metadata(source) else {
        return Ok(None);
    };
    let mut header = Vec::new();
    File::open(source)?
        .take(COVER_HEADER)
        .read_to_end(&mut header)?;
    let size = avif_size(&header);
    let same = std::fs::metadata(target).is_ok_and(|t| t.len() == meta.len());
    if !same {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Через временный файл: gateway не отдаст недописанную картинку.
        let tmp = target.with_extension("tmp");
        std::fs::copy(source, &tmp)?;
        std::fs::rename(&tmp, target)?;
    }
    Ok(Some((size, !same)))
}

/// Каталог обложек библиотеки: `covers/`, в старых библиотеках — `convers/`.
fn covers_source(dir: &Path) -> PathBuf {
    let covers = dir.join("covers");
    if covers.is_dir() {
        covers
    } else {
        dir.join("convers")
    }
}

/// Файлы метаданных по порядку имён.
fn metadata_files(dir: &Path) -> Result<Vec<PathBuf>, BoxError> {
    let folder = dir.join("metadata");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)
        .map_err(|e| format!("{}: {e}", folder.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("{}: no *.jsonl files", folder.display()).into());
    }
    Ok(files)
}

/// Импорт библиотеки. Файлы читаются в отдельном потоке, запись — пачками по `batch_size`.
pub async fn import(state: &AppState, options: &Options) -> Result<Report, BoxError> {
    let files = metadata_files(&options.dir)?;
    let covers_from = covers_source(&options.dir);
    let covers_to = state.config.book_covers_dir.clone().map(PathBuf::from);
    if covers_to.is_none() {
        tracing::warn!("BOOK_COVERS_DIR is not set: books are imported without covers");
    }
    let genre_tags = ensure_genres(state).await?;
    // Книги в архиве книг (`books::archive`) обратно не импортируются.
    let archived = super::archive::archived_cassandra_ids(state)?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u64, u64, Vec<Book>), String>>(2);
    let batch_size = options.batch_size.max(1);
    let limit = options.limit;
    let reader = tokio::task::spawn_blocking(move || {
        let send = |message| tx.blocking_send(message).is_ok();
        let mut batch = Vec::with_capacity(batch_size);
        let (mut rows, mut skipped, mut taken) = (0u64, 0u64, 0u64);
        'files: for path in files {
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(e) => {
                    send(Err(format!("{}: {e}", path.display())));
                    return;
                }
            };
            for line in BufReader::with_capacity(1 << 20, file).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        send(Err(format!("{}: {e}", path.display())));
                        return;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                if limit.is_some_and(|l| taken >= l) {
                    break 'files;
                }
                rows += 1;
                // Битая строка — потеряна одна книга, а не файл.
                match serde_json::from_str::<Row>(&line).ok().and_then(Row::book) {
                    Some(book) => {
                        taken += 1;
                        batch.push(book);
                    }
                    None => skipped += 1,
                }
                if batch.len() >= batch_size
                    && !send(Ok((rows, skipped, std::mem::take(&mut batch))))
                {
                    return;
                }
            }
        }
        send(Ok((rows, skipped, batch)));
    });

    let mut report = Report::default();
    let mut people: HashMap<String, Uuid> = HashMap::new();
    while let Some(message) = rx.recv().await {
        let (rows, skipped, batch) = message?;
        report.rows = rows;
        report.skipped = skipped;
        write_batch(
            state,
            batch,
            &genre_tags,
            &archived,
            &mut people,
            &covers_from,
            covers_to.as_deref(),
            &mut report,
        )
        .await?;
        tracing::info!(
            rows = report.rows,
            books = report.books_created,
            known = report.known,
            people = report.people_created,
            covers = report.covers_copied,
            "cassandra import progress"
        );
    }
    reader.await?;
    Ok(report)
}

/// Теги-жанры для всех названий таблицы [`genres::GENRES`]: существующий тег с тем же названием
/// (жанр предпочтительнее обычного тега) или новый. Один раз на запуск.
async fn ensure_genres(state: &AppState) -> Result<HashMap<&'static str, Uuid>, BoxError> {
    let mut names: Vec<&'static str> = Vec::new();
    for (_, name) in genres::GENRES {
        if !names.contains(name) {
            names.push(name);
        }
    }
    let mut tx = state.db.begin().await?;
    let rows: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT ON (replace(lower(name), 'ё', 'е')) replace(lower(name), 'ё', 'е'), id
         FROM tags ORDER BY replace(lower(name), 'ё', 'е'), (kind = 'genre') DESC, entities_count DESC",
    )
    .fetch_all(&mut *tx)
    .await?;
    let known: HashMap<String, Uuid> = rows.into_iter().collect();

    let mut tags: HashMap<&'static str, Uuid> = HashMap::new();
    let mut missing: Vec<&'static str> = Vec::new();
    for name in names {
        match known.get(&name_key(name)) {
            Some(id) => {
                tags.insert(name, *id);
            }
            None => missing.push(name),
        }
    }
    let candidates: Vec<Vec<String>> = missing
        .iter()
        .map(|name| {
            let base = slugify(name);
            if base.is_empty() {
                vec![format!("genre-{}", short_hash(name))]
            } else {
                vec![base.clone(), format!("{base}-{}", short_hash(name))]
            }
        })
        .collect();
    let slugs = pick_slugs(&mut tx, "tags", &candidates).await?;
    let (mut t_slugs, mut t_names) = (Vec::new(), Vec::new());
    for (name, slug) in missing.iter().zip(slugs) {
        if let Some(slug) = slug {
            t_slugs.push(slug);
            t_names.push(*name);
        }
    }
    let created: Vec<(Uuid, String)> = sqlx::query_as(
        "INSERT INTO tags (slug, name, kind)
         SELECT s, n, 'genre' FROM unnest($1::text[], $2::text[]) AS u(s, n)
         ON CONFLICT (slug) DO NOTHING RETURNING id, name",
    )
    .bind(&t_slugs)
    .bind(&t_names)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (id, name) in created {
        if let Some(known) = missing.iter().find(|n| **n == name) {
            tags.insert(known, id);
        }
    }
    Ok(tags)
}

/// Свободные slug'и из кандидатов: занятые в `table` и уже выданные в этой пачке пропускаются.
async fn pick_slugs(
    tx: &mut sqlx::PgConnection,
    table: &str,
    candidates: &[Vec<String>],
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

/// Обрезка slug'а до `max` символов по границе слова.
fn slug_base(text: &str, max: usize) -> String {
    let mut base = slugify(text);
    if let Some((i, _)) = base.char_indices().nth(max) {
        base = base[..i].trim_end_matches('-').to_string();
    }
    base
}

/// Кандидаты slug'а книги: `sobache-serdtse`, `sobache-serdtse-bulgakov` (у одного названия
/// много изданий и однофамильцев-произведений), затем с хешем номера книги — он не повторяется.
fn book_slugs(book: &Book) -> Vec<String> {
    let base = slug_base(&book.title, MAX_SLUG_BASE);
    let hash = short_hash(&format!("cassandra:{}", book.cassandra_id));
    if base.is_empty() {
        return vec![format!("book-{hash}")];
    }
    let mut out = vec![base.clone()];
    // Фамилия — последнее слово имени автора.
    let surname = book
        .authors
        .first()
        .and_then(|author| author.split(' ').next_back())
        .map(|word| slug_base(word, 15))
        .filter(|s| !s.is_empty());
    if let Some(surname) = surname {
        out.push(format!("{base}-{surname}"));
    }
    out.push(format!("{base}-{hash}"));
    out
}

fn person_slugs(name: &str, key: &str) -> Vec<String> {
    let base = slug_base(name, MAX_SLUG_BASE);
    let hash = short_hash(key);
    if base.is_empty() {
        vec![format!("author-{hash}")]
    } else {
        vec![base.clone(), format!("{base}-{hash}")]
    }
}

/// Пачка: новые книги, их люди, титры, жанры и обложки. Запись в БД — одной транзакцией.
#[allow(clippy::too_many_arguments)]
async fn write_batch(
    state: &AppState,
    batch: Vec<Book>,
    genre_tags: &HashMap<&'static str, Uuid>,
    archived: &HashSet<i64>,
    people: &mut HashMap<String, Uuid>,
    covers_from: &Path,
    covers_to: Option<&Path>,
    report: &mut Report,
) -> Result<(), BoxError> {
    if batch.is_empty() {
        return Ok(());
    }
    // 1. Уже загруженные книги пропускаем; повтор номера внутри пачки — тоже.
    let ids: Vec<i64> = batch.iter().map(|b| b.cassandra_id).collect();
    let known: HashSet<i64> = sqlx::query_scalar(
        "SELECT (external_ids ->> 'cassandra')::bigint FROM entities
         WHERE external_ids ? 'cassandra' AND (external_ids ->> 'cassandra')::bigint = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();
    let mut seen = HashSet::new();
    let total = batch.len();
    let books: Vec<Book> = batch
        .into_iter()
        .filter(|b| {
            !known.contains(&b.cassandra_id)
                && !archived.contains(&b.cassandra_id)
                && seen.insert(b.cassandra_id)
        })
        .collect();
    report.known += (total - books.len()) as u64;
    if books.is_empty() {
        return Ok(());
    }

    // 2. Обложки — файлами на диск до транзакции: копия идемпотентна, а недописанная пачка
    //    при повторе просто найдёт файлы на месте.
    let covers: Vec<Option<Cover>> = match covers_to {
        None => vec![None; books.len()],
        Some(covers_to) => {
            let paths: Vec<Option<String>> = books.iter().map(|b| b.cover.clone()).collect();
            let (from, to) = (covers_from.to_path_buf(), covers_to.to_path_buf());
            let public_url = state.config.public_url.trim_end_matches('/').to_string();
            let (covers, copied, missing) = tokio::task::spawn_blocking(move || {
                let (mut copied, mut missing) = (0u64, 0u64);
                let mut out = Vec::with_capacity(paths.len());
                for path in paths {
                    let Some(path) = path else {
                        out.push(None);
                        continue;
                    };
                    match copy_cover(&from.join(&path), &to.join(&path))? {
                        Some((size, was_copied)) => {
                            copied += u64::from(was_copied);
                            out.push(Some(Cover {
                                url: format!("{public_url}/covers/books/{path}"),
                                size,
                            }));
                        }
                        None => {
                            missing += 1;
                            out.push(None);
                        }
                    }
                }
                Ok::<_, std::io::Error>((out, copied, missing))
            })
            .await??;
            report.covers_copied += copied;
            report.covers_missing += missing;
            covers
        }
    };

    let mut tx = state.db.begin().await?;

    // 3. Книги.
    let candidates: Vec<Vec<String>> = books.iter().map(book_slugs).collect();
    let slugs = pick_slugs(&mut tx, "entities", &candidates).await?;
    let mut rows: Vec<(String, &Book, Option<&Cover>)> = Vec::with_capacity(books.len());
    for ((book, slug), cover) in books.iter().zip(slugs).zip(&covers) {
        if let Some(slug) = slug {
            rows.push((slug, book, cover.as_ref()));
        }
    }
    let created: Vec<(Uuid, i64)> = sqlx::query_as(
        "INSERT INTO entities (kind, slug, title, description, release_date, cover_url,
                               cover_width, cover_height, metadata, external_ids)
         SELECT 'book', u.slug, u.title, u.description, u.release_date, u.cover_url,
                u.cover_width, u.cover_height, u.metadata,
                jsonb_build_object('cassandra', u.cassandra_id)
         FROM unnest($1::text[], $2::text[], $3::text[], $4::date[], $5::text[], $6::int[],
                     $7::int[], $8::jsonb[], $9::bigint[])
              AS u(slug, title, description, release_date, cover_url, cover_width, cover_height,
                   metadata, cassandra_id)
         ON CONFLICT DO NOTHING
         RETURNING id, (external_ids ->> 'cassandra')::bigint",
    )
    .bind(rows.iter().map(|(s, _, _)| s.as_str()).collect::<Vec<_>>())
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.title.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.description.as_deref())
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.release_date)
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, _, c)| c.map(|c| c.url.as_str()))
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, _, c)| c.and_then(|c| c.size).map(|s| s.0))
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, _, c)| c.and_then(|c| c.size).map(|s| s.1))
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.metadata.clone())
            .collect::<Vec<_>>(),
    )
    .bind(
        rows.iter()
            .map(|(_, b, _)| b.cassandra_id)
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *tx)
    .await?;
    report.books_created += created.len() as u64;
    let by_id: HashMap<i64, &Book> = books.iter().map(|b| (b.cassandra_id, b)).collect();
    let new_books: Vec<(Uuid, &Book)> = created
        .iter()
        .filter_map(|(id, cassandra_id)| Some((*id, *by_id.get(cassandra_id)?)))
        .collect();

    // 4. Люди: человек с тем же именем, уже записанный автором или переводчиком книги, иначе —
    //    новый. Найденные запоминаются на весь запуск (`people`).
    let mut names: HashMap<String, &str> = HashMap::new();
    for (_, book) in &new_books {
        for name in book.authors.iter().chain(&book.translators) {
            let key = name_key(name);
            if !people.contains_key(&key) {
                names.entry(key).or_insert(name);
            }
        }
    }
    let name_keys: Vec<&String> = names.keys().collect();
    let found: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT ON (replace(lower(p.full_name), 'ё', 'е'))
                replace(lower(p.full_name), 'ё', 'е'), p.id
         FROM people p
         WHERE replace(lower(p.full_name), 'ё', 'е') = ANY($1)
           AND EXISTS (SELECT 1 FROM entity_credits c
                       WHERE c.person_id = p.id AND c.role IN ('author', 'translator'))
         ORDER BY replace(lower(p.full_name), 'ё', 'е'), p.created_at",
    )
    .bind(&name_keys)
    .fetch_all(&mut *tx)
    .await?;
    // До коммита новые люди — в отдельной карте: откат транзакции не должен оставить в
    // `people` id, которых в БД нет.
    let mut fresh: HashMap<String, Uuid> = found.into_iter().collect();
    let missing: Vec<(&String, &str)> = names
        .iter()
        .filter(|(key, _)| !fresh.contains_key(*key))
        .map(|(key, name)| (key, *name))
        .collect();
    let candidates: Vec<Vec<String>> = missing
        .iter()
        .map(|(key, name)| person_slugs(name, key))
        .collect();
    let slugs = pick_slugs(&mut tx, "people", &candidates).await?;
    let (mut p_slugs, mut p_names, mut slug_key) = (Vec::new(), Vec::new(), HashMap::new());
    for ((key, name), slug) in missing.iter().zip(slugs) {
        let Some(slug) = slug else { continue };
        p_slugs.push(slug.clone());
        p_names.push((*name).to_string());
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
    let people_created = created_people.len() as u64;
    for (id, slug) in created_people {
        if let Some(key) = slug_key.remove(&slug) {
            fresh.insert(key, id);
        }
    }

    // 5. Титры: авторы, затем переводчики, порядок как в книге.
    let (mut c_entity, mut c_person, mut c_role, mut c_pos) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (entity_id, book) in &new_books {
        let mut position = 0i32;
        for (role, list) in [("author", &book.authors), ("translator", &book.translators)] {
            let mut added = HashSet::new();
            for name in list {
                let key = name_key(name);
                let Some(person) = fresh.get(&key).or_else(|| people.get(&key)) else {
                    continue;
                };
                if added.insert(*person) {
                    c_entity.push(*entity_id);
                    c_person.push(*person);
                    c_role.push(role);
                    c_pos.push(position);
                    position += 1;
                }
            }
        }
    }
    let credits = sqlx::query(
        "INSERT INTO entity_credits (entity_id, person_id, role, position)
         SELECT e, p, r, pos FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::int[])
              AS u(e, p, r, pos)
         ON CONFLICT DO NOTHING",
    )
    .bind(&c_entity)
    .bind(&c_person)
    .bind(&c_role)
    .bind(&c_pos)
    .execute(&mut *tx)
    .await?;

    // 6. Жанры.
    let (mut t_entity, mut t_tag) = (Vec::new(), Vec::new());
    for (entity_id, book) in &new_books {
        let mut added = HashSet::new();
        for genre in &book.genres {
            if let Some(tag) = genre_tags.get(genre) {
                if added.insert(*tag) {
                    t_entity.push(*entity_id);
                    t_tag.push(*tag);
                }
            }
        }
    }
    let genres_linked = sqlx::query(
        "INSERT INTO entity_tags (entity_id, tag_id)
         SELECT * FROM unnest($1::uuid[], $2::uuid[]) ON CONFLICT DO NOTHING",
    )
    .bind(&t_entity)
    .bind(&t_tag)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    report.people_created += people_created;
    report.credits_created += credits.rows_affected();
    report.genres_linked += genres_linked.rows_affected();
    people.extend(fresh);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(json: Value) -> Book {
        serde_json::from_value::<Row>(json).unwrap().book().unwrap()
    }

    #[test]
    fn row_maps_to_book_and_drops_document_fields() {
        let book = row(json!({
            "id": 113037,
            "title": "  Абарат: Первая  книга часов ",
            "author": "Клайв Баркер",
            "genre_codes": ["child_sf", "my_junk", "sf_fantasy"],
            "archive": "104215-113436.arx",
            "internal_path": "113037.epub",
            "cover": "104215-113436/113037.avif",
            "details": {
                "authors": ["Клайв Баркер", "клайв  баркер", "неизвестен Автор"],
                "annotation": " Здесь обыденное переплетается с невиданным. ",
                "keywords": "магия, Острова; магия",
                "lang": "RU", "src_lang": "en-US",
                "translators": ["Л. Бочарова"],
                "series": "Абарат", "series_number": "1",
                "date": "2002", "publisher": "Изд-во «Домино»", "city": "СПб",
                "year": "2004", "isbn": "5-699-07427-9",
                "doc_id": "6DDB1434", "doc_version": "1.1", "doc_date": "2005-03-11",
                "program_used": "FB Tools", "src_url": "", "src_ocr": "Библиотека",
                "chapters": ["ПРОЛОГ"]
            },
            "size": 3085912, "words": 105136
        }));
        assert_eq!(book.cassandra_id, 113037);
        assert_eq!(book.title, "Абарат: Первая книга часов");
        assert_eq!(
            book.description.as_deref(),
            Some("Здесь обыденное переплетается с невиданным.")
        );
        assert_eq!(book.release_date, NaiveDate::from_ymd_opt(2004, 1, 1));
        assert_eq!(book.authors, ["Клайв Баркер"]);
        assert_eq!(book.translators, ["Л. Бочарова"]);
        assert_eq!(book.genres, ["Детская фантастика", "Фэнтези"]);
        assert_eq!(book.cover.as_deref(), Some("104215-113436/113037.avif"));
        assert_eq!(
            book.metadata,
            json!({
                "isbn": "5699074279",
                "publisher": "Изд-во «Домино»",
                "series": "Абарат", "series_number": "1",
                "language": "ru", "original_language": "en",
                "keywords": ["магия", "Острова"],
                "written": "2002",
                "words": 105136,
                "file": { "archive": "104215-113436.arx", "path": "113037.epub", "size": 3085912 }
            })
        );
        for dropped in ["chapters", "doc_id", "city", "program_used", "src_ocr"] {
            assert!(book.metadata.get(dropped).is_none(), "{dropped}");
        }
    }

    #[test]
    fn bad_isbn_and_missing_fields_do_not_lose_the_book() {
        let book = row(json!({ "id": 1, "title": "Без всего", "details": { "isbn": "123" } }));
        assert_eq!(book.metadata, json!({}));
        assert!(book.authors.is_empty() && book.cover.is_none() && book.description.is_none());
        assert!(
            serde_json::from_value::<Row>(json!({ "id": 2, "title": "  " }))
                .unwrap()
                .book()
                .is_none()
        );
    }

    #[test]
    fn year_falls_back_to_written_date() {
        let book = row(json!({ "id": 1, "title": "Т", "details": { "date": "1869" } }));
        assert_eq!(book.release_date, NaiveDate::from_ymd_opt(1869, 1, 1));
    }

    #[test]
    fn cover_path_must_be_a_plain_folder_and_file() {
        assert!(safe_cover_path("104215-113436/113037.avif"));
        for bad in [
            "../x.avif",
            "a/../b.avif",
            "a/b/c.avif",
            "x.avif",
            "a/.hidden",
            "a/b c.avif",
        ] {
            assert!(!safe_cover_path(bad), "{bad}");
        }
    }

    #[test]
    fn slugs_go_from_title_to_surname_to_hash() {
        let mut book = row(json!({
            "id": 7, "title": "Собачье сердце",
            "details": { "authors": ["Михаил Афанасьевич Булгаков"] }
        }));
        let slugs = book_slugs(&book);
        assert_eq!(slugs[0], "sobache-serdtse");
        assert_eq!(slugs[1], "sobache-serdtse-bulgakov");
        assert!(
            slugs[2].starts_with("sobache-serdtse-")
                && slugs[2].len() == "sobache-serdtse-".len() + 8
        );
        book.title = "日本".into();
        assert!(book_slugs(&book)[0].starts_with("book-"));
    }

    #[test]
    fn avif_size_reads_the_largest_ispe_box() {
        let mut header = vec![0u8; 32];
        for (w, h) in [(160u32, 240u32), (600, 900)] {
            header.extend_from_slice(b"ispe");
            header.extend_from_slice(&[0, 0, 0, 0]);
            header.extend_from_slice(&w.to_be_bytes());
            header.extend_from_slice(&h.to_be_bytes());
            header.extend_from_slice(&[0; 12]);
        }
        assert_eq!(avif_size(&header), Some((600, 900)));
        assert_eq!(avif_size(b"no boxes here"), None);
        assert_eq!(avif_size(b"....ispe\0\0"), None, "truncated box");
    }

    #[test]
    fn languages_are_normalized() {
        assert_eq!(language("RU").as_deref(), Some("ru"));
        assert_eq!(language("rus").as_deref(), Some("ru"));
        assert_eq!(language("ru-RU").as_deref(), Some("ru"));
        assert_eq!(language("eng").as_deref(), Some("en"));
        assert_eq!(language(""), None);
        assert_eq!(language("русский язык"), None);
    }
}
