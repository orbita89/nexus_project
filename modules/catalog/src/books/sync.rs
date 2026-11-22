//! Книжный поиск ← PostgreSQL: слой А (Tantivy, текст) и слой Б (RAM, фасеты и порядок).
//!
//! - [`rebuild`] — все видимые книги пачками (keyset по id) в индекс одним коммитом (до него
//!   поиск читает прежний индекс) и тем же проходом — новый слой Б. Запускается командой
//!   `nexus books index` и самим сервером при старте, если индекс пуст ([`spawn_initial`]).
//! - [`reload_sidecar`] — только слой Б: тот же проход по БД без записи в Tantivy. Раз в сутки,
//!   после [`update`] (не чаще [`SIDECAR_MIN_INTERVAL`]) и при старте, если снимка нет.
//! - [`update`] — затронутые книги после правки в админке и новых оценок Nexus (тот же путь,
//!   что обновляет Meilisearch: `search::update_entities`). В Tantivy пишутся только книги,
//!   у которых изменился текст ([`BookDoc::text_hash`]); скрытые и удалённые — удаляются из
//!   него сразу.
//! - [`apply_ranks`] — ручные места из `catalog_manual_ranks` в слой Б за миллисекунды:
//!   после правки в админке и импорта списка; другие процессы замечают правку опросом
//!   ([`RANKS_POLL`]).
//!
//! Документ строится целиком из БД (книга, авторы, участники, теги, оценки, место), поэтому
//! повтор любого шага безопасен. Номер книги `ord` выдаёт `catalog_book_ords` один раз.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};
use shared::AppState;
use tokio::sync::Notify;
use uuid::Uuid;

use super::index::{BookDoc, BookForm, BookIndex, IndexError};
use super::sidecar::SidecarBuilder;
use super::BoxError;
use crate::models::{SummaryAuthor, VISIBLE};

/// Сколько книг читать из БД и добавлять в индекс за раз.
const BATCH: i64 = 2000;
/// Пересборка слоя Б после правок — не чаще: поток новых оценок не должен превращаться в
/// непрерывное чтение всех книг.
pub const SIDECAR_MIN_INTERVAL: Duration = Duration::from_secs(30);
/// Полная пересборка слоя Б по расписанию.
pub const SIDECAR_DAILY: Duration = Duration::from_secs(24 * 60 * 60);
/// Как часто проверять, не поменялся ли ручной порядок (импорт списка другим процессом,
/// правка на другой реплике).
pub const RANKS_POLL: Duration = Duration::from_secs(60);

/// Книжный поиск включён: задан `BOOKS_INDEX_DIR`.
pub fn enabled(state: &AppState) -> bool {
    state.config.books_index_dir.is_some()
}

/// Индекс из `BOOKS_INDEX_DIR`. `None` — книжный поиск выключен.
pub fn open(state: &AppState) -> Option<Result<Arc<BookIndex>, IndexError>> {
    let dir = state.config.books_index_dir.as_deref()?;
    Some(BookIndex::open(Path::new(dir)))
}

/// Слой Б устарел (правка книг): фоновая задача [`spawn_initial`] пересоберёт его.
fn dirty() -> &'static Notify {
    static DIRTY: OnceLock<Notify> = OnceLock::new();
    DIRTY.get_or_init(Notify::new)
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    slug: String,
    title: String,
    original_title: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    cover_width: Option<i32>,
    cover_height: Option<i32>,
    publisher: Option<String>,
    series: Option<String>,
    language: Option<String>,
    has_description: bool,
    form: String,
}

const COLUMNS: &str = "e.id, e.slug, e.title, e.original_title, e.release_date, e.cover_url, \
    e.cover_width, e.cover_height, e.metadata ->> 'publisher' AS publisher, \
    e.metadata ->> 'series' AS series, e.metadata ->> 'language' AS language, \
    coalesce(e.description <> '', false) AS has_description, \
    CASE WHEN e.metadata ->> 'kind' IN ('article', 'document') THEN e.metadata ->> 'kind' \
         WHEN e.metadata ? 'collection' THEN 'collection' ELSE 'book' END AS form";

