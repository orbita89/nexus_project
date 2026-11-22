//! Архив книг: книги, которым не место в каталоге (без обложки, крошечный файл, не книга), —
//! в отдельный индекс Tantivy холодного архива, строки из PostgreSQL удаляются. Как архив
//! фильмов (`catalog::archive`), но:
//!
//! - индекс свой — `books-archive` рядом с `BOOKS_INDEX_DIR` (роадмап: индекс на тип,
//!   `<kind>_archive`); у индекса фильмов старая схема без `ext_id` / `payload`;
//! - книги нет в TMDB, поэтому документ хранит полный снимок (`payload`: строка `entities`,
//!   участники, теги, причина) — возврат ([`restore`]) без потерь;
//! - `ext_id = cassandra:<номер>` — импорт библиотеки (`books::cassandra`) архивные книги
//!   обратно не загружает.
//!
//! Порядок переноса как у фильмов: сначала документы в архив (коммит), потом удаление из БД
//! (`archive::pipeline::delete_entities`: книга, участия, теги, авторы без других работ); не
//! вышло удалить — документы убираются из архива. Книги, нужные пользователям (интересы,
//! рецензии, коллекции…), не переносятся.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use shared::AppState;
use uuid::Uuid;

use super::BoxError;
use crate::archive::index::{Archive, ArchiveDoc, ArchiveError};
use crate::models::EntityKind;

/// Префикс внешнего id книг библиотеки Cassandra.
const CASSANDRA: &str = "cassandra:";

/// Каталог индекса архива книг: `books-archive` рядом с `BOOKS_INDEX_DIR`.
pub fn dir(state: &AppState) -> Option<PathBuf> {
    let books = state.config.books_index_dir.as_deref()?;
    Some(Path::new(books).with_file_name("books-archive"))
}

/// Индекс архива книг. `None` — книжный поиск выключен (`BOOKS_INDEX_DIR` не задан).
pub fn open(state: &AppState) -> Result<Option<Arc<Archive>>, ArchiveError> {
    dir(state).map(|dir| Archive::open(&dir)).transpose()
}

/// Номера Cassandra книг архива: их импорт пропускает.
pub fn archived_cassandra_ids(state: &AppState) -> Result<HashSet<i64>, BoxError> {
    let Some(archive) = open(state)? else {
        return Ok(HashSet::new());
    };
    Ok(archive
        .ext_ids(CASSANDRA)?
        .into_iter()
        .filter_map(|id| id.parse().ok())
        .collect())
}

/// Участие человека в книге — в снимке.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
struct Credit {
    entity_id: Uuid,
    role: String,
    character_name: Option<String>,
    position: i32,
    slug: String,
    full_name: String,
    original_name: Option<String>,
}

/// Снимок книги в `payload`.
#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    /// Строка `entities` целиком (`to_jsonb(e)`).
    entity: serde_json::Value,
    credits: Vec<Credit>,
    /// Slug'и тегов и жанров.
    tags: Vec<String>,
    /// Почему в архиве (`no_cover`, `small_file`, `not_a_book:article`…).
    reason: String,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    slug: String,
    title: String,
    original_title: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    cassandra: Option<String>,
    size: Option<i64>,
    entity: serde_json::Value,
}

/// Итог переноса.
#[derive(Debug, Default)]
pub struct Demoted {
    pub archived: u64,
    /// Нужны пользователям — остались в каталоге.
    pub in_use: u64,
    /// Нет в БД (уже удалены) или не книги.
    pub missing: u64,
    pub people_deleted: u64,
}

