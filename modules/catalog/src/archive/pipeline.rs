//! Конвейер горячего слоя и архива: `nexus archive dump` и `nexus archive build`.
//!
//! **dump** — TSV со всеми фильмами, которые делит конвейер: строки `entities`, связанные с TMDB и
//! не `active_manual`, плюс документы архива. Колонки: `id` (slug), `title`, `orig_title`, `type`,
//! `popularity_score` (TMDB popularity), `has_ru_locale` (в названии есть кириллица), `tmdb_id`,
//! `release_date`, `cover_url` (постер; для архива его один раз дозагружает
//! `scripts/catalog-import/archive_covers.py`).
//!
//! **build** — два прохода по дампу, память не зависит от его размера:
//! 1. Top-K: две min-кучи (`BinaryHeap<Reverse<..>>`) — `ru` лучших по popularity с русским
//!    названием (по умолчанию все) и `foreign` без него (500); в памяти только они.
//! 2. Маршрутизация: строка из топа — в горячий слой (PostgreSQL, пачки по [`BATCH`]:
//!    `active = true` или вставка), остальные — в архив (Tantivy, писатель с бюджетом 50 МБ, коммит
//!    каждые [`COMMIT_EVERY`] документов). Строки PostgreSQL, ушедшие в архив, удаляются **после**
//!    коммита архива, пачками, вместе с людьми, у которых не осталось работ.
//!
//! `fresh` — архив пересоздаётся пустым в текущей схеме ([`Archive::recreate`]).
//!
//! `defer_delete` (первичная раскладка): строки не удаляются, только `archived_tmdb`; удаление —
//! `scripts/archive-purge.sql`.
//!
//! Не трогаются: `active_manual` (сиды, ручные правки), сущности без TMDB (их нет в дампе) и
//! сущности с пользовательскими данными (`InterestDirectory::entities_in_use`).

use super::index::{Archive, ArchiveDoc, ArchiveError};
use crate::models::EntityKind;
use crate::validate;
use chrono::NaiveDate;
use shared::AppState;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::IndexWriter;
use tokio::sync::mpsc;
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Пачка записи в PostgreSQL и чтения дампа.
pub const BATCH: usize = 1_000;
/// Коммит архива каждые столько документов.
pub const COMMIT_EVERY: u64 = 100_000;
/// Горячий слой по умолчанию (решение 2026-10-14): **все** фильмы с русским названием и 500 самых
/// популярных иностранных. Прежняя раскладка — `--ru 90000 --foreign 10000`.
pub const DEFAULT_RU: usize = usize::MAX;
pub const DEFAULT_FOREIGN: usize = 500;

const HEADER: [&str; 9] = [
    "id",
    "title",
    "orig_title",
    "type",
    "popularity_score",
    "has_ru_locale",
    "tmdb_id",
    "release_date",
    "cover_url",
];

/// Строка дампа.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    slug: String,
    title: String,
    original_title: Option<String>,
    kind: EntityKind,
    popularity: f64,
    ru: bool,
    tmdb_id: Option<i32>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
}

impl Row {
    fn doc(&self) -> ArchiveDoc {
        ArchiveDoc {
            slug: self.slug.clone(),
            title: self.title.clone(),
            original_title: self.original_title.clone(),
            kind: self.kind,
            tmdb_id: self.tmdb_id,
            release_date: self.release_date,
            popularity: self.popularity,
            cover_url: self.cover_url.clone(),
        }
    }
}

/// Есть русское название: в `title` есть кириллица.
fn has_ru_locale(title: &str) -> bool {
    title
        .chars()
        .any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё'))
}

/// Табуляции и переводы строк внутри полей сломали бы TSV без кавычек.
fn clean(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

// ---------------------------------------------------------------- dump

#[derive(Debug, Default)]
pub struct DumpReport {
    pub from_db: u64,
    pub from_archive: u64,
}

impl fmt::Display for DumpReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "dump: {} rows ({} from PostgreSQL, {} from the archive)",
            self.from_db + self.from_archive,
            self.from_db,
            self.from_archive
        )
    }
}

