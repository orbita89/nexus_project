//! Единый индекс ← PostgreSQL: слой А (Tantivy, текст) и слой Б (RAM, фасеты и порядок).
//!
//! - [`rebuild`] — сущности (все виды или перечисленные) и/или люди пачками (keyset по id) в
//!   индекс одним коммитом (до него поиск читает прежние документы; гард
//!   `SEARCH_REINDEX_MIN_PERCENT` срабатывает до коммита) и тем же проходом — слой Б. Запускается
//!   кнопкой в админке или `nexus search rebuild`; **автоматически не запускается никогда**.
//! - [`reload_sidecar`] — только слой Б: тот же проход по БД без записи в Tantivy. Раз в сутки,
//!   после [`update`] (не чаще [`SIDECAR_MIN_INTERVAL`]) и при старте, если снимка нет.
//! - [`update`] / [`update_people`] — затронутые сущности и люди после правки в админке и новых
//!   оценок Nexus. В Tantivy пишутся только документы, у которых изменился текст
//!   ([`TextDoc::text_hash`]); скрытые и удалённые — удаляются из него сразу.
//! - [`apply_ranks`] — ручные места из `catalog_manual_ranks` в слой Б за миллисекунды.
//!
//! Документ строится целиком из БД, поэтому повтор любого шага безопасен. Номера выдают
//! `catalog_entity_ords` и `catalog_person_ords` один раз.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};
use shared::AppState;
use tokio::sync::Notify;
use uuid::Uuid;

use super::collect::Space;
use super::people_sidecar::{PeopleBuilder, PersonFacts};
use super::sidecar::SidecarBuilder;
use super::types::{BookForm, EntityDoc, TextDoc};
use super::{CatalogIndex, IndexError, Rebuild, Scope, PERSON_KIND};
use crate::jobs::{Reporter, Step};
use crate::models::{EntityKind, ReleaseStatus, SummaryAuthor, VISIBLE};
use crate::people_search::{
    PersonDoc, DOC_COLUMNS, INDEXED_PERSON, WORKS_LATERAL, WORKS_TEMP_JOIN,
};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Сколько документов читать из БД и добавлять в индекс за раз.
const BATCH: i64 = 2000;
/// Пересборка слоя Б после правок — не чаще: поток новых оценок не должен превращаться в
/// непрерывное чтение всех сущностей.
pub const SIDECAR_MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Полная пересборка слоя Б по расписанию.
pub const SIDECAR_DAILY: Duration = Duration::from_secs(24 * 60 * 60);
/// Как часто проверять, не поменялся ли ручной порядок.
pub const RANKS_POLL: Duration = Duration::from_secs(60);
/// Верхушка состава у фильмов, игр и картин, попадающая в текстовое поле `people`.
const TOP_CREDITS: i32 = 6;

/// Что перестраивает [`rebuild`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Сущности перечисленных видов (слой Б пересобирается после).
    Kinds(Vec<EntityKind>),
    People,
    All,
}

impl Target {
    /// `book`, `movie` (`film`), `series`, `game`, `painting`, `people`, `all`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "people" | "person" => Some(Self::People),
            "film" => Some(Self::Kinds(vec![EntityKind::Movie])),
            other => match EntityKind::parse(other) {
                Some(EntityKind::Season) | None => None,
                Some(kind) => Some(Self::Kinds(vec![kind])),
            },
        }
    }
}

/// Ошибка перестройки: новый индекс слишком мал (не применён) или сбой.
#[derive(Debug)]
pub enum BuildError {
    TooSmall { new: u64, before: u64 },
    Other(BoxError),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooSmall { new, before } => {
                write!(f, "new index has {new} documents (was {before}): too small")
            }
            Self::Other(error) => write!(f, "{error}"),
        }
    }
}

impl<E: Into<BoxError>> From<E> for BuildError {
    fn from(error: E) -> Self {
        Self::Other(error.into())
    }
}

/// Индекс включён: задан `SEARCH_INDEX_DIR`.
pub fn enabled(state: &AppState) -> bool {
    state.config.search_index_dir.is_some()
}

/// Каталог индекса: `<SEARCH_INDEX_DIR>/catalog`.
pub fn dir(state: &AppState) -> Option<PathBuf> {
    state
        .config
        .search_index_dir
        .as_deref()
        .map(|root| Path::new(root).join("catalog"))
}

/// Индекс каталога. `None` — поиск выключен.
/// Индекс только читается ([`shared::Config::search_index_readonly`]).
pub fn readonly(state: &AppState) -> bool {
    state.config.search_index_readonly
}

/// Ошибка записи в индекс, открытый только для чтения.
pub const READONLY: &str = "search index is read-only on this server (SEARCH_INDEX_READONLY): \
                            build it locally and upload the files";

pub fn open(state: &AppState) -> Option<Result<Arc<CatalogIndex>, IndexError>> {
    let dir = dir(state)?;
    // Прод (только чтение) индекс не строит: чужая схема — ошибка, а не удаление файлов и пустой
    // индекс (так `CatalogIndex::open` поступает при сборке).
    if readonly(state) {
        let found = std::fs::read_to_string(dir.join(shared::tantivy::SCHEMA_FILE)).unwrap_or_default();
        if found.trim() != super::schema::SCHEMA_VERSION {
            return Some(Err(IndexError::from(tantivy::TantivyError::InvalidArgument(format!(
                "catalog index schema is {:?}, this build needs {:?}: upload an index built by this version",
                found.trim(),
                super::schema::SCHEMA_VERSION
            )))));
        }
    }
    Some(CatalogIndex::open(&dir))
}