/// Номера книг `ids` (выдаются тем, у кого их ещё нет) и отпечатки их текста в индексе.
/// Вставляются только недостающие: `ON CONFLICT` тратил бы номер на каждую уже известную
/// книгу, и массивы слоя Б росли бы дырами.
async fn ords_of(
    db: &sqlx::PgPool,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, (u32, Option<i64>)>, sqlx::Error> {
    sqlx::query(
        "INSERT INTO catalog_book_ords (entity_id)
         SELECT id FROM unnest($1::uuid[]) AS id
         WHERE NOT EXISTS (SELECT 1 FROM catalog_book_ords o WHERE o.entity_id = id)
         ON CONFLICT (entity_id) DO NOTHING",
    )
    .bind(ids)
    .execute(db)
    .await?;
    let rows: Vec<(Uuid, i32, Option<i64>)> = sqlx::query_as(
        "SELECT entity_id, ord, text_hash FROM catalog_book_ords WHERE entity_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, ord, hash)| Some((id, (u32::try_from(ord).ok()?, hash))))
        .collect())
}

/// Документы пачки книг: номера, участники, теги, оценки и ручные места — по одному запросу
/// на пачку. Второе значение — отпечатки текста, уже записанного в индекс.
async fn docs_of(
    db: &sqlx::PgPool,
    rows: Vec<Row>,
) -> Result<(Vec<BookDoc>, HashMap<Uuid, i64>), sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let ords = ords_of(db, &ids).await?;
    // `active_credits` — сколько видимых работ у человека: известность автора для ранга.
    let credits: Vec<(Uuid, String, String, String, i32)> = sqlx::query_as(
        "SELECT ec.entity_id, ec.role, p.slug, p.full_name, coalesce(pc.active_credits, 0)
         FROM entity_credits ec JOIN people p ON p.id = ec.person_id
         LEFT JOIN person_credit_counts pc ON pc.person_id = p.id
         WHERE ec.entity_id = ANY($1) ORDER BY ec.entity_id, ec.position",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let tags: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT et.entity_id, t.slug FROM entity_tags et JOIN tags t ON t.id = et.tag_id
         WHERE et.entity_id = ANY($1) ORDER BY t.slug",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let ratings: Vec<(Uuid, Option<f64>, i64)> = sqlx::query_as(
        "SELECT entity_id, average, count FROM entity_ratings WHERE entity_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let ranks: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT entity_id, rank FROM catalog_manual_ranks WHERE entity_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let mut hashes = HashMap::new();
    let mut docs: HashMap<Uuid, BookDoc> = rows
        .into_iter()
        .filter_map(|row| {
            let (ord, hash) = *ords.get(&row.id)?;
            if let Some(hash) = hash {
                hashes.insert(row.id, hash);
            }
            let doc = BookDoc {
                id: row.id,
                ord,
                slug: row.slug,
                title: row.title,
                original_title: row.original_title,
                release_date: row.release_date,
                cover_url: row.cover_url,
                cover_width: row.cover_width,
                cover_height: row.cover_height,
                publisher: row.publisher,
                series: row.series,
                language: row.language,
                has_description: row.has_description,
                form: BookForm::parse(&row.form).unwrap_or_default(),
                ..BookDoc::default()
            };
            Some((row.id, doc))
        })
        .collect();
    for (id, role, slug, name, works) in credits {
        if let Some(doc) = docs.get_mut(&id) {
            if role == "author" {
                doc.author_works = doc.author_works.max(works.max(0) as u32);
                doc.authors.push(SummaryAuthor {
                    slug: slug.clone(),
                    name,
                });
            }
            doc.credits.push((role, slug));
        }
    }
    for (id, slug) in tags {
        if let Some(doc) = docs.get_mut(&id) {
            doc.tags.push(slug);
        }
    }
    for (id, average, count) in ratings {
        if let Some(doc) = docs.get_mut(&id) {
            doc.rating_avg = average;
            doc.ratings_count = count;
        }
    }
    for (id, rank) in ranks {
        if let Some(doc) = docs.get_mut(&id) {
            doc.manual_rank = u16::try_from(rank).unwrap_or(0);
        }
    }
    // В порядке id, как прочитали: выдача при равных ключах сортировки не зависит от HashMap.
    let docs = ids.iter().filter_map(|id| docs.remove(id)).collect();
    Ok((docs, hashes))
}

/// Запомнить отпечатки текста, записанного в индекс (`None` — книги в индексе больше нет).
async fn save_hashes(db: &sqlx::PgPool, hashes: &[(Uuid, Option<i64>)]) -> Result<(), sqlx::Error> {
    for chunk in hashes.chunks(10_000) {
        let ids: Vec<Uuid> = chunk.iter().map(|h| h.0).collect();
        let values: Vec<Option<i64>> = chunk.iter().map(|h| h.1).collect();
        sqlx::query(
            "UPDATE catalog_book_ords o SET text_hash = v.hash
             FROM unnest($1::uuid[], $2::bigint[]) AS v(id, hash)
             WHERE o.entity_id = v.id AND o.text_hash IS DISTINCT FROM v.hash",
        )
        .bind(&ids)
        .bind(&values)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Все видимые книги пачками по [`BATCH`] (keyset по id).
struct Pages {
    after: Option<Uuid>,
    seen: u64,
}

impl Pages {
    fn new() -> Self {
        Self {
            after: None,
            seen: 0,
        }
    }

    /// Следующая пачка: документы и отпечатки их текста в индексе. `None` — книги кончились.
    async fn next(
        &mut self,
        db: &sqlx::PgPool,
    ) -> Result<Option<(Vec<BookDoc>, HashMap<Uuid, i64>)>, sqlx::Error> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM entities e
             WHERE e.kind = 'book' AND {VISIBLE} AND e.parent_id IS NULL AND ($1::uuid IS NULL OR e.id > $1)
             ORDER BY e.id LIMIT $2"
        ))
        .bind(self.after)
        .bind(BATCH)
        .fetch_all(db)
        .await?;
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        self.after = Some(last.id);
        let page = docs_of(db, rows).await?;
        self.seen += page.0.len() as u64;
        if self.seen % 50_000 < BATCH as u64 {
            tracing::info!(books = self.seen, "books read from the database");
        }
        Ok(Some(page))
    }
}