#[derive(sqlx::FromRow)]
struct DbRow {
    slug: String,
    title: String,
    original_title: Option<String>,
    kind: EntityKind,
    popularity: f64,
    tmdb_id: i32,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
}

fn write_row(out: &mut csv::Writer<BufWriter<File>>, row: &Row) -> csv::Result<()> {
    out.write_record([
        clean(&row.slug),
        clean(&row.title),
        row.original_title.as_deref().map(clean).unwrap_or_default(),
        row.kind.as_str().to_string(),
        row.popularity.to_string(),
        if row.ru { "true" } else { "false" }.to_string(),
        row.tmdb_id.map(|id| id.to_string()).unwrap_or_default(),
        row.release_date.map(|d| d.to_string()).unwrap_or_default(),
        row.cover_url.as_deref().map(clean).unwrap_or_default(),
    ])
}

/// Выгружает дамп в `path`. Строки БД идут потоком (курсор), пишет их отдельный поток через
/// канал на несколько пачек: память не зависит от размера каталога.
pub async fn dump(
    state: &AppState,
    archive: Option<Arc<Archive>>,
    path: &Path,
) -> Result<DumpReport, BoxError> {
    use futures_util::TryStreamExt;

    let (tx, mut rx) = mpsc::channel::<Vec<Row>>(4);
    let path = path.to_path_buf();
    let writer = tokio::task::spawn_blocking(move || -> Result<DumpReport, BoxError> {
        let mut out = csv::WriterBuilder::new()
            .delimiter(b'\t')
            .quote_style(csv::QuoteStyle::Never)
            .from_writer(BufWriter::new(File::create(&path)?));
        out.write_record(HEADER)?;
        let mut report = DumpReport::default();
        while let Some(rows) = rx.blocking_recv() {
            for row in &rows {
                write_row(&mut out, row)?;
            }
            report.from_db += rows.len() as u64;
        }
        if let Some(archive) = archive {
            report.from_archive = archive.for_each(|doc| {
                let ru = has_ru_locale(&doc.title);
                write_row(
                    &mut out,
                    &Row {
                        slug: doc.slug,
                        title: doc.title,
                        original_title: doc.original_title,
                        kind: doc.kind,
                        popularity: doc.popularity,
                        ru,
                        tmdb_id: doc.tmdb_id,
                        release_date: doc.release_date,
                        cover_url: doc.cover_url,
                    },
                )?;
                Ok(())
            })?;
        }
        out.flush()?;
        Ok(report)
    });

    let mut rows = sqlx::query_as::<_, DbRow>(
        "SELECT e.slug, e.title, e.original_title, e.kind,
                COALESCE(s.tmdb_popularity, 0) AS popularity, t.tmdb_id, e.release_date,
                e.cover_url
         FROM entities e
         JOIN entity_tmdb t ON t.entity_id = e.id
         LEFT JOIN entity_stats s ON s.entity_id = e.id
         WHERE NOT e.active_manual",
    )
    .fetch(&state.db);
    let mut batch = Vec::with_capacity(BATCH);
    let mut send_result = Ok(());
    while let Some(row) = rows.try_next().await? {
        batch.push(Row {
            ru: has_ru_locale(&row.title),
            slug: row.slug,
            title: row.title,
            original_title: row.original_title,
            kind: row.kind,
            popularity: row.popularity,
            tmdb_id: Some(row.tmdb_id),
            release_date: row.release_date,
            cover_url: row.cover_url,
        });
        if batch.len() == BATCH {
            // Писатель упал — его ошибку вернёт join ниже.
            send_result = tx.send(std::mem::take(&mut batch)).await;
            if send_result.is_err() {
                break;
            }
        }
    }
    if send_result.is_ok() && !batch.is_empty() {
        let _ = tx.send(batch).await;
    }
    drop(tx);
    writer.await?
}

// ---------------------------------------------------------------- чтение дампа