/// Слой Б устарел (правка): фоновая задача [`spawn_initial`] пересоберёт его.
fn dirty() -> &'static Notify {
    static DIRTY: OnceLock<Notify> = OnceLock::new();
    DIRTY.get_or_init(Notify::new)
}

fn people_dirty() -> &'static Notify {
    static DIRTY: OnceLock<Notify> = OnceLock::new();
    DIRTY.get_or_init(Notify::new)
}

fn hidden_dirty() -> &'static Notify {
    static DIRTY: OnceLock<Notify> = OnceLock::new();
    DIRTY.get_or_init(Notify::new)
}

/// Свежесть индекса на проде (только чтение): скрытые и удалённые после сборки — это номера,
/// которые есть в индексе, но не видимы в БД. Один запрос номеров видимых (~700 тыс. чисел,
/// десятки мс), в памяти — битовая карта. Новая выкладка индекса — список считается заново.
pub async fn refresh_hidden(state: &AppState) -> Result<u64, BoxError> {
    let index = open(state).ok_or("SEARCH_INDEX_DIR is not set")??;
    let visible: Vec<i32> = sqlx::query_scalar(&format!(
        "SELECT o.ord FROM catalog_entity_ords o JOIN entities e ON e.id = o.entity_id
         WHERE {INDEXED} AND {VISIBLE}"
    ))
    .fetch_all(&state.db)
    .await?;
    let mut hidden = tokio::task::spawn_blocking({
        let index = index.clone();
        move || index.indexed_entities()
    })
    .await??;
    let visible: roaring::RoaringBitmap = visible
        .into_iter()
        .filter_map(|o| u32::try_from(o).ok())
        .collect();
    hidden -= &visible;
    let n = hidden.len();
    index.set_hidden(hidden);
    Ok(n)
}

/// Прод: список скрытых при старте и после каждой правки (пачка правок — один пересчёт).
async fn hidden_loop(state: AppState) {
    loop {
        match refresh_hidden(&state).await {
            Ok(n) => tracing::info!(hidden = n, "catalog index: hidden since build"),
            Err(error) => tracing::warn!(%error, "catalog index: hidden list not refreshed"),
        }
        hidden_dirty().notified().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Правки, пришедшие во время перестройки (писатель занят): повторяются после неё.
#[derive(Default)]
struct Journal {
    entities: Vec<Uuid>,
    people: Vec<Uuid>,
}

static JOURNAL: Mutex<Option<Journal>> = Mutex::new(None);

// ------------------------------------------------------------------ сущности

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    kind: EntityKind,
    slug: String,
    title: String,
    original_title: Option<String>,
    release_date: Option<NaiveDate>,
    #[sqlx(try_from = "String")]
    release_status: ReleaseStatus,
    cover_url: Option<String>,
    cover_width: Option<i32>,
    cover_height: Option<i32>,
    has_description: bool,
    form: String,
    /// Нужные поиску ключи `metadata`: издатель, серия, язык, музей, студии, страны.
    meta: serde_json::Value,
    imdb_rating: Option<f64>,
    imdb_votes: Option<i32>,
    tmdb_popularity: Option<f64>,
    quality: Option<f64>,
    rank_score: Option<f64>,
}

/// Что индексируется: всё видимое, кроме сезонов и частей книг.
const INDEXED: &str = "e.kind <> 'season' AND (e.kind <> 'book' OR e.parent_id IS NULL)";

const COLUMNS: &str = concat!(
    "e.id, e.kind, e.slug, e.title, e.original_title, e.release_date, e.cover_url, \
     e.cover_width, e.cover_height, ",
    "(CASE WHEN e.release_date <= current_date THEN 'released' \
      WHEN e.release_date <= current_date + (SELECT sr.soon_days FROM catalog_ranking sr) \
      THEN 'soon' ELSE 'announced' END) AS release_status, ",
    "coalesce(e.description <> '', false) AS has_description, \
     CASE WHEN e.metadata ->> 'kind' IN ('article', 'document') THEN e.metadata ->> 'kind' \
          WHEN e.metadata ? 'collection' THEN 'collection' ELSE 'book' END AS form, \
     jsonb_strip_nulls(jsonb_build_object('publisher', e.metadata -> 'publisher', \
         'developer', e.metadata -> 'developer', 'series', e.metadata -> 'series', \
         'language', e.metadata -> 'language', 'museum', e.metadata -> 'museum', \
         'studios', e.metadata -> 'studios', 'countries', e.metadata -> 'countries')) AS meta, \
     es.imdb_rating, es.imdb_votes, es.tmdb_popularity, es.quality, es.rank_score"
);

const FROM: &str = "FROM entities e LEFT JOIN entity_stats es ON es.entity_id = e.id";

fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
    match &value[key] {
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        serde_json::Value::String(s) if !s.trim().is_empty() => vec![s.trim().to_owned()],
        _ => Vec::new(),
    }
}

