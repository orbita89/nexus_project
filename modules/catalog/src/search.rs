//! Поиск по каталогу через Meilisearch и L3 для карточек ([`crate::cards`]).
//!
//! Источник правды — PostgreSQL, индексы производные:
//! - `entities`: поля для поиска и выдачи и готовая карточка `card` (как `GET /entities/{slug}`);
//! - `people`: готовая карточка человека `card` (как `GET /people/{slug}`).
//!
//! После каждой записи в админке затронутые сущности и люди переотправляются в индексы
//! ([`sync`]), sync ждёт, пока Meilisearch их применит, и только потом сбрасывает кэш карточек.
//! Ошибка Meilisearch запись не отменяет, только пишется в лог.
//!
//! Полная перестройка ([`reindex`]) — при старте, по расписанию и `POST /admin/search/reindex`.
//! Новые индексы строятся рядом и подменяют старые (swap), поиск не пустеет.

use crate::cards::{CardKind, KEY_PREFIX};
use crate::entities::{check_year, detail_by_id};
use crate::models::{
    page_bounds, EntityDetail, EntityKind, EntitySummary, Page, Person, PersonDetail,
    ReindexResult, SearchQuery, PERSON_COLUMNS,
};
use crate::{people, validate};
use axum::extract::State;
use axum::http::Method;
use axum::Json;
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::{json, Value};
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::search::SearchError;
use shared::{AdminUser, AppError, AppResult, AppState};
use std::time::Duration;
use uuid::Uuid;

/// Имя индекса сущностей (без префикса).
pub const INDEX: &str = "entities";
/// Имя индекса людей (без префикса).
pub const PEOPLE_INDEX: &str = "people";

/// Сколько документов отправлять одним запросом при перестройке.
const BATCH: usize = 1000;

/// Блокировка перестройки по расписанию: одна на все инстансы.
const REINDEX_LOCK: &str = "catalog:reindex-lock";

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Документ индекса сущностей: поля для отображения в выдаче, для поиска и карточка.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct SearchDoc {
    id: Uuid,
    kind: EntityKind,
    slug: String,
    title: String,
    original_title: Option<String>,
    description: Option<String>,
    release_date: Option<NaiveDate>,
    cover_url: Option<String>,
    /// Для фильтра `year = 2021`.
    year: Option<i32>,
    /// Slug'и тегов: фильтр.
    tags: Vec<String>,
    /// Названия тегов: поиск.
    tag_names: Vec<String>,
    /// Имена участников: поиск («вильнёв» находит его фильмы).
    people: Vec<String>,
    /// Карточка целиком: L3 для `GET /entities/{slug}`. Не ищется и не отдаётся поиском.
    #[sqlx(skip)]
    card: Option<EntityDetail>,
}

/// Документ индекса людей.
#[derive(Debug, Serialize)]
struct PersonDoc {
    id: Uuid,
    slug: String,
    full_name: String,
    /// Карточка целиком: L3 для `GET /people/{slug}`.
    card: PersonDetail,
}

fn entity_settings() -> Value {
    json!({
        // Порядок задаёт вес: совпадение в названии важнее, чем в описании.
        "searchableAttributes": ["title", "original_title", "people", "tag_names", "description"],
        // slug — для чтения карточки по slug.
        "filterableAttributes": ["kind", "tags", "year", "slug"],
    })
}

fn people_settings() -> Value {
    json!({
        "searchableAttributes": ["full_name"],
        "filterableAttributes": ["slug"],
    })
}

/// Документы для сущностей `ids` (все, если `None`). Удалённые из БД в ответ не попадают.
async fn load_docs(db: &sqlx::PgPool, ids: Option<&[Uuid]>) -> Result<Vec<SearchDoc>, BoxError> {
    let mut docs: Vec<SearchDoc> = sqlx::query_as(
        "SELECT e.id, e.kind, e.slug, e.title, e.original_title, e.description, e.release_date,
                e.cover_url, extract(year FROM e.release_date)::int AS year,
                array(SELECT t.slug FROM entity_tags et JOIN tags t ON t.id = et.tag_id
                      WHERE et.entity_id = e.id ORDER BY t.slug) AS tags,
                array(SELECT t.name FROM entity_tags et JOIN tags t ON t.id = et.tag_id
                      WHERE et.entity_id = e.id ORDER BY t.slug) AS tag_names,
                array(SELECT DISTINCT p.full_name FROM entity_credits c
                      JOIN people p ON p.id = c.person_id WHERE c.entity_id = e.id) AS people
         FROM entities e
         WHERE $1::uuid[] IS NULL OR e.id = ANY($1)
         ORDER BY e.id",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    for doc in &mut docs {
        // Та же карточка, что отдавал API из БД.
        match detail_by_id(db, doc.id).await {
            Ok(card) => doc.card = Some(card),
            // Удалена между запросами — следующий sync её уберёт.
            Err(AppError::NotFound) => {}
            Err(error) => return Err(error.to_string().into()),
        }
    }
    docs.retain(|doc| doc.card.is_some());
    Ok(docs)
}