/// Номера колонок по заголовку: порядок в файле любой, лишние колонки не мешают.
#[derive(Debug, Clone, Copy)]
struct Columns {
    id: usize,
    title: usize,
    orig_title: usize,
    kind: usize,
    popularity: usize,
    ru: usize,
    tmdb_id: Option<usize>,
    release_date: Option<usize>,
    cover_url: Option<usize>,
}

impl Columns {
    fn from_header(header: &csv::StringRecord) -> Result<Self, BoxError> {
        let find = |name: &str| header.iter().position(|h| h.trim() == name);
        let need = |name: &str| find(name).ok_or_else(|| format!("dump has no column {name:?}"));
        Ok(Self {
            id: need("id")?,
            title: need("title")?,
            orig_title: need("orig_title")?,
            kind: need("type")?,
            popularity: need("popularity_score")?,
            ru: need("has_ru_locale")?,
            tmdb_id: find("tmdb_id"),
            release_date: find("release_date"),
            cover_url: find("cover_url"),
        })
    }

    /// Строка дампа; неполная или неверная (slug, тип, число) — `None`, она пропускается.
    /// Неверный `cover_url` строку не портит: фильм остаётся без постера.
    fn parse(&self, record: &csv::StringRecord) -> Option<Row> {
        let field = |i: usize| record.get(i).map(str::trim).unwrap_or_default();
        let optional = |i: Option<usize>| i.map(field).filter(|v| !v.is_empty());
        let slug = field(self.id);
        validate::slug("id", slug).ok()?;
        let title = field(self.title);
        if title.is_empty() || title.chars().count() > validate::MAX_TITLE {
            return None;
        }
        let popularity = match field(self.popularity) {
            "" => 0.0,
            value => value.parse::<f64>().ok().filter(|p| p.is_finite())?,
        };
        let ru = match field(self.ru) {
            "1" | "true" | "t" => true,
            "0" | "false" | "f" | "" => false,
            _ => return None,
        };
        Some(Row {
            slug: slug.to_string(),
            title: title.to_string(),
            original_title: Some(field(self.orig_title))
                .filter(|t| !t.is_empty())
                .map(|t| t.chars().take(validate::MAX_TITLE).collect()),
            kind: EntityKind::parse(field(self.kind))?,
            popularity,
            ru,
            tmdb_id: match optional(self.tmdb_id) {
                Some(id) => Some(id.parse().ok()?),
                None => None,
            },
            release_date: match optional(self.release_date) {
                Some(date) => Some(NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?),
                None => None,
            },
            cover_url: validate::url("cover_url", optional(self.cover_url).map(str::to_owned))
                .ok()
                .flatten(),
        })
    }
}

/// Читатель TSV без кавычек: в названиях бывают `"`, а табуляций нет (их убирает [`clean`]).
fn open_dump(path: &Path) -> Result<(csv::Reader<BufReader<File>>, Columns), BoxError> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .quoting(false)
        .flexible(true)
        .from_reader(BufReader::with_capacity(1 << 20, File::open(path)?));
    let columns = Columns::from_header(reader.headers()?)?;
    Ok((reader, columns))
}

// ---------------------------------------------------------------- проход 1: Top-K

/// Вес для кучи: popularity, при равенстве — slug (результат не зависит от порядка строк).
#[derive(Debug, Clone, PartialEq)]
struct Ranked(f64, String);

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .total_cmp(&other.0)
            .then_with(|| other.1.cmp(&self.1))
    }
}

/// `k` лучших: min-куча, на вершине — худший из отобранных; новый вытесняет его, если лучше.
struct TopK {
    k: usize,
    heap: BinaryHeap<Reverse<Ranked>>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.min(1 << 17) + 1),
        }
    }

    fn offer(&mut self, popularity: f64, slug: &str) {
        if self.k == 0 {
            return;
        }
        if self.heap.len() < self.k {
            self.heap
                .push(Reverse(Ranked(popularity, slug.to_string())));
            return;
        }
        let worst = &self.heap.peek().expect("k > 0").0;
        // Сравнение без аллокации: slug копируется, только если строка попадает в топ.
        let better = match popularity.total_cmp(&worst.0) {
            Ordering::Greater => true,
            Ordering::Equal => slug < worst.1.as_str(),
            Ordering::Less => false,
        };
        if better {
            self.heap.pop();
            self.heap
                .push(Reverse(Ranked(popularity, slug.to_string())));
        }
    }

    fn into_slugs(self) -> impl Iterator<Item = String> {
        self.heap.into_iter().map(|Reverse(Ranked(_, slug))| slug)
    }
}