/// Номера `ids` (выдаются тем, у кого их ещё нет) и отпечатки их текста в индексе. Вставляются
/// только недостающие: `ON CONFLICT` тратил бы номер на каждую известную сущность, и массивы
/// слоя Б росли бы дырами.
async fn entity_ords(
    db: &sqlx::PgPool,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, (u32, Option<i64>)>, sqlx::Error> {
    sqlx::query(
        "INSERT INTO catalog_entity_ords (entity_id)
         SELECT id FROM unnest($1::uuid[]) AS id
         WHERE NOT EXISTS (SELECT 1 FROM catalog_entity_ords o WHERE o.entity_id = id)
         ON CONFLICT (entity_id) DO NOTHING",
    )
    .bind(ids)
    .execute(db)
    .await?;
    let rows: Vec<(Uuid, i32, Option<i64>)> = sqlx::query_as(
        "SELECT entity_id, ord, text_hash FROM catalog_entity_ords WHERE entity_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, ord, hash)| Some((id, (u32::try_from(ord).ok()?, hash))))
        .collect())
}

/// Документы пачки сущностей: номера, участники, теги, оценки, студии, награды и ручные места —
/// по одному запросу на пачку. Второе значение — отпечатки текста, уже записанного в индекс.
async fn entity_docs(
    db: &sqlx::PgPool,
    rows: Vec<Row>,
) -> Result<(Vec<EntityDoc>, HashMap<Uuid, i64>), sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let ords = entity_ords(db, &ids).await?;
    // Весь состав — термы фильтра `person`/`role` у всех видов (поиск не спрашивает БД); в текст
    // имён идёт только верхушка титров (`top`). `active_credits` — сколько видимых работ у
    // человека: известность автора для ранга.
    let credits: Vec<(Uuid, String, String, String, i32, bool)> = sqlx::query_as(
        "SELECT ec.entity_id, ec.role, p.slug, p.full_name, coalesce(pc.active_credits, 0),
                ec.position < $2
         FROM entity_credits ec JOIN people p ON p.id = ec.person_id
         LEFT JOIN person_credit_counts pc ON pc.person_id = p.id
         WHERE ec.entity_id = ANY($1)
         ORDER BY ec.entity_id, ec.position",
    )
    .bind(&ids)
    .bind(TOP_CREDITS)
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
    let studio_names: Vec<String> = rows
        .iter()
        .flat_map(|row| strings(&row.meta, "studios"))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let studio_slugs: HashMap<String, String> =
        sqlx::query_as("SELECT name, slug FROM studios WHERE name = ANY($1)")
            .bind(&studio_names)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    let mut awards = crate::awards::facets_of(db, &ids).await?;

    let mut hashes = HashMap::new();
    let mut docs: HashMap<Uuid, EntityDoc> = rows
        .into_iter()
        .filter_map(|row| {
            let (ord, hash) = *ords.get(&row.id)?;
            if let Some(hash) = hash {
                hashes.insert(row.id, hash);
            }
            let book = row.kind == EntityKind::Book;
            let studios = strings(&row.meta, "studios");
            let mut publishers: Vec<String> = Vec::new();
            for key in ["publisher", "developer"] {
                for value in strings(&row.meta, key) {
                    if !publishers.contains(&value) {
                        publishers.push(value);
                    }
                }
            }
            let awards = awards.remove(&row.id).unwrap_or_default();
            let doc = EntityDoc {
                id: row.id,
                ord,
                kind: row.kind,
                slug: row.slug,
                title: row.title,
                original_title: row.original_title,
                release_date: row.release_date,
                release_status: row.release_status,
                cover_url: row.cover_url,
                cover_width: row.cover_width,
                cover_height: row.cover_height,
                imdb_rating: row.imdb_rating,
                imdb_votes: row.imdb_votes,
                // Серия книги и студии — самое слабое текстовое поле.
                extra: if book {
                    strings(&row.meta, "series")
                } else {
                    studios.clone()
                },
                has_description: row.has_description,
                // В БД коды стран в верхнем регистре, но индекс строится и по старым строкам.
                countries: strings(&row.meta, "countries")
                    .into_iter()
                    .map(|code| code.to_ascii_uppercase())
                    .collect(),
                studio_slugs: studios
                    .iter()
                    .filter_map(|name| studio_slugs.get(name).cloned())
                    .collect(),
                studios,
                publishers,
                museums: strings(&row.meta, "museum"),
                language: strings(&row.meta, "language").into_iter().next(),
                form: BookForm::parse(&row.form).unwrap_or_default(),
                award_slugs: awards.award_slugs,
                award_years: awards.award_years,
                award_wins: awards.award_wins,
                rank_score: row.rank_score,
                popularity: row.tmdb_popularity,
                quality: row.quality,
                ..EntityDoc::default()
            };
            Some((row.id, doc))
        })
        .collect();
    for (id, role, slug, name, works, top) in credits {
        let Some(doc) = docs.get_mut(&id) else {
            continue;
        };
        if doc.kind == EntityKind::Book {
            if role == "author" {
                doc.author_works = doc.author_works.max(works.max(0) as u32);
                doc.authors.push(SummaryAuthor {
                    slug: slug.clone(),
                    name: name.clone(),
                });
                doc.people.push(name);
            }
            doc.credits.push((role, slug));
        } else {
            if top && !doc.people.contains(&name) {
                doc.people.push(name);
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
    // В порядке id, как прочитали: выдача при равных ключах не зависит от HashMap.
    let docs = ids.iter().filter_map(|id| docs.remove(id)).collect();
    Ok((docs, hashes))
}

/// Запомнить отпечатки текста, записанного в индекс (`None` — документа в индексе больше нет).
async fn save_hashes(
    db: &sqlx::PgPool,
    table: &str,
    key: &str,
    hashes: &[(Uuid, Option<i64>)],
) -> Result<(), sqlx::Error> {
    for chunk in hashes.chunks(10_000) {
        let ids: Vec<Uuid> = chunk.iter().map(|h| h.0).collect();
        let values: Vec<Option<i64>> = chunk.iter().map(|h| h.1).collect();
        sqlx::query(&format!(
            "UPDATE {table} o SET text_hash = v.hash
             FROM unnest($1::uuid[], $2::bigint[]) AS v(id, hash)
             WHERE o.{key} = v.id AND o.text_hash IS DISTINCT FROM v.hash"
        ))
        .bind(&ids)
        .bind(&values)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Видимые сущности пачками по [`BATCH`] (keyset по id); `kinds` — только эти виды.
struct Pages {
    after: Option<Uuid>,
    seen: u64,
    kinds: Vec<String>,
}

impl Pages {
    fn new(kinds: Option<&[EntityKind]>) -> Self {
        Self {
            after: None,
            seen: 0,
            kinds: kinds
                .map(|kinds| kinds.iter().map(|k| k.as_str().to_owned()).collect())
                .unwrap_or_default(),
        }
    }

    /// Следующая пачка: документы и отпечатки их текста в индексе. `None` — сущности кончились.
    async fn next(
        &mut self,
        db: &sqlx::PgPool,
    ) -> Result<Option<(Vec<EntityDoc>, HashMap<Uuid, i64>)>, sqlx::Error> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} {FROM}
             WHERE {INDEXED} AND {VISIBLE} AND (cardinality($3::text[]) = 0 OR e.kind::text = ANY($3))
               AND ($1::uuid IS NULL OR e.id > $1)
             ORDER BY e.id LIMIT $2"
        ))
        .bind(self.after)
        .bind(BATCH)
        .bind(&self.kinds)
        .fetch_all(db)
        .await?;
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        self.after = Some(last.id);
        let page = entity_docs(db, rows).await?;
        self.seen += page.0.len() as u64;
        if self.seen % 50_000 < BATCH as u64 {
            tracing::info!(
                entities = self.seen,
                "catalog entities read from the database"
            );
        }
        Ok(Some(page))
    }
}

// --------------------------------------------------------------------- люди

/// Номера людей `ids` и отпечатки их текста в индексе.
async fn person_ords(
    db: &sqlx::PgPool,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, (u32, Option<i64>)>, sqlx::Error> {
    sqlx::query(
        "INSERT INTO catalog_person_ords (person_id)
         SELECT id FROM unnest($1::uuid[]) AS id
         WHERE NOT EXISTS (SELECT 1 FROM catalog_person_ords o WHERE o.person_id = id)
         ON CONFLICT (person_id) DO NOTHING",
    )
    .bind(ids)
    .execute(db)
    .await?;
    let rows: Vec<(Uuid, i32, Option<i64>)> = sqlx::query_as(
        "SELECT person_id, ord, text_hash FROM catalog_person_ords WHERE person_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, ord, hash)| Some((id, (u32::try_from(ord).ok()?, hash))))
        .collect())
}

/// Слой А и Б человека.
struct PersonParts {
    id: Uuid,
    text: TextDoc,
    facts: PersonFacts,
}

fn person_parts(doc: &PersonDoc, ord: u32) -> Result<PersonParts, serde_json::Error> {
    Ok(PersonParts {
        id: doc.id,
        text: TextDoc {
            kind: PERSON_KIND,
            ord: Space::Person.raw(ord),
            titles: std::iter::once(doc.full_name.clone())
                .chain(doc.original_name.clone())
                .collect(),
            people: Vec::new(),
            extra: doc.known_for.iter().cloned().collect(),
            facets: Vec::new(),
            card: serde_json::to_string(doc)?,
        },
        facts: PersonFacts {
            ord,
            name: doc.full_name.clone(),
            known_for: doc.known_for.clone(),
            fame: doc.fame,
        },
    })
}

async fn parts_of(
    db: &sqlx::PgPool,
    docs: Vec<PersonDoc>,
) -> Result<(Vec<PersonParts>, HashMap<Uuid, i64>), BoxError> {
    let ids: Vec<Uuid> = docs.iter().map(|d| d.id).collect();
    let ords = person_ords(db, &ids).await?;
    let mut hashes = HashMap::new();
    let mut parts = Vec::with_capacity(docs.len());
    for doc in &docs {
        let Some((ord, hash)) = ords.get(&doc.id).copied() else {
            continue;
        };
        if let Some(hash) = hash {
            hashes.insert(doc.id, hash);
        }
        parts.push(person_parts(doc, ord)?);
    }
    Ok((parts, hashes))
}

/// Только слой Б людей из БД: имя, роль и известность (без `works_count` и `top_works`).
async fn load_people_facts(db: &sqlx::PgPool) -> Result<Vec<PersonFacts>, sqlx::Error> {
    let rows: Vec<(i32, String, Option<String>, f64)> = sqlx::query_as(&format!(
        "SELECT o.ord, p.full_name, ps.known_for, coalesce(ps.fame, 0)
         FROM people p
         JOIN catalog_person_ords o ON o.person_id = p.id
         LEFT JOIN person_stats ps ON ps.person_id = p.id
         WHERE {INDEXED_PERSON}"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(ord, name, known_for, fame)| {
            Some(PersonFacts {
                ord: u32::try_from(ord).ok()?,
                name,
                known_for,
                fame,
            })
        })
        .collect())
}

// ------------------------------------------------------------- перестройка

/// Последняя сборка: когда закончилась, сколько шла, сколько документов.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    pub at: DateTime<Utc>,
    pub took_ms: u64,
    pub docs: u64,
}