/// Документы для людей `ids` (все, если `None`).
async fn load_people_docs(
    db: &sqlx::PgPool,
    ids: Option<&[Uuid]>,
) -> Result<Vec<PersonDoc>, BoxError> {
    let people: Vec<Person> = sqlx::query_as(&format!(
        "SELECT {PERSON_COLUMNS} FROM people p
         WHERE $1::uuid[] IS NULL OR p.id = ANY($1) ORDER BY p.id"
    ))
    .bind(ids)
    .fetch_all(db)
    .await?;
    let mut docs = Vec::with_capacity(people.len());
    for person in people {
        let (id, slug, full_name) = (person.id, person.slug.clone(), person.full_name.clone());
        let card = people::detail(db, person)
            .await
            .map_err(|e| e.to_string())?;
        docs.push(PersonDoc {
            id,
            slug,
            full_name,
            card,
        });
    }
    Ok(docs)
}

/// Что затронула запись в каталоге: какие документы переотправить и какие карточки сбросить.
///
/// Связи раскрываются сами: у сущности — её участники (их карточки показывают сущность),
/// у человека — его работы (их карточки показывают человека). Собирать **до** записи, если
/// запись удаляет или меняет slug: старые slug'и запоминаются, чтобы сбросить их ключи.
#[derive(Debug, Default)]
pub struct Touched {
    entities: Vec<Uuid>,
    people: Vec<Uuid>,
    /// Ключи кэша по slug'ам на момент сбора.
    keys: Vec<String>,
}

impl Touched {
    pub async fn collect(
        db: &sqlx::PgPool,
        entities: &[Uuid],
        people: &[Uuid],
    ) -> Result<Self, sqlx::Error> {
        let people_of: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT person_id FROM entity_credits WHERE entity_id = ANY($1)",
        )
        .bind(entities)
        .fetch_all(db)
        .await?;
        let entities_of: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT entity_id FROM entity_credits WHERE person_id = ANY($1)",
        )
        .bind(people)
        .fetch_all(db)
        .await?;
        let mut touched = Self {
            entities: merge(entities, entities_of),
            people: merge(people, people_of),
            keys: Vec::new(),
        };
        touched.keys = touched.current_keys(db).await?;
        Ok(touched)
    }

    /// Новая сущность (до неё ничего не было в кэше).
    pub fn entity(id: Uuid) -> Self {
        Self {
            entities: vec![id],
            ..Self::default()
        }
    }

    /// Новый человек.
    pub fn person(id: Uuid) -> Self {
        Self {
            people: vec![id],
            ..Self::default()
        }
    }

    /// Ключи карточек по slug'ам, которые сейчас в БД.
    async fn current_keys(&self, db: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
        let entity_slugs: Vec<String> =
            sqlx::query_scalar("SELECT slug FROM entities WHERE id = ANY($1)")
                .bind(&self.entities)
                .fetch_all(db)
                .await?;
        let person_slugs: Vec<String> =
            sqlx::query_scalar("SELECT slug FROM people WHERE id = ANY($1)")
                .bind(&self.people)
                .fetch_all(db)
                .await?;
        Ok(entity_slugs
            .iter()
            .map(|slug| CardKind::Entity.key(slug))
            .chain(person_slugs.iter().map(|slug| CardKind::Person.key(slug)))
            .collect())
    }
}

