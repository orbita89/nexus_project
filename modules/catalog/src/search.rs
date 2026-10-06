//! Поиск по каталогу через Meilisearch.
//!
//! Источник правды — PostgreSQL, индекс производный:
//! - после каждой записи в админке затронутые сущности переотправляются в индекс ([`sync`]).
//!   Ошибка Meilisearch запись не отменяет, только пишется в лог;
//! - полная перестройка ([`reindex`]) — при старте приложения и `POST /admin/search/reindex`.
//!   Новый индекс строится рядом и подменяет старый (swap), поиск не пустеет.

use crate::entities::check_year;
use crate::models::{page_bounds, EntityKind, EntitySummary, Page, ReindexResult, SearchQuery};
use crate::validate;
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
use uuid::Uuid;

/// Имя индекса сущностей (без префикса).
pub const INDEX: &str = "entities";

/// Сколько документов отправлять одним запросом при перестройке.
const BATCH: usize = 1000;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Документ индекса: поля для отображения в выдаче и для поиска.
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
}

fn settings() -> Value {
    json!({
        // Порядок задаёт вес: совпадение в названии важнее, чем в описании.
        "searchableAttributes": ["title", "original_title", "people", "tag_names", "description"],
        "filterableAttributes": ["kind", "tags", "year"],
    })
}

/// Документы для сущностей `ids` (все, если `None`).
async fn load_docs(db: &sqlx::PgPool, ids: Option<&[Uuid]>) -> Result<Vec<SearchDoc>, sqlx::Error> {
    sqlx::query_as(
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
    .await
}

/// Переотправляет сущности в индекс: существующие обновляет, удалённые из БД убирает.
/// Вызывать после коммита. Ошибки только логируются: индекс догонит [`reindex`].
pub async fn sync(state: &AppState, ids: &[Uuid]) {
    if !state.search.is_enabled() || ids.is_empty() {
        return;
    }
    if let Err(error) = try_sync(state, ids).await {
        tracing::warn!(%error, count = ids.len(), "search index sync failed, run reindex");
    }
}

async fn try_sync(state: &AppState, ids: &[Uuid]) -> Result<(), BoxError> {
    let index = state.search.index(INDEX);
    let docs = load_docs(&state.db, Some(ids)).await?;
    let deleted: Vec<Uuid> = ids
        .iter()
        .filter(|id| !docs.iter().any(|doc| doc.id == **id))
        .copied()
        .collect();
    if !docs.is_empty() {
        state
            .search
            .call(
                Method::POST,
                &format!("/indexes/{index}/documents?primaryKey=id"),
                Some(&serde_json::to_value(&docs)?),
            )
            .await?;
    }
    if !deleted.is_empty() {
        state
            .search
            .call(
                Method::POST,
                &format!("/indexes/{index}/documents/delete-batch"),
                Some(&json!(deleted)),
            )
            .await?;
    }
    Ok(())
}

/// Одна перестройка за раз: временный индекс у них общий.
static REINDEX_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Строит индекс заново во временном индексе и подменяет им текущий. Возвращает число документов.
pub async fn reindex(state: &AppState) -> Result<usize, BoxError> {
    let _guard = REINDEX_LOCK.lock().await;
    let search = &state.search;
    let main = search.index(INDEX);
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
            Some(&settings()),
        )
        .await?;

    let docs = load_docs(&state.db, None).await?;
    for chunk in docs.chunks(BATCH) {
        search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{tmp}/documents"),
                Some(&serde_json::to_value(chunk)?),
            )
            .await?;
    }

    // swap требует, чтобы оба индекса существовали.
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
    search
        .call_and_wait(
            Method::POST,
            "/swap-indexes",
            Some(&json!([{ "indexes": [main, tmp] }])),
        )
        .await?;
    search
        .call_and_wait(Method::DELETE, &format!("/indexes/{tmp}"), None)
        .await?;

    Ok(docs.len())
}

fn ignore(result: Result<(), SearchError>, code: &str) -> Result<(), SearchError> {
    match result {
        Err(error) if error.code() == Some(code) => Ok(()),
        other => other,
    }
}

/// Перестройка индекса в фоне при старте приложения.
pub fn spawn_reindex(state: AppState) {
    if !state.search.is_enabled() {
        return;
    }
    tokio::spawn(async move {
        match reindex(&state).await {
            Ok(indexed) => tracing::info!(indexed, "search index rebuilt"),
            Err(error) => tracing::warn!(%error, "search index rebuild failed"),
        }
    });
}

fn unavailable() -> AppError {
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