static LAST_SIDECAR: Mutex<Option<BuildInfo>> = Mutex::new(None);
static LAST_INDEX: Mutex<Option<BuildInfo>> = Mutex::new(None);

fn remember(slot: &Mutex<Option<BuildInfo>>, started: Instant, docs: u64) {
    *slot.lock().unwrap() = Some(BuildInfo {
        at: Utc::now(),
        took_ms: started.elapsed().as_millis() as u64,
        docs,
    });
}

/// Последние сборки в этом процессе: слоя Б и индекса (`None` — с запуска не было).
pub fn last_builds() -> (Option<BuildInfo>, Option<BuildInfo>) {
    (*LAST_SIDECAR.lock().unwrap(), *LAST_INDEX.lock().unwrap())
}

/// Гард: новых документов не меньше `percent`% прежних (прежних нет — всегда можно).
fn guard(rebuild: &Rebuild, percent: u64, force: bool) -> Result<(), BuildError> {
    let (before, new) = (rebuild.before(), rebuild.added());
    if !force && before > 0 && new * 100 < before * percent {
        return Err(BuildError::TooSmall { new, before });
    }
    Ok(())
}

/// Перестройка из БД; `force` — применить, даже если новый индекс меньше
/// `SEARCH_REINDEX_MIN_PERCENT`% прежнего. Возвращает число документов (сущностей и людей).
/// Идёт другая запись — `Busy`.
pub async fn rebuild(
    state: &AppState,
    target: Target,
    report: &Reporter,
    force: bool,
) -> Result<u64, BuildError> {
    if readonly(state) {
        return Err(BuildError::from(BoxError::from(READONLY)));
    }
    let index = open(state).ok_or("SEARCH_INDEX_DIR is not set")??;
    *JOURNAL.lock().unwrap() = Some(Journal::default());
    let result = build(state, &index, &target, report, force).await;
    let journal = JOURNAL.lock().unwrap().take().unwrap_or_default();
    let built = result?;
    if !journal.entities.is_empty() {
        update(state, &journal.entities).await?;
    }
    if !journal.people.is_empty() {
        update_people(state, &journal.people).await?;
    }
    Ok(built)
}