/// Полная перестройка: индекс Tantivy и слой Б одним проходом по БД. Возвращает число книг.
/// Идёт другая запись — `Busy`.
pub async fn rebuild(state: &AppState) -> Result<u64, BoxError> {
    let index = open(state).ok_or("BOOKS_INDEX_DIR is not set")??;
    let started = Instant::now();
    let rebuild = tokio::task::spawn_blocking({
        let index = index.clone();
        move || index.rebuild()
    })
    .await??;
    let mut writer = rebuild;
    let mut builder = SidecarBuilder::default();
    let mut hashes: Vec<(Uuid, Option<i64>)> = Vec::new();
    let mut pages = Pages::new();
    while let Some((docs, _)) = pages.next(&state.db).await? {
        for doc in &docs {
            builder.add(doc);
            hashes.push((doc.id, Some(doc.text_hash())));
        }
        // Разбор текста — работа процессора: вне потоков async.
        writer = tokio::task::spawn_blocking(move || {
            writer.add(&docs)?;
            Ok::<_, IndexError>(writer)
        })
        .await??;
    }
    let total = tokio::task::spawn_blocking(move || writer.commit()).await??;
    // Книги, которых в новом индексе нет, — без отпечатка: появятся — будут записаны заново.
    sqlx::query("UPDATE catalog_book_ords SET text_hash = NULL WHERE text_hash IS NOT NULL")
        .execute(&state.db)
        .await?;
    save_hashes(&state.db, &hashes).await?;
    let sidecar = builder.finish();
    tokio::task::spawn_blocking(move || index.set_sidecar(sidecar)).await??;
    remember(&LAST_INDEX, started, total);
    remember(&LAST_SIDECAR, started, total);
    Ok(total)
}

/// Последняя сборка: когда закончилась, сколько шла, сколько книг.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    pub at: DateTime<Utc>,
    pub took_ms: u64,
    pub books: u64,
}