/// Проход 1: slug'и горячего слоя.
fn elite(path: &Path, ru: usize, foreign: usize) -> Result<(HashSet<String>, u64), BoxError> {
    let (mut reader, columns) = open_dump(path)?;
    let (mut top_ru, mut top_foreign) = (TopK::new(ru), TopK::new(foreign));
    let mut record = csv::StringRecord::new();
    let mut invalid = 0;
    while reader.read_record(&mut record)? {
        match columns.parse(&record) {
            Some(row) if row.ru => top_ru.offer(row.popularity, &row.slug),
            Some(row) => top_foreign.offer(row.popularity, &row.slug),
            None => invalid += 1,
        }
    }
    Ok((
        top_ru
            .into_slugs()
            .chain(top_foreign.into_slugs())
            .collect(),
        invalid,
    ))
}

// ---------------------------------------------------------------- проход 2: маршрутизация

#[derive(Debug, Clone, Copy)]
pub struct BuildOptions {
    /// Сколько фильмов с русским названием в горячем слое (`usize::MAX` — все).
    pub ru: usize,
    /// Сколько иностранных без русского названия.
    pub foreign: usize,
    /// Пересоздать архив пустым в текущей схеме (иначе документы заменяются по slug).
    pub fresh: bool,
    /// Не удалять из PostgreSQL ушедшие в архив строки, только записать их TMDB id в
    /// `archived_tmdb`: первичная раскладка миллиона строк построчно (FK-каскады, триггеры
    /// счётчиков) идёт часами, её доделывает `scripts/archive-purge.sql` одним проходом.
    pub defer_delete: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            ru: DEFAULT_RU,
            foreign: DEFAULT_FOREIGN,
            fresh: false,
            defer_delete: false,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildReport {
    /// Строк в дампе (верных).
    pub rows: u64,
    /// Пропущено неверных строк.
    pub invalid: u64,
    /// Строк горячего слоя.
    pub hot: u64,
    /// Из них вставлено в PostgreSQL (вернулись из архива).
    pub inserted: u64,
    /// Включено (`active = true`) из скрытых.
    pub activated: u64,
    /// Документов записано в архив.
    pub archived: u64,
    /// Строк удалено из PostgreSQL (ушли в архив).
    pub deleted: u64,
    /// Остались в PostgreSQL, хотя не в топе: пользовательские данные.
    pub kept_in_use: u64,
    /// Остались в PostgreSQL: `active_manual`.
    pub kept_manual: u64,
    /// Удалено людей без работ.
    pub people_deleted: u64,
    /// Ушли в архив, но остались в PostgreSQL до `archive-purge.sql` (`defer_delete`).
    pub deferred: u64,
    /// Документов в архиве после сборки.
    pub archive_size: u64,
}

impl fmt::Display for BuildReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "dump rows:        {} (invalid skipped: {})",
            self.rows, self.invalid
        )?;
        writeln!(
            f,
            "hot (PostgreSQL): {} (inserted {}, activated {})",
            self.hot, self.inserted, self.activated
        )?;
        writeln!(f, "archived:         {}", self.archived)?;
        writeln!(f, "deleted from PG:  {}", self.deleted)?;
        writeln!(
            f,
            "kept in PG:       {} with user data, {} manual",
            self.kept_in_use, self.kept_manual
        )?;
        writeln!(f, "people deleted:   {}", self.people_deleted)?;
        if self.deferred > 0 {
            writeln!(
                f,
                "deferred:         {} (still in PostgreSQL: run scripts/archive-purge.sql)",
                self.deferred
            )?;
        }
        writeln!(f, "archive size:     {}", self.archive_size)
    }
}