async fn build(
    state: &AppState,
    index: &Arc<CatalogIndex>,
    target: &Target,
    report: &Reporter,
    force: bool,
) -> Result<u64, BuildError> {
    let started = Instant::now();
    let percent = state.config.search_reindex_min_percent;
    let (kinds, with_entities, with_people): (Vec<EntityKind>, bool, bool) = match target {
        Target::All => (Vec::new(), true, true),
        Target::People => (Vec::new(), false, true),
        Target::Kinds(kinds) => (kinds.clone(), true, false),
    };
    let scope = match target {
        Target::All => Scope::All,
        Target::People => Scope::Kinds(vec![PERSON_KIND]),
        Target::Kinds(kinds) => Scope::Kinds(kinds.iter().map(|k| k.as_str()).collect()),
    };
    let mut writer = {
        let index = index.clone();
        tokio::task::spawn_blocking(move || index.rebuild(scope)).await??
    };
    let mut entities = SidecarBuilder::default();
    let mut people = PeopleBuilder::default();
    let mut entity_hashes: Vec<(Uuid, Option<i64>)> = Vec::new();
    let mut person_hashes: Vec<(Uuid, Option<i64>)> = Vec::new();

    if with_entities {
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM entities e
             WHERE {INDEXED} AND {VISIBLE}
               AND (cardinality($1::text[]) = 0 OR e.kind::text = ANY($1))"
        ))
        .bind(
            kinds
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect::<Vec<_>>(),
        )
        .fetch_one(&state.db)
        .await?;
        let total = u64::try_from(total).unwrap_or(0);
        report.progress(Step::Works, 0, total);
        let mut pages = Pages::new((!kinds.is_empty()).then_some(kinds.as_slice()));
        while let Some((docs, _)) = pages.next(&state.db).await? {
            let mut texts = Vec::with_capacity(docs.len());
            for doc in &docs {
                let text = doc.text();
                entity_hashes.push((doc.id, Some(text.text_hash())));
                texts.push(text);
                if matches!(target, Target::All) {
                    entities.add(doc);
                }
            }
            // Разбор текста — работа процессора: вне потоков async.
            writer = tokio::task::spawn_blocking(move || {
                writer.add(&texts)?;
                Ok::<_, IndexError>(writer)
            })
            .await??;
            report.progress(Step::Works, pages.seen, total.max(pages.seen));
        }
    }

    if with_people {
        report.progress(Step::People, 0, 0);
        crate::people_search::refresh_fame(&state.db, None).await?;
        let total = crate::people_search::count(&state.db).await?;
        report.progress(Step::People, 0, total);
        // Работы всех людей — одним проходом во временную таблицу выделенного соединения.
        let mut conn = state.db.acquire().await?;
        crate::people_search::create_works_temp(&mut conn).await?;
        let mut after: Option<Uuid> = None;
        let mut done = 0u64;
        loop {
            let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
                "SELECT {DOC_COLUMNS}
                 FROM (SELECT * FROM people p WHERE {INDEXED_PERSON}
                         AND ($1::uuid IS NULL OR p.id > $1) ORDER BY p.id LIMIT $2) p
                 LEFT JOIN person_stats ps ON ps.person_id = p.id
                 {WORKS_TEMP_JOIN}
                 ORDER BY p.id"
            ))
            .bind(after)
            .bind(BATCH)
            .fetch_all(&mut *conn)
            .await?;
            let Some(last) = docs.last() else { break };
            after = Some(last.id);
            let (parts, _) = parts_of(&state.db, docs).await?;
            let mut texts = Vec::with_capacity(parts.len());
            for part in parts {
                person_hashes.push((part.id, Some(part.text.text_hash())));
                people.add(&part.facts);
                texts.push(part.text);
            }
            done += texts.len() as u64;
            writer = tokio::task::spawn_blocking(move || {
                writer.add(&texts)?;
                Ok::<_, IndexError>(writer)
            })
            .await??;
            report.progress(Step::People, done, total.max(done));
        }
        // После ошибки выше таблицу уберёт `create_works_temp` следующей перестройки.
        crate::people_search::drop_works_temp(&mut conn).await?;
    }

    // Гард — до коммита: слишком малый индекс не заменяет прежний.
    if let Err(error) = guard(&writer, percent, force) {
        tokio::task::spawn_blocking(move || writer.rollback()).await??;
        return Err(error);
    }
    let total = tokio::task::spawn_blocking(move || writer.commit()).await??;

    // Документы, которых в новом индексе нет, — без отпечатка: появятся — будут записаны заново.
    if with_entities {
        sqlx::query(
            "UPDATE catalog_entity_ords o SET text_hash = NULL
             FROM entities e WHERE o.entity_id = e.id AND o.text_hash IS NOT NULL
               AND (cardinality($1::text[]) = 0 OR e.kind::text = ANY($1))",
        )
        .bind(
            kinds
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect::<Vec<_>>(),
        )
        .execute(&state.db)
        .await?;
        save_hashes(
            &state.db,
            "catalog_entity_ords",
            "entity_id",
            &entity_hashes,
        )
        .await?;
    }
    if with_people {
        sqlx::query("UPDATE catalog_person_ords SET text_hash = NULL WHERE text_hash IS NOT NULL")
            .execute(&state.db)
            .await?;
        save_hashes(
            &state.db,
            "catalog_person_ords",
            "person_id",
            &person_hashes,
        )
        .await?;
        let sidecar = people.finish();
        let index = index.clone();
        tokio::task::spawn_blocking(move || index.set_people(sidecar)).await??;
    }
    match target {
        Target::All => {
            let sidecar = entities.finish();
            let refs = load_refs(&state.db).await?;
            let index = index.clone();
            tokio::task::spawn_blocking(move || {
                index.set_refs(refs)?;
                index.set_entities(sidecar)
            })
            .await??;
            remember(&LAST_SIDECAR, started, total);
        }
        Target::Kinds(_) => {
            // Слой Б собирается по всем видам сразу: один проход по БД.
            reload_sidecar(state).await?;
        }
        Target::People => {}
    }
    remember(&LAST_INDEX, started, total);
    Ok(total)
}