/// Перенести книги `books` (id и причина) в архив книг.
pub async fn demote(state: &AppState, books: &[(Uuid, String)]) -> Result<Demoted, BoxError> {
    let archive = open(state)?.ok_or("BOOKS_INDEX_DIR is not set")?;
    let mut out = Demoted::default();
    let ids: Vec<Uuid> = books.iter().map(|b| b.0).collect();
    let reasons: HashMap<Uuid, &str> = books.iter().map(|(id, r)| (*id, r.as_str())).collect();
    let in_use = state.interests.entities_in_use(&ids).await?;
    out.in_use = in_use.len() as u64;
    let ids: Vec<Uuid> = ids.into_iter().filter(|id| !in_use.contains(id)).collect();
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT e.id, e.slug, e.title, e.original_title, e.release_date, e.cover_url,
                e.external_ids ->> 'cassandra' AS cassandra,
                (e.metadata -> 'file' ->> 'size')::bigint AS size, to_jsonb(e) AS entity
         FROM entities e WHERE e.id = ANY($1) AND e.kind = 'book'",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    out.missing = (ids.len() - rows.len()) as u64;
    if rows.is_empty() {
        return Ok(out);
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let credits: Vec<Credit> = sqlx::query_as(
        "SELECT ec.entity_id, ec.role, ec.character_name, ec.position, p.slug, p.full_name,
                p.original_name
         FROM entity_credits ec JOIN people p ON p.id = ec.person_id
         WHERE ec.entity_id = ANY($1) ORDER BY ec.entity_id, ec.position",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    let tags: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT et.entity_id, t.slug FROM entity_tags et JOIN tags t ON t.id = et.tag_id
         WHERE et.entity_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    let mut credits_of: HashMap<Uuid, Vec<Credit>> = HashMap::new();
    for credit in credits {
        credits_of.entry(credit.entity_id).or_default().push(credit);
    }
    let mut tags_of: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (id, slug) in tags {
        tags_of.entry(id).or_default().push(slug);
    }
    let mut docs = Vec::with_capacity(rows.len());
    for row in rows {
        let snapshot = Snapshot {
            entity: row.entity,
            credits: credits_of.remove(&row.id).unwrap_or_default(),
            tags: tags_of.remove(&row.id).unwrap_or_default(),
            reason: reasons
                .get(&row.id)
                .copied()
                .unwrap_or("manual")
                .to_string(),
        };
        docs.push(ArchiveDoc {
            slug: row.slug,
            title: row.title,
            original_title: row.original_title,
            kind: EntityKind::Book,
            tmdb_id: None,
            release_date: row.release_date,
            // У книг нет TMDB popularity: размер файла — грубая мера «солидности».
            popularity: row.size.unwrap_or(0) as f64,
            cover_url: row.cover_url,
            manual: true,
            ext_id: row.cassandra.map(|id| format!("{CASSANDRA}{id}")),
            payload: Some(serde_json::to_string(&snapshot)?),
        });
    }

    // Сначала архив (коммит), потом удаление из БД.
    let slugs: Vec<String> = docs.iter().map(|d| d.slug.clone()).collect();
    let writer = {
        let archive = archive.clone();
        tokio::task::spawn_blocking(move || -> Result<_, ArchiveError> {
            let mut writer = archive.writer()?;
            for doc in &docs {
                archive.upsert(&writer, doc)?;
            }
            writer.commit()?;
            archive.reload()?;
            Ok(writer)
        })
        .await??
    };
    let deleted = async {
        let mut tx = state.db.begin().await?;
        let counts = crate::archive::pipeline::delete_entities(
            &mut tx,
            state.interests.as_ref(),
            &ids,
            true,
        )
        .await?;
        tx.commit().await?;
        Ok::<_, BoxError>(counts)
    }
    .await;
    match deleted {
        Ok((entities, people)) => {
            out.archived = entities;
            out.people_deleted = people;
            drop(writer);
        }
        Err(error) => {
            // БД не удалила — документы из архива обратно убираются.
            let archive = archive.clone();
            tokio::task::spawn_blocking(move || -> Result<(), ArchiveError> {
                let mut writer = writer;
                for slug in &slugs {
                    archive.delete(&writer, slug);
                }
                writer.commit()?;
                archive.reload()
            })
            .await??;
            return Err(error);
        }
    }
    // Из книжного индекса: удалённых книг в БД нет — досылка уберёт их документы.
    super::sync::update(state, &ids).await?;
    Ok(out)
}

/// Вернуть книгу из архива в каталог: строка `entities` как была, участники (люди по slug,
/// нет — создаются), теги, что ещё есть. Возвращает id книги.
pub async fn restore(state: &AppState, slug: &str) -> Result<Uuid, BoxError> {
    let archive = open(state)?.ok_or("BOOKS_INDEX_DIR is not set")?;
    let doc = {
        let (archive, slug) = (archive.clone(), slug.to_string());
        tokio::task::spawn_blocking(move || archive.get(&slug)).await??
    }
    .ok_or("no such book in the books archive")?;
    let snapshot: Snapshot = serde_json::from_str(
        doc.payload
            .as_deref()
            .ok_or("archive document has no payload")?,
    )?;
    let mut tx = state.db.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO entities SELECT (jsonb_populate_record(NULL::entities, $1)).* RETURNING id",
    )
    .bind(&snapshot.entity)
    .fetch_one(&mut *tx)
    .await?;
    for credit in &snapshot.credits {
        let person: Uuid = sqlx::query_scalar(
            "WITH found AS (SELECT id FROM people WHERE slug = $1),
                  made AS (INSERT INTO people (slug, full_name, original_name)
                           SELECT $1, $2, $3 WHERE NOT EXISTS (SELECT 1 FROM found)
                           RETURNING id)
             SELECT id FROM found UNION ALL SELECT id FROM made",
        )
        .bind(&credit.slug)
        .bind(&credit.full_name)
        .bind(&credit.original_name)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(person)
        .bind(&credit.role)
        .bind(&credit.character_name)
        .bind(credit.position)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO entity_tags (entity_id, tag_id)
         SELECT $1, t.id FROM tags t WHERE t.slug = ANY($2) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(&snapshot.tags)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let slug = slug.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), ArchiveError> {
        let mut writer = archive.writer()?;
        archive.delete(&writer, &slug);
        writer.commit()?;
        archive.reload()
    })
    .await??;
    super::sync::update(state, &[id]).await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = Snapshot {
            entity: serde_json::json!({"id": Uuid::nil(), "slug": "kniga", "kind": "book"}),
            credits: vec![Credit {
                entity_id: Uuid::nil(),
                role: "author".into(),
                character_name: None,
                position: 0,
                slug: "avtor".into(),
                full_name: "Автор".into(),
                original_name: None,
            }],
            tags: vec!["roman".into()],
            reason: "no_cover".into(),
        };
        let back: Snapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(back.credits[0].slug, "avtor");
        assert_eq!(back.reason, "no_cover");
    }
}