static LAST_SIDECAR: Mutex<Option<BuildInfo>> = Mutex::new(None);
static LAST_INDEX: Mutex<Option<BuildInfo>> = Mutex::new(None);

fn remember(slot: &Mutex<Option<BuildInfo>>, started: Instant, books: u64) {
    *slot.lock().unwrap() = Some(BuildInfo {
        at: Utc::now(),
        took_ms: started.elapsed().as_millis() as u64,
        books,
    });
}

/// Последние сборки в этом процессе: слоя Б и всего индекса (`None` — с запуска не было).
pub fn last_builds() -> (Option<BuildInfo>, Option<BuildInfo>) {
    (*LAST_SIDECAR.lock().unwrap(), *LAST_INDEX.lock().unwrap())
}

/// Видимых книг в БД (тех, что должны быть в индексе) и выданных номеров `ord`.
pub async fn db_counts(db: &sqlx::PgPool) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT (SELECT count(*) FROM entities e
                 WHERE e.kind = 'book' AND {VISIBLE} AND e.parent_id IS NULL),
                (SELECT count(*) FROM catalog_book_ords)"
    ))
    .fetch_one(db)
    .await
}

/// Пересобрать только слой Б (фасеты, сортировки, места) из БД — Tantivy не трогается.
/// Возвращает число книг.
pub async fn reload_sidecar(state: &AppState) -> Result<u64, BoxError> {
    let index = open(state).ok_or("BOOKS_INDEX_DIR is not set")??;
    let started = Instant::now();
    let mut builder = SidecarBuilder::default();
    let mut pages = Pages::new();
    while let Some((docs, _)) = pages.next(&state.db).await? {
        for doc in &docs {
            builder.add(doc);
        }
    }
    let books = pages.seen;
    let sidecar = builder.finish();
    tokio::task::spawn_blocking(move || index.set_sidecar(sidecar)).await??;
    tracing::info!(books, took = ?started.elapsed(), "books sidecar rebuilt");
    remember(&LAST_SIDECAR, started, books);
    Ok(books)
}

/// Ручные места книг: `ord → место`.
async fn load_ranks(db: &sqlx::PgPool) -> Result<HashMap<u32, u16>, sqlx::Error> {
    let rows: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT o.ord, r.rank FROM catalog_manual_ranks r
         JOIN catalog_book_ords o ON o.entity_id = r.entity_id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(ord, rank)| Some((u32::try_from(ord).ok()?, u16::try_from(rank).ok()?)))
        .collect())
}

/// Ручные места из БД — в слой Б (без пересборки фасетов). Книжный поиск выключен — ничего.
pub async fn apply_ranks(state: &AppState) -> Result<(), BoxError> {
    let Some(index) = open(state) else {
        return Ok(());
    };
    let index = index?;
    let ranks = load_ranks(&state.db).await?;
    tokio::task::spawn_blocking(move || index.set_ranks(&ranks)).await??;
    Ok(())
}

/// Отпечаток ручного списка: число строк и время последней правки.
async fn ranks_version(db: &sqlx::PgPool) -> Result<(i64, Option<DateTime<Utc>>), sqlx::Error> {
    sqlx::query_as("SELECT count(*), max(updated_at) FROM catalog_manual_ranks")
        .fetch_one(db)
        .await
}