/// Видимых сущностей в БД (тех, что должны быть в индексе), выданных номеров, видимых людей.
pub async fn db_counts(db: &sqlx::PgPool) -> Result<(i64, i64, i64), sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT (SELECT count(*) FROM entities e WHERE {INDEXED} AND {VISIBLE}),
                (SELECT count(*) FROM catalog_entity_ords),
                (SELECT count(*) FROM people p WHERE {INDEXED_PERSON})"
    ))
    .fetch_one(db)
    .await
}

/// Видимых сущностей данного вида в БД.
pub async fn db_kind_count(db: &sqlx::PgPool, kind: EntityKind) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM entities e WHERE {INDEXED} AND {VISIBLE} AND e.kind = $1"
    ))
    .bind(kind)
    .fetch_one(db)
    .await
}

/// Пересобрать только слой Б (фасеты, сортировки, места) из БД — Tantivy не трогается.
/// Возвращает число сущностей.
pub async fn reload_sidecar(state: &AppState) -> Result<u64, BoxError> {
    if readonly(state) {
        return Err(READONLY.into());
    }
    let index = open(state).ok_or("SEARCH_INDEX_DIR is not set")??;
    let started = Instant::now();
    let mut builder = SidecarBuilder::default();
    let mut pages = Pages::new(None);
    while let Some((docs, _)) = pages.next(&state.db).await? {
        for doc in &docs {
            builder.add(doc);
        }
    }
    let seen = pages.seen;
    let sidecar = builder.finish();
    let refs = load_refs(&state.db).await?;
    let people = {
        let mut people = PeopleBuilder::default();
        for facts in load_people_facts(&state.db).await? {
            people.add(&facts);
        }
        people.finish()
    };
    tokio::task::spawn_blocking(move || {
        index.set_refs(refs)?;
        index.set_entities(sidecar)?;
        index.set_people(people)
    })
    .await??;
    tracing::info!(entities = seen, took = ?started.elapsed(), "catalog sidecar rebuilt");
    remember(&LAST_SIDECAR, started, seen);
    Ok(seen)
}

/// Справочники студий и тегов для подсказок фильтров ([`super::refs`]): ответ поиска их не
/// ищет в БД.
async fn load_refs(db: &sqlx::PgPool) -> Result<super::refs::Refs, sqlx::Error> {
    let studios: Vec<super::refs::StudioRow> =
        sqlx::query_as("SELECT name, slug, logo_url FROM studios")
            .fetch_all(db)
            .await?;
    let tags: Vec<super::refs::TagRow> =
        sqlx::query_as("SELECT slug, name, kind, description FROM tags")
            .fetch_all(db)
            .await?;
    Ok(super::refs::Refs::build(studios, tags))
}

/// Ручные места сущностей: `ord → место`.
async fn load_ranks(db: &sqlx::PgPool) -> Result<HashMap<u32, u16>, sqlx::Error> {
    let rows: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT o.ord, r.rank FROM catalog_manual_ranks r
         JOIN catalog_entity_ords o ON o.entity_id = r.entity_id",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(ord, rank)| Some((u32::try_from(ord).ok()?, u16::try_from(rank).ok()?)))
        .collect())
}