fn merge(a: &[Uuid], b: Vec<Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = a.iter().copied().chain(b).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Переотправляет затронутое в индексы и ждёт применения, затем сбрасывает карточки в кэше
/// (старые и новые slug'и). Вызывать после коммита. Ошибки только логируются: индекс догонит
/// [`reindex`] по расписанию.
pub async fn sync(state: &AppState, touched: Touched) {
    if touched.entities.is_empty() && touched.people.is_empty() {
        return;
    }
    if state.search.is_enabled() {
        if let Err(error) = try_sync(state, &touched).await {
            tracing::warn!(
                %error,
                entities = touched.entities.len(),
                people = touched.people.len(),
                "search index sync failed, run reindex"
            );
        }
    }
    let mut keys = touched.keys.clone();
    match touched.current_keys(&state.db).await {
        Ok(current) => keys.extend(current),
        Err(error) => tracing::warn!(%error, "card cache keys lookup failed"),
    }
    keys.sort_unstable();
    keys.dedup();
    state.cache.invalidate(&keys).await;
}

async fn try_sync(state: &AppState, touched: &Touched) -> Result<(), BoxError> {
    if !touched.entities.is_empty() {
        let docs = load_docs(&state.db, Some(&touched.entities)).await?;
        let present: Vec<Uuid> = docs.iter().map(|doc| doc.id).collect();
        upsert_and_delete(
            state,
            INDEX,
            serde_json::to_value(&docs)?,
            &touched.entities,
            &present,
        )
        .await?;
    }
    if !touched.people.is_empty() {
        let docs = load_people_docs(&state.db, Some(&touched.people)).await?;
        let present: Vec<Uuid> = docs.iter().map(|doc| doc.id).collect();
        upsert_and_delete(
            state,
            PEOPLE_INDEX,
            serde_json::to_value(&docs)?,
            &touched.people,
            &present,
        )
        .await?;
    }
    Ok(())
}

/// Обновляет документы `docs` и удаляет те из `ids`, которых нет среди `present`. Ждёт применения.
async fn upsert_and_delete(
    state: &AppState,
    name: &str,
    docs: Value,
    ids: &[Uuid],
    present: &[Uuid],
) -> Result<(), BoxError> {
    let index = state.search.index(name);
    if docs.as_array().is_some_and(|docs| !docs.is_empty()) {
        state
            .search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents?primaryKey=id"),
                Some(&docs),
            )
            .await?;
    }
    let deleted: Vec<Uuid> = ids
        .iter()
        .filter(|id| !present.contains(id))
        .copied()
        .collect();
    if !deleted.is_empty() {
        let result = state
            .search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents/delete-batch"),
                Some(&json!(deleted)),
            )
            .await;
        // Удалять из ещё не созданного индекса нечего.
        ignore(result, "index_not_found")?;
    }
    Ok(())
}

/// Одна перестройка за раз в процессе: временные индексы у них общие.
static REINDEX_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Строит индексы сущностей и людей заново во временных индексах, подменяет ими текущие и
/// сбрасывает карточки в кэше. Возвращает число сущностей.
pub async fn reindex(state: &AppState) -> Result<usize, BoxError> {
    let _guard = REINDEX_MUTEX.lock().await;
    let entities = load_docs(&state.db, None).await?;
    let people = load_people_docs(&state.db, None).await?;

    let built = [
        build_tmp(
            state,
            INDEX,
            entity_settings(),
            serde_json::to_value(&entities)?,
        )
        .await?,
        build_tmp(
            state,
            PEOPLE_INDEX,
            people_settings(),
            serde_json::to_value(&people)?,
        )
        .await?,
    ];
    let search = &state.search;
    let swaps: Vec<Value> = built
        .iter()
        .map(|(main, tmp)| json!({ "indexes": [main, tmp] }))
        .collect();
    search
        .call_and_wait(Method::POST, "/swap-indexes", Some(&json!(swaps)))
        .await?;
    for (_, tmp) in &built {
        search
            .call_and_wait(Method::DELETE, &format!("/indexes/{tmp}"), None)
            .await?;
    }

    // Карточки могли разойтись с новым индексом.
    state.cache.clear(KEY_PREFIX).await;
    Ok(entities.len())
}

/// Временный индекс `{name}_reindex` с настройками и документами; основной создаётся, если его
/// нет (swap требует, чтобы оба существовали). Возвращает `(основной, временный)`.
async fn build_tmp(
    state: &AppState,
    name: &str,
    settings: Value,
    docs: Value,
) -> Result<(String, String), BoxError> {
    let search = &state.search;
    let main = search.index(name);
    let tmp = format!("{main}_reindex");

    // Остатки прерванной перестройки.
    ignore(
        search
            .call_and_wait(Method::DELETE, &format!("/indexes/{tmp}"), None)
            .await,
        "index_not_found",
    )?;
    search
        .call_and_wait(
            Method::POST,
            "/indexes",
            Some(&json!({ "uid": tmp, "primaryKey": "id" })),
        )
        .await?;
    search
        .call_and_wait(
            Method::PATCH,
            &format!("/indexes/{tmp}/settings"),
            Some(&settings),
        )
        .await?;
    let docs = docs.as_array().cloned().unwrap_or_default();
    for chunk in docs.chunks(BATCH) {
        search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{tmp}/documents"),
                Some(&Value::from(chunk)),
            )
            .await?;
    }
    ignore(
        search
            .call_and_wait(
                Method::POST,
                "/indexes",
                Some(&json!({ "uid": main, "primaryKey": "id" })),
            )
            .await,
        "index_already_exists",
    )?;
    Ok((main, tmp))
}