enum Batch {
    Hot(Vec<Row>),
    Cold(Vec<Row>),
}

/// Проход 2 в отдельном потоке: чтение дампа и сортировка строк по пачкам.
fn route(
    path: PathBuf,
    elite: Arc<HashSet<String>>,
    tx: mpsc::Sender<Batch>,
) -> Result<(u64, u64), BoxError> {
    let (mut reader, columns) = open_dump(&path)?;
    let mut record = csv::StringRecord::new();
    let (mut hot, mut cold) = (Vec::with_capacity(BATCH), Vec::with_capacity(BATCH));
    let (mut rows, mut invalid) = (0, 0);
    let send = |batch| tx.blocking_send(batch).map_err(|_| "build stopped");
    while reader.read_record(&mut record)? {
        let Some(row) = columns.parse(&record) else {
            invalid += 1;
            continue;
        };
        rows += 1;
        if elite.contains(&row.slug) {
            hot.push(row);
            if hot.len() == BATCH {
                send(Batch::Hot(std::mem::take(&mut hot)))?;
            }
        } else {
            cold.push(row);
            if cold.len() == BATCH {
                send(Batch::Cold(std::mem::take(&mut cold)))?;
            }
        }
    }
    if !hot.is_empty() {
        send(Batch::Hot(hot))?;
    }
    if !cold.is_empty() {
        send(Batch::Cold(cold))?;
    }
    Ok((rows, invalid))
}

/// Строки PostgreSQL, ожидающие коммита архива перед удалением.
#[derive(Default)]
struct Pending {
    /// Строки PostgreSQL и их TMDB id.
    rows: Vec<(Uuid, Option<i32>)>,
    /// TMDB id документов, которых в PostgreSQL не было.
    tmdb_ids: Vec<i32>,
}