/// Ручные места из БД — в слой Б (без пересборки фасетов). Поиск выключен — ничего.
/// Баллы «Рекомендуемые» из `entity_stats` в слой Б — только пары (номер, балл): секунды и
/// без Tantivy. После пересчёта баллов (`crate::rank_kinds`). Возвращает число баллов.
pub async fn apply_scores(state: &AppState) -> Result<u64, BoxError> {
    if readonly(state) {
        return Err(READONLY.into());
    }
    let Some(index) = open(state) else {
        return Ok(0);
    };
    let index = index?;
    let started = Instant::now();
    let rows: Vec<(i32, f64)> = sqlx::query_as(
        "SELECT o.ord, s.rank_score FROM catalog_entity_ords o
         JOIN entity_stats s ON s.entity_id = o.entity_id
         WHERE s.rank_score IS NOT NULL",
    )
    .fetch_all(&state.db)
    .await?;
    let scores: HashMap<u32, f64> = rows
        .into_iter()
        .filter_map(|(ord, score)| Some((u32::try_from(ord).ok()?, score)))
        .collect();
    let n = scores.len() as u64;
    tokio::task::spawn_blocking(move || index.set_scores(&scores)).await??;
    tracing::info!(scores = n, took = ?started.elapsed(), "catalog scores applied");
    Ok(n)
}

pub async fn apply_ranks(state: &AppState) -> Result<(), BoxError> {
    if readonly(state) {
        return Ok(());
    }
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

// ----------------------------------------------------------- точечные правки

/// `Busy` во время перестройки не ошибка: правка записана в журнал и повторится после неё.
fn deferred(result: Result<bool, IndexError>) -> Result<bool, BoxError> {
    match result {
        Err(IndexError::Busy) if JOURNAL.lock().unwrap().is_some() => Ok(false),
        other => Ok(other?),
    }
}

/// Дослать затронутые сущности: с изменившимся текстом — в Tantivy, скрытые и удалённые — из
/// него; слой Б пересобирается в фоне ([`spawn_initial`]).
pub async fn update(state: &AppState, ids: &[Uuid]) -> Result<(), BoxError> {
    if ids.is_empty() {
        return Ok(());
    }
    if readonly(state) {
        // Прод индекс не правит: скрытое и удалённое вычитается из выдачи ([`refresh_hidden`]).
        hidden_dirty().notify_one();
        return Ok(());
    }
    if let Some(journal) = JOURNAL.lock().unwrap().as_mut() {
        journal.entities.extend_from_slice(ids);
    }
    let Some(index) = open(state) else {
        return Ok(());
    };
    let index = index?;
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM}
         WHERE e.id = ANY($1) AND {INDEXED} AND {VISIBLE} ORDER BY e.id"
    ))
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    let (docs, stored) = entity_docs(&state.db, rows).await?;
    let present: HashSet<Uuid> = docs.iter().map(|doc| doc.id).collect();
    let gone: Vec<Uuid> = ids
        .iter()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    let gone_ords: Vec<(Uuid, u64)> = sqlx::query_as::<_, (Uuid, i32)>(
        "SELECT entity_id, ord FROM catalog_entity_ords WHERE entity_id = ANY($1)",
    )
    .bind(&gone)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter_map(|(id, ord)| Some((id, u64::from(u32::try_from(ord).ok()?))))
    .collect();
    let changed: Vec<(Uuid, TextDoc)> = docs
        .iter()
        .map(|doc| (doc.id, doc.text()))
        .filter(|(id, text)| stored.get(id) != Some(&text.text_hash()))
        .collect();
    let mut hashes: Vec<(Uuid, Option<i64>)> = changed
        .iter()
        .map(|(id, text)| (*id, Some(text.text_hash())))
        .collect();
    let had_entities = !present.is_empty();
    // Среди затронутых нет видимых и индекс их не знает — писателя не открываем: правка скрытой
    // сущности не должна стоить коммита.
    let texts: Vec<TextDoc> = changed.into_iter().map(|(_, text)| text).collect();
    let remove: Vec<u64> = gone_ords.iter().map(|(_, ord)| *ord).collect();
    let written = deferred(
        tokio::task::spawn_blocking(move || {
            let gone_indexed = !remove.is_empty() && index.has_any(&remove)?;
            if texts.is_empty() && !gone_indexed {
                return Ok(false);
            }
            let remove = if gone_indexed { remove } else { Vec::new() };
            index.update(&remove, &texts)?;
            Ok::<_, IndexError>(gone_indexed)
        })
        .await?,
    )?;
    if written {
        hashes.extend(gone_ords.iter().map(|(id, _)| (*id, None)));
    }
    save_hashes(&state.db, "catalog_entity_ords", "entity_id", &hashes).await?;
    if had_entities || written {
        dirty().notify_one();
    }
    Ok(())
}