fn ignore(result: Result<(), SearchError>, code: &str) -> Result<(), SearchError> {
    match result {
        Err(error) if error.code() == Some(code) => Ok(()),
        other => other,
    }
}

/// Перестройка индексов в фоне: сразу при старте приложения, затем каждые
/// `SEARCH_REINDEX_INTERVAL_SECS` (если не 0). По расписанию перестраивает только один инстанс:
/// блокировка в Redis.
pub fn spawn_reindex(state: AppState) {
    if !state.search.is_enabled() {
        return;
    }
    let interval = Duration::from_secs(state.config.search_reindex_interval_secs);
    tokio::spawn(async move {
        run_reindex(&state, "startup").await;
        if interval.is_zero() {
            return;
        }
        // Блокировка чуть короче интервала: к следующему разу она точно истечёт.
        let lock_ttl = interval
            .saturating_sub(Duration::from_secs(60))
            .max(Duration::from_secs(60));
        loop {
            tokio::time::sleep(interval).await;
            match state.cache.try_lock(REINDEX_LOCK, lock_ttl).await {
                Ok(true) => run_reindex(&state, "scheduled").await,
                Ok(false) => tracing::debug!("scheduled reindex is running on another instance"),
                // Без Redis не узнать, перестраивает ли кто-то ещё: лишняя перестройка безопасна.
                Err(error) => {
                    tracing::warn!(%error, "reindex lock unavailable, rebuilding anyway");
                    run_reindex(&state, "scheduled").await;
                }
            }
        }
    });
}

async fn run_reindex(state: &AppState, reason: &str) {
    match reindex(state).await {
        Ok(indexed) => tracing::info!(indexed, reason, "search index rebuilt"),
        Err(error) => tracing::warn!(%error, reason, "search index rebuild failed"),
    }
}

pub(crate) fn unavailable() -> AppError {
    AppError::Unavailable("search is temporarily unavailable".into())
}

/// Полнотекстовый поиск по каталогу: опечатки, ранжирование, фильтры. Сортировка по релевантности.
///
/// Индекс обновляется после изменений в админке с задержкой в доли секунды.
#[utoipa::path(
    get, path = "/search", tag = "catalog",
    params(SearchQuery),
    responses(
        (status = 200, description = "Страница результатов. `total` — оценка", body = Page<EntitySummary>),
        (status = 400, description = "Неверный фильтр", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Page<EntitySummary>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let mut filters = Vec::new();
    if let Some(kind) = query.kind {
        filters.push(format!("kind = {}", kind.as_str()));
    }
    if let Some(tag) = &query.tag {
        // Проверенный slug безопасно подставлять в строку фильтра.
        validate::slug("tag", tag)?;
        filters.push(format!("tags = \"{tag}\""));
    }
    if let Some(year) = query.year {
        check_year(year)?;
        filters.push(format!("year = {year}"));
    }
    let body = json!({
        "q": query.q.as_deref().unwrap_or_default().trim(),
        "filter": if filters.is_empty() { Value::Null } else { filters.join(" AND ").into() },
        "limit": limit,
        "offset": offset,
        "attributesToRetrieve":
            ["id", "kind", "slug", "title", "original_title", "release_date", "cover_url"],
    });

    let path = format!("/indexes/{}/search", state.search.index(INDEX));
    let response = match state.search.call(Method::POST, &path, Some(&body)).await {
        Ok(response) => response,
        // Индекс ещё не построен — искать не в чем.
        Err(error) if error.code() == Some("index_not_found") => json!({ "hits": [] }),
        Err(SearchError::Disabled) => return Err(unavailable()),
        Err(error) => {
            tracing::warn!(%error, "search request failed");
            return Err(unavailable());
        }
    };
    let items: Vec<EntitySummary> = serde_json::from_value(response["hits"].clone())
        .map_err(|e| AppError::Internal(format!("unexpected search hits: {e}")))?;
    let total = response["estimatedTotalHits"].as_i64().unwrap_or(0);

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Перестроить поисковый индекс из PostgreSQL. Нужно после загрузки данных в БД в обход API
/// (`make seed`) или если индекс отстал. Ждёт завершения.
#[utoipa::path(
    post, path = "/admin/search/reindex", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Индекс перестроен", body = ReindexResult),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "Meilisearch недоступен", body = ErrorBody),
    )
)]
pub async fn reindex_handler(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<Json<ReindexResult>> {
    if !state.search.is_enabled() {
        return Err(unavailable());
    }
    let indexed = reindex(&state).await.map_err(|error| {
        tracing::warn!(%error, "search reindex failed");
        unavailable()
    })?;
    tracing::info!(admin_id = %admin.id, indexed, "search index rebuilt");
    Ok(Json(ReindexResult { indexed }))
}