/// Дослать затронутые сущности: книги с изменившимся текстом — в Tantivy, скрытые и
/// удалённые — из него; слой Б пересобирается в фоне ([`spawn_initial`]).
pub async fn update(state: &AppState, ids: &[Uuid]) -> Result<(), BoxError> {
    let Some(index) = open(state) else {
        return Ok(());
    };
    if ids.is_empty() {
        return Ok(());
    }
    let index = index?;
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM entities e
         WHERE e.id = ANY($1) AND e.kind = 'book' AND {VISIBLE} AND e.parent_id IS NULL ORDER BY e.id"
    ))
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    let (docs, stored) = docs_of(&state.db, rows).await?;
    let present: Vec<Uuid> = docs.iter().map(|doc| doc.id).collect();
    let gone: Vec<Uuid> = ids
        .iter()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    let changed: Vec<BookDoc> = docs
        .into_iter()
        .filter(|doc| stored.get(&doc.id) != Some(&doc.text_hash()))
        .collect();
    let mut hashes: Vec<(Uuid, Option<i64>)> = changed
        .iter()
        .map(|doc| (doc.id, Some(doc.text_hash())))
        .collect();
    let had_books = !present.is_empty();
    // Среди затронутых нет книг и индекс их не знает — писателя не открываем: правка фильма не
    // должна стоить коммита в книжный индекс.
    let written = tokio::task::spawn_blocking({
        let gone = gone.clone();
        move || {
            let gone_indexed = !gone.is_empty() && index.has_any(&gone)?;
            if changed.is_empty() && !gone_indexed {
                return Ok(false);
            }
            let remove: Vec<Uuid> = if gone_indexed { gone } else { Vec::new() };
            index.update(&remove, &changed)?;
            Ok::<_, IndexError>(gone_indexed)
        }
    })
    .await??;
    if written {
        hashes.extend(gone.iter().map(|id| (*id, None)));
    }
    save_hashes(&state.db, &hashes).await?;
    if had_books || written {
        dirty().notify_one();
    }
    Ok(())
}

/// При старте сервера: индекс включён, но пуст (первый запуск, сменилась схема) — построить в
/// фоне; нет снимка слоя Б — собрать его. Дальше — фоновая задача слоя Б: пересборка раз в
/// сутки и после правок, ручные места — по опросу [`RANKS_POLL`].
pub fn spawn_initial(state: AppState) {
    let Some(index) = open(&state) else {
        return;
    };
    tokio::spawn(async move {
        match index {
            Ok(index) if index.is_empty() => {
                tracing::info!("books index is empty, building it from the database");
                match rebuild(&state).await {
                    Ok(books) => tracing::info!(books, "books index built"),
                    Err(error) => tracing::warn!(%error, "books index build failed"),
                }
            }
            Ok(index) if index.sidecar().is_empty() => {
                tracing::info!(books = index.len(), "books index opened, building sidecar");
                if let Err(error) = reload_sidecar(&state).await {
                    tracing::warn!(%error, "books sidecar build failed");
                }
            }
            Ok(index) => {
                tracing::info!(books = index.len(), "books index opened");
                // Списки раздела — заранее: первый посетитель не ждёт сортировки всех книг.
                let warmed = tokio::task::spawn_blocking(move || index.warm()).await;
                if let Ok(Err(error)) = warmed {
                    tracing::warn!(%error, "books index warm-up failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "books index cannot be opened");
                return;
            }
        }
        sidecar_loop(state).await;
    });
}

/// Фоновая жизнь слоя Б: пересборка после правок (не чаще [`SIDECAR_MIN_INTERVAL`]) и раз в
/// сутки; ручные места — когда меняется их отпечаток в БД.
async fn sidecar_loop(state: AppState) {
    let mut built = Instant::now();
    let mut pending = false;
    let mut ranks = ranks_version(&state.db).await.ok();
    loop {
        tokio::select! {
            _ = dirty().notified() => pending = true,
            _ = tokio::time::sleep(RANKS_POLL) => {}
        }
        let due = built.elapsed() >= SIDECAR_DAILY;
        if (pending && built.elapsed() >= SIDECAR_MIN_INTERVAL) || due {
            pending = false;
            built = Instant::now();
            if let Err(error) = reload_sidecar(&state).await {
                tracing::warn!(%error, "books sidecar rebuild failed");
            }
            ranks = ranks_version(&state.db).await.ok();
            continue;
        }
        if pending {
            // Подождать до конца интервала и собрать один раз за всю пачку правок.
            let wait = SIDECAR_MIN_INTERVAL.saturating_sub(built.elapsed());
            tokio::time::sleep(wait).await;
            dirty().notify_one();
            continue;
        }
        match ranks_version(&state.db).await {
            Ok(version) if ranks.as_ref() != Some(&version) => {
                ranks = Some(version);
                if let Err(error) = apply_ranks(&state).await {
                    tracing::warn!(%error, "books manual ranks were not applied");
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "books manual ranks check failed"),
        }
    }
}