/// Собирает горячий слой и архив из дампа `path`.
pub async fn build(
    state: &AppState,
    archive: Arc<Archive>,
    path: &Path,
    options: BuildOptions,
) -> Result<BuildReport, BoxError> {
    let mut report = BuildReport::default();

    // Проход 1.
    let (elite, invalid) = {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || elite(&path, options.ru, options.foreign)).await??
    };
    tracing::info!(
        elite = elite.len(),
        invalid,
        "archive build: top-k collected"
    );

    // Проход 2: читатель в своём потоке, запись — здесь.
    let archive = if options.fresh {
        tokio::task::spawn_blocking(move || archive.recreate()).await??
    } else {
        archive
    };
    let mut writer = Some(archive.writer()?);
    let (tx, mut rx) = mpsc::channel::<Batch>(4);
    let reader = {
        let (path, elite) = (path.to_path_buf(), Arc::new(elite));
        tokio::task::spawn_blocking(move || route(path, elite, tx))
    };

    let mut pending = Pending::default();
    let mut since_commit = 0u64;
    let result: Result<(), BoxError> = async {
        while let Some(batch) = rx.recv().await {
            match batch {
                Batch::Hot(rows) => {
                    report.hot += rows.len() as u64;
                    upsert_hot(state, &rows, &mut report).await?;
                    let slugs: Vec<String> = rows.into_iter().map(|r| r.slug).collect();
                    with_writer(&mut writer, &archive, move |archive, w| {
                        slugs.iter().for_each(|slug| archive.delete(w, slug));
                        Ok(())
                    })
                    .await?;
                }
                Batch::Cold(rows) => {
                    let docs = route_cold(state, rows, &mut pending, &mut report).await?;
                    since_commit += docs.archive.len() as u64;
                    report.archived += docs.archive.len() as u64;
                    with_writer(&mut writer, &archive, move |archive, w| {
                        docs.keep.iter().for_each(|slug| archive.delete(w, slug));
                        docs.archive
                            .iter()
                            .try_for_each(|doc| archive.upsert(w, doc))
                    })
                    .await?;
                    if since_commit >= COMMIT_EVERY {
                        commit(&mut writer, &archive).await?;
                        delete_archived(state, &mut pending, options, &mut report).await?;
                        since_commit = 0;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    // Ошибка записи закрывает канал — читатель остановится; его ошибка важнее.
    drop(rx);
    let (rows, invalid) = reader.await??;
    result?;
    report.rows = rows;
    report.invalid = invalid;

    commit(&mut writer, &archive).await?;
    delete_archived(state, &mut pending, options, &mut report).await?;
    drop(writer);
    archive.reload()?;
    report.archive_size = archive.len();
    Ok(report)
}

/// Операция писателя в блокирующем потоке: писатель уходит туда и возвращается.
async fn with_writer(
    writer: &mut Option<IndexWriter>,
    archive: &Arc<Archive>,
    f: impl FnOnce(&Archive, &mut IndexWriter) -> Result<(), ArchiveError> + Send + 'static,
) -> Result<(), BoxError> {
    let mut w = writer.take().ok_or("archive writer is gone")?;
    let archive = archive.clone();
    let (w, result) = tokio::task::spawn_blocking(move || {
        let result = f(&archive, &mut w);
        (w, result)
    })
    .await?;
    *writer = Some(w);
    Ok(result?)
}

async fn commit(writer: &mut Option<IndexWriter>, archive: &Arc<Archive>) -> Result<(), BoxError> {
    with_writer(writer, archive, |_, w| {
        w.commit()?;
        Ok(())
    })
    .await?;
    tracing::info!("archive build: committed");
    Ok(())
}

/// Горячая пачка: существующие строки включаются, отсутствующие (вернулись из архива)
/// вставляются с постером, связью TMDB и popularity. `active_manual` не трогается.
async fn upsert_hot(
    state: &AppState,
    rows: &[Row],
    report: &mut BuildReport,
) -> Result<(), BoxError> {
    let mut tx = state.db.begin().await?;
    let changed: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "INSERT INTO entities (kind, slug, title, original_title, release_date, cover_url, active)
         SELECT u.k::entity_kind, u.s, u.t, u.o, u.d, u.c, true
         FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::date[], $6::text[])
             AS u(k, s, t, o, d, c)
         ON CONFLICT (slug) DO UPDATE SET active = true
             WHERE NOT entities.active AND NOT entities.active_manual
         RETURNING id, slug, (xmax = 0) AS inserted",
    )
    .bind(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>())
    .bind(
        rows.iter()
            .map(|r| r.original_title.as_deref())
            .collect::<Vec<_>>(),
    )
    .bind(rows.iter().map(|r| r.release_date).collect::<Vec<_>>())
    .bind(
        rows.iter()
            .map(|r| r.cover_url.as_deref())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *tx)
    .await?;

    let by_slug: HashMap<&str, &Row> = rows.iter().map(|r| (r.slug.as_str(), r)).collect();
    let (mut ids, mut tmdb_ids, mut popularity) = (Vec::new(), Vec::new(), Vec::new());
    for (id, slug, inserted) in &changed {
        if !inserted {
            report.activated += 1;
            continue;
        }
        report.inserted += 1;
        if let Some(row) = by_slug.get(slug.as_str()) {
            if let Some(tmdb_id) = row.tmdb_id {
                ids.push(*id);
                tmdb_ids.push(tmdb_id);
                popularity.push(row.popularity);
            }
        }
    }
    if !ids.is_empty() {
        sqlx::query(
            "INSERT INTO entity_tmdb (entity_id, tmdb_id)
             SELECT * FROM unnest($1::uuid[], $2::int[]) ON CONFLICT DO NOTHING",
        )
        .bind(&ids)
        .bind(&tmdb_ids)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO entity_stats (entity_id, tmdb_popularity)
             SELECT * FROM unnest($1::uuid[], $2::float8[]) ON CONFLICT DO NOTHING",
        )
        .bind(&ids)
        .bind(&popularity)
        .execute(&mut *tx)
        .await?;
    }
    let returned: Vec<i32> = rows.iter().filter_map(|r| r.tmdb_id).collect();
    sqlx::query("DELETE FROM archived_tmdb WHERE tmdb_id = ANY($1)")
        .bind(&returned)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

struct ColdDocs {
    /// Остаются в PostgreSQL: из архива их убрать.
    keep: Vec<String>,
    archive: Vec<ArchiveDoc>,
}

/// Холодная пачка: что идёт в архив, а что остаётся в PostgreSQL (`active_manual`, данные
/// пользователей). Строки PostgreSQL, идущие в архив, ждут коммита в `pending`.
async fn route_cold(
    state: &AppState,
    rows: Vec<Row>,
    pending: &mut Pending,
    report: &mut BuildReport,
) -> Result<ColdDocs, BoxError> {
    let slugs: Vec<&str> = rows.iter().map(|r| r.slug.as_str()).collect();
    let found: Vec<(Uuid, String, bool)> =
        sqlx::query_as("SELECT id, slug, active_manual FROM entities WHERE slug = ANY($1)")
            .bind(&slugs)
            .fetch_all(&state.db)
            .await?;
    let ids: Vec<Uuid> = found.iter().filter(|f| !f.2).map(|f| f.0).collect();
    let in_use = state.interests.entities_in_use(&ids).await?;
    let found: HashMap<String, (Uuid, bool)> = found
        .into_iter()
        .map(|(id, slug, manual)| (slug, (id, manual)))
        .collect();

    let mut docs = ColdDocs {
        keep: Vec::new(),
        archive: Vec::with_capacity(rows.len()),
    };
    for row in rows {
        match found.get(&row.slug) {
            Some((_, true)) => {
                report.kept_manual += 1;
                docs.keep.push(row.slug);
            }
            Some((id, false)) if in_use.contains(id) => {
                report.kept_in_use += 1;
                docs.keep.push(row.slug);
            }
            Some((id, false)) => {
                pending.rows.push((*id, row.tmdb_id));
                docs.archive.push(row.doc());
            }
            None => {
                pending.tmdb_ids.extend(row.tmdb_id);
                docs.archive.push(row.doc());
            }
        }
    }
    Ok(docs)
}

/// Удаляет из PostgreSQL строки, уже закоммиченные в архив, пачками по [`BATCH`]; запоминает их
/// TMDB id (импорт пропустит) и удаляет людей, у которых не осталось работ.
async fn delete_archived(
    state: &AppState,
    pending: &mut Pending,
    options: BuildOptions,
    report: &mut BuildReport,
) -> Result<(), BoxError> {
    let pending = std::mem::take(pending);
    for chunk in pending.tmdb_ids.chunks(BATCH) {
        archive_tmdb_ids(&state.db, chunk).await?;
    }
    if options.defer_delete {
        let tmdb_ids: Vec<i32> = pending.rows.iter().filter_map(|(_, t)| *t).collect();
        for chunk in tmdb_ids.chunks(BATCH) {
            archive_tmdb_ids(&state.db, chunk).await?;
        }
        report.deferred += pending.rows.len() as u64;
        return Ok(());
    }
    for chunk in pending.rows.chunks(BATCH) {
        // Повторная проверка: пока шла пачка, фильм могли добавить в коллекцию. Такой остаётся
        // и в архиве — его уберёт следующая сборка.
        let all: Vec<Uuid> = chunk.iter().map(|(id, _)| *id).collect();
        let in_use = state.interests.entities_in_use(&all).await?;
        let (ids, tmdb_ids): (Vec<Uuid>, Vec<Option<i32>>) = chunk
            .iter()
            .filter(|(id, _)| !in_use.contains(id))
            .copied()
            .unzip();
        report.kept_in_use += (chunk.len() - ids.len()) as u64;

        let mut tx = state.db.begin().await?;
        archive_tmdb_ids(
            &mut *tx,
            &tmdb_ids.into_iter().flatten().collect::<Vec<_>>(),
        )
        .await?;
        let people: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT person_id FROM entity_credits WHERE entity_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        let deleted = sqlx::query("DELETE FROM entities WHERE id = ANY($1) AND NOT active_manual")
            .bind(&ids)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let people_deleted = sqlx::query(
            "DELETE FROM people p WHERE p.id = ANY($1)
               AND NOT EXISTS (SELECT 1 FROM entity_credits c WHERE c.person_id = p.id)",
        )
        .bind(&people)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        report.deleted += deleted;
        report.people_deleted += people_deleted;
    }
    Ok(())
}

async fn archive_tmdb_ids(db: impl sqlx::PgExecutor<'_>, tmdb_ids: &[i32]) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO archived_tmdb (tmdb_id) SELECT unnest($1::int[]) ON CONFLICT DO NOTHING",
    )
    .bind(tmdb_ids)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_keeps_the_best_and_breaks_ties_by_slug() {
        let mut top = TopK::new(3);
        for (p, slug) in [
            (1.0, "a"),
            (5.0, "b"),
            (3.0, "c"),
            (5.0, "d"),
            (0.5, "e"),
            (3.0, "b2"),
        ] {
            top.offer(p, slug);
        }
        let mut slugs: Vec<String> = top.into_slugs().collect();
        slugs.sort();
        // 5.0 b, 5.0 d, затем из двух 3.0 — меньший slug.
        assert_eq!(slugs, ["b", "b2", "d"]);

        let mut none = TopK::new(0);
        none.offer(1.0, "a");
        assert_eq!(none.into_slugs().count(), 0);
    }

    #[test]
    fn russian_locale_is_cyrillic_in_title() {
        assert!(has_ru_locale("Дюна"));
        assert!(has_ru_locale("Ёлки 2"));
        assert!(!has_ru_locale("Dune"));
        assert!(!has_ru_locale("Amélie"));
    }

    #[test]
    fn dump_rows_are_parsed_by_header() {
        let header = csv::StringRecord::from(vec![
            "type",
            "id",
            "title",
            "orig_title",
            "popularity_score",
            "has_ru_locale",
            "extra",
            "cover_url",
        ]);
        let columns = Columns::from_header(&header).unwrap();
        let row = |fields: Vec<&str>| columns.parse(&csv::StringRecord::from(fields));

        let parsed = row(vec![
            "movie",
            "dune-2021",
            "Дюна",
            "Dune \"2021\"",
            "12.5",
            "1",
            "x",
            "https://image.tmdb.org/t/p/w780/a.jpg",
        ])
        .unwrap();
        assert_eq!(parsed.slug, "dune-2021");
        assert_eq!(parsed.original_title.as_deref(), Some("Dune \"2021\""));
        assert!(parsed.ru);
        assert_eq!(parsed.popularity, 12.5);
        assert_eq!(parsed.tmdb_id, None);
        assert_eq!(
            parsed.cover_url.as_deref(),
            Some("https://image.tmdb.org/t/p/w780/a.jpg")
        );
        let bad_cover = row(vec!["movie", "ok", "T", "", "1", "0", "", "/a.jpg"]).unwrap();
        assert_eq!(
            bad_cover.cover_url, None,
            "not a URL: the row stays, without a cover"
        );

        assert!(row(vec!["movie", "Bad Slug", "T", "", "1", "0", ""]).is_none());
        assert!(row(vec!["tvSeries", "ok", "T", "", "1", "0", ""]).is_none());
        assert!(row(vec!["movie", "ok", "T", "", "NaN", "0", ""]).is_none());
        assert!(row(vec!["movie", "ok", "", "", "1", "0", ""]).is_none());
        assert!(row(vec!["movie", "ok", "T", "", "", "yes", ""]).is_none());
        assert_eq!(
            row(vec!["movie", "ok", "T", "", "", "", ""])
                .unwrap()
                .popularity,
            0.0
        );

        assert!(Columns::from_header(&csv::StringRecord::from(vec!["id", "title"])).is_err());
    }
}