/// Правка затронула людей: известность пересчитывает вызывающий
/// ([`crate::people_search::update_index`]); документы переписываются, скрытые и удалённые —
/// удаляются.
pub async fn update_people(state: &AppState, people: &[Uuid]) -> Result<(), BoxError> {
    if people.is_empty() || readonly(state) {
        return Ok(());
    }
    if let Some(journal) = JOURNAL.lock().unwrap().as_mut() {
        journal.people.extend_from_slice(people);
    }
    let Some(index) = open(state) else {
        return Ok(());
    };
    let index = index?;
    let docs: Vec<PersonDoc> = sqlx::query_as(&format!(
        "SELECT {DOC_COLUMNS}
         FROM people p LEFT JOIN person_stats ps ON ps.person_id = p.id
         {WORKS_LATERAL}
         WHERE p.id = ANY($1) AND {INDEXED_PERSON} ORDER BY p.id"
    ))
    .bind(people)
    .fetch_all(&state.db)
    .await?;
    let (parts, stored) = parts_of(&state.db, docs).await?;
    let present: HashSet<Uuid> = parts.iter().map(|part| part.id).collect();
    let gone: Vec<Uuid> = people
        .iter()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    let gone_ords: Vec<(Uuid, u64)> = sqlx::query_as::<_, (Uuid, i32)>(
        "SELECT person_id, ord FROM catalog_person_ords WHERE person_id = ANY($1)",
    )
    .bind(&gone)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter_map(|(id, ord)| Some((id, Space::Person.raw(u32::try_from(ord).ok()?))))
    .collect();
    let changed: Vec<(Uuid, TextDoc)> = parts
        .into_iter()
        .map(|part| (part.id, part.text))
        .filter(|(id, text)| stored.get(id) != Some(&text.text_hash()))
        .collect();
    let mut hashes: Vec<(Uuid, Option<i64>)> = changed
        .iter()
        .map(|(id, text)| (*id, Some(text.text_hash())))
        .collect();
    let texts: Vec<TextDoc> = changed.into_iter().map(|(_, text)| text).collect();
    let remove: Vec<u64> = gone_ords.iter().map(|(_, ord)| *ord).collect();
    let written = deferred(
        tokio::task::spawn_blocking(move || {
            let gone_indexed = !remove.is_empty() && index.has_any(&remove)?;
            if texts.is_empty() && !gone_indexed {
                return Ok(false);
            }
            let remove = if gone_indexed { remove } else { Vec::new() };
            index.update(&remove, &texts)?;
            Ok::<_, IndexError>(true)
        })
        .await?,
    )?;
    if written {
        hashes.extend(gone_ords.iter().map(|(id, _)| (*id, None)));
    }
    save_hashes(&state.db, "catalog_person_ords", "person_id", &hashes).await?;
    people_dirty().notify_one();
    Ok(())
}

/// Пересобрать только слой Б людей (быстро: без фильмографий).
async fn reload_people(state: &AppState) -> Result<(), BoxError> {
    if readonly(state) {
        return Ok(());
    }
    let index = open(state).ok_or("SEARCH_INDEX_DIR is not set")??;
    let mut people = PeopleBuilder::default();
    for facts in load_people_facts(&state.db).await? {
        people.add(&facts);
    }
    let sidecar = people.finish();
    tokio::task::spawn_blocking(move || index.set_people(sidecar)).await??;
    Ok(())
}

/// При старте сервера: индекс пуст (первый запуск, сменилась схема) — **не** строится сам, а
/// предупреждает: построить кнопкой в админке или `nexus search rebuild`. Нет снимка слоя Б —
/// собрать его из БД. Дальше — фоновая жизнь слоя Б: пересборка раз в сутки и после правок
/// (не чаще [`SIDECAR_MIN_INTERVAL`]), ручные места — по опросу [`RANKS_POLL`].
pub fn spawn_initial(state: AppState) {
    let Some(index) = open(&state) else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = super::weights::reload(&state.db).await {
            tracing::warn!(%error, "search weights not loaded, defaults in use");
        }
        match index {
            Ok(index) if index.is_empty() => {
                tracing::warn!(
                    "catalog index is empty: search returns nothing until it is built \
                     (admin: POST /admin/search/indexes/catalog/rebuild, or `nexus search rebuild`)"
                );
            }
            Ok(index) if index.entities().is_empty() && readonly(&state) => {
                tracing::warn!(
                    docs = index.len(),
                    "catalog sidecar files are missing: upload a locally built index"
                );
            }
            Ok(index) if index.entities().is_empty() => {
                tracing::info!(docs = index.len(), "catalog index opened, building sidecar");
                if let Err(error) = reload_sidecar(&state).await {
                    tracing::warn!(%error, "catalog sidecar build failed");
                }
            }
            Ok(index) => {
                tracing::info!(docs = index.len(), "catalog index opened");
                // Списки раздела — заранее: первый посетитель не ждёт сортировки всех сущностей.
                let warmed = tokio::task::spawn_blocking(move || index.warm()).await;
                if let Ok(Err(error)) = warmed {
                    tracing::warn!(%error, "catalog index warm-up failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "catalog index cannot be opened");
                return;
            }
        }
        // Прод: файлы готовые, слой Б не пересобирается; из БД — только список скрытых после
        // сборки (одним запросом при старте и после правок).
        if readonly(&state) {
            hidden_loop(state).await;
        } else {
            sidecar_loop(state).await;
        }
    });
}

/// Фоновая жизнь слоя Б: пересборка после правок (не чаще [`SIDECAR_MIN_INTERVAL`]) и раз в
/// сутки; люди — после правок; ручные места — когда меняется их отпечаток в БД.
async fn sidecar_loop(state: AppState) {
    let mut built = Instant::now();
    let mut pending = false;
    let mut ranks = ranks_version(&state.db).await.ok();
    loop {
        tokio::select! {
            _ = dirty().notified() => pending = true,
            _ = people_dirty().notified() => {
                if let Err(error) = reload_people(&state).await {
                    tracing::warn!(%error, "people sidecar rebuild failed");
                }
            }
            _ = tokio::time::sleep(RANKS_POLL) => {}
        }
        let due = built.elapsed() >= SIDECAR_DAILY;
        if (pending && built.elapsed() >= SIDECAR_MIN_INTERVAL) || due {
            pending = false;
            built = Instant::now();
            if let Err(error) = reload_sidecar(&state).await {
                tracing::warn!(%error, "catalog sidecar rebuild failed");
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
                    tracing::warn!(%error, "catalog manual ranks were not applied");
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "catalog manual ranks check failed"),
        }
    }
}
