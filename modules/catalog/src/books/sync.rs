//! Книжный индекс ← PostgreSQL: полная перестройка и досылка изменённых книг.
//!
//! - [`rebuild`] — все видимые книги пачками (keyset по id) в индекс одним коммитом: до него
//!   поиск читает прежний индекс. Запускается командой `nexus books index` и самим сервером при
//!   старте, если индекс пуст ([`spawn_initial`]).
//! - [`update`] — затронутые книги после правки в админке и после новых оценок Nexus (тот же
//!   путь, что обновляет Meilisearch: `search::update_entities`).
//!
//! Документ строится целиком из БД (книга, авторы, участники, теги, оценки), поэтому повтор
//! любого шага безопасен.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::NaiveDate;
use shared::AppState;
use uuid::Uuid;

use super::index::{BookDoc, BookIndex, IndexError};
use super::BoxError;
use crate::models::{SummaryAuthor, VISIBLE};

/// Сколько книг читать из БД и добавлять в индекс за раз.
const BATCH: i64 = 2000;

/// Книжный поиск включён: задан `BOOKS_INDEX_DIR`.
pub fn enabled(state: &AppState) -> bool {
    state.config.books_index_dir.is_some()
}

/// Индекс из `BOOKS_INDEX_DIR`. `None` — книжный поиск выключен.
pub fn open(state: &AppState) -> Option<Result<Arc<BookIndex>, IndexError>> {
    let dir = state.config.books_index_dir.as_deref()?;
    Some(BookIndex::open(Path::new(dir)))
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
}

const COLUMNS: &str = "e.id, e.slug, e.title, e.original_title, e.release_date, e.cover_url, \
    e.cover_width, e.cover_height, e.metadata ->> 'publisher' AS publisher, \
    e.metadata ->> 'series' AS series, e.metadata ->> 'language' AS language, \
    coalesce(e.description <> '', false) AS has_description";

/// Документы пачки книг: участники, теги и оценки — по одному запросу на пачку.
async fn docs_of(db: &sqlx::PgPool, rows: Vec<Row>) -> Result<Vec<BookDoc>, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
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

    let mut docs: HashMap<Uuid, BookDoc> = rows
        .into_iter()
        .map(|row| {
            let doc = BookDoc {
                id: row.id,
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
                ..BookDoc::default()
            };
            (row.id, doc)
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
    // В порядке id, как прочитали: выдача при равных ключах сортировки не зависит от HashMap.
    Ok(ids.iter().filter_map(|id| docs.remove(id)).collect())
}

/// Полная перестройка индекса из БД. Возвращает число книг. Идёт другая запись — `Busy`.
pub async fn rebuild(state: &AppState) -> Result<u64, BoxError> {
    let index = open(state).ok_or("BOOKS_INDEX_DIR is not set")??;
    let mut rebuild = tokio::task::spawn_blocking({
        let index = index.clone();
        move || index.rebuild()
    })
    .await??;
    let mut after: Option<Uuid> = None;
    let mut indexed = 0u64;
    loop {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM entities e
             WHERE e.kind = 'book' AND {VISIBLE} AND ($1::uuid IS NULL OR e.id > $1)
             ORDER BY e.id LIMIT $2"
        ))
        .bind(after)
        .bind(BATCH)
        .fetch_all(&state.db)
        .await?;
        let Some(last) = rows.last() else { break };
        after = Some(last.id);
        let docs = docs_of(&state.db, rows).await?;
        indexed += docs.len() as u64;
        // Разбор текста — работа процессора: вне потоков async.
        rebuild = tokio::task::spawn_blocking(move || {
            rebuild.add(&docs)?;
            Ok::<_, IndexError>(rebuild)
        })
        .await??;
        if indexed % 50_000 < BATCH as u64 {
            tracing::info!(books = indexed, "books index rebuild progress");
        }
    }
    let total = tokio::task::spawn_blocking(move || rebuild.commit()).await??;
    Ok(total)
}

/// Дослать в индекс затронутые сущности: видимые книги из `ids` переиндексируются, остальные
/// (скрытые, удалённые, не книги) — удаляются из индекса, если были в нём.
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
         WHERE e.id = ANY($1) AND e.kind = 'book' AND {VISIBLE} ORDER BY e.id"
    ))
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    // Среди затронутых нет книг и индекс их не знает — писателя не открываем: правка фильма не
    // должна стоить коммита в книжный индекс. Удалённую книгу отличить от фильма нельзя (строки
    // уже нет), поэтому при пустой выборке спрашиваем индекс.
    let docs = docs_of(&state.db, rows).await?;
    let ids = ids.to_vec();
    tokio::task::spawn_blocking(move || {
        if docs.is_empty() && !index.has_any(&ids)? {
            return Ok(());
        }
        index.update(&ids, &docs)
    })
    .await??;
    Ok(())
}

/// При старте сервера: индекс включён, но пуст (первый запуск, сменилась схема) — построить в
/// фоне. Поиск книг до конца сборки отвечает пустой выдачей.
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
            Ok(index) => {
                tracing::info!(books = index.len(), "books index opened");
                // Списки раздела — заранее: первый посетитель не ждёт сортировки всех книг.
                let warmed = tokio::task::spawn_blocking(move || index.warm()).await;
                if let Ok(Err(error)) = warmed {
                    tracing::warn!(%error, "books index warm-up failed");
                }
            }
            Err(error) => tracing::warn!(%error, "books index cannot be opened"),
        }
    });
}
