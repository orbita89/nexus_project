//! Поиск по коллекциям через Meilisearch: индекс `collections`.
//!
//! Зачем индекс, а не `ILIKE` по названию: нужны опечатки и поиск по описанию, а подстрока в
//! PostgreSQL этого не даёт (и на росте числа коллекций упирается в последовательный просмотр).
//! Документ компактный — название, описание и дата: выдача всё равно читается из БД, чтобы в
//! ней были счётчики, обложка и автор, как в обычном списке.
//!
//! В индексе только **публичные** коллекции: приватную никто искать не должен, поэтому она не
//! попадает туда вовсе, а не отфильтровывается на поиске. Скрыли коллекцию — документ
//! удаляется ([`update`]).
//!
//! Индекс наполняется при старте приложения ([`spawn_fill`]) и правится при каждой записи
//! (создание, правка, удаление, смена видимости, действия админа). Meilisearch выключен или
//! недоступен — запись в БД это не отменяет: поиск догонит следующей перестройкой.

use crate::models::PageQuery;
use axum::http::Method;
use serde::Serialize;
use serde_json::{json, Value};
use shared::search::{ignore, search_exact, unavailable, SearchError};
use shared::{AppError, AppResult, AppState};
use uuid::Uuid;

/// Ошибка наполнения: и Meilisearch, и PostgreSQL.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Имя индекса (без префикса окружения).
pub const INDEX: &str = "collections";
/// Коллекций в одной пачке при наполнении.
const BATCH: i64 = 1_000;
/// Потолок счёта и листания, как у каталога.
const MAX_TOTAL_HITS: u32 = 100_000;
/// Длина поискового запроса.
pub const MAX_QUERY: usize = 300;

/// Документ коллекции: по названию и описанию ищут, по дате сортируют равные совпадения.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct CollectionDoc {
    id: Uuid,
    title: String,
    description: Option<String>,
    /// `created_at` в unix-секундах: Meilisearch сортирует только числа.
    created_ts: i64,
}

const DOC_SELECT: &str = "SELECT c.id, c.title, c.description,
                                 extract(epoch FROM c.created_at)::bigint AS created_ts
                          FROM collections c WHERE c.is_public";

fn settings() -> Value {
    json!({
        // Название важнее описания: порядок задаёт вес.
        "searchableAttributes": ["title", "description"],
        "sortableAttributes": ["created_ts"],
        // При равном качестве совпадения — свежие выше.
        "rankingRules": ["words", "typo", "proximity", "attribute", "sort", "exactness",
                         "created_ts:desc"],
        "pagination": { "maxTotalHits": MAX_TOTAL_HITS },
    })
}

/// Наполняет индекс с нуля: настройки и все публичные коллекции пачками. Старые документы
/// удаляются вместе с индексом — коллекций немного, перестройка занимает доли секунды.
pub async fn fill(state: &AppState) -> Result<u64, BoxError> {
    if !state.search.is_enabled() {
        return Ok(0);
    }
    let index = state.search.index(INDEX);
    ignore(
        state
            .search
            .call_and_wait(Method::DELETE, &format!("/indexes/{index}"), None)
            .await
            .map(|_| ()),
        "index_not_found",
    )?;
    state
        .search
        .call_and_wait(
            Method::POST,
            "/indexes",
            Some(&json!({ "uid": index, "primaryKey": "id" })),
        )
        .await?;
    state
        .search
        .call_and_wait(
            Method::PATCH,
            &format!("/indexes/{index}/settings"),
            Some(&settings()),
        )
        .await?;

    let mut sent = 0;
    let mut offset = 0;
    loop {
        let docs: Vec<CollectionDoc> =
            sqlx::query_as(&format!("{DOC_SELECT} ORDER BY c.id LIMIT $1 OFFSET $2"))
                .bind(BATCH)
                .bind(offset)
                .fetch_all(&state.db)
                .await?;
        if docs.is_empty() {
            break;
        }
        let count = docs.len();
        state
            .search
            .call_and_wait(
                Method::POST,
                &format!("/indexes/{index}/documents?primaryKey=id"),
                Some(&serde_json::to_value(&docs).unwrap_or(Value::Null)),
            )
            .await?;
        sent += count as u64;
        offset += BATCH;
        if count < BATCH as usize {
            break;
        }
    }
    Ok(sent)
}

/// Наполнение при старте: в фоне, чтобы не задерживать запуск. Meilisearch выключен — ничего.
pub fn spawn_fill(state: AppState) {
    if !state.search.is_enabled() {
        return;
    }
    tokio::spawn(async move {
        match fill(&state).await {
            Ok(sent) => tracing::info!(collections = sent, "collections index built"),
            Err(error) => tracing::warn!(%error, "collections index build failed"),
        }
    });
}

/// Переотправляет изменённые коллекции: публичные обновляются, остальные (приватные и
/// удалённые) убираются из индекса. Вызывается после коммита.
pub async fn update(state: &AppState, ids: &[Uuid]) {
    if ids.is_empty() || !state.search.is_enabled() {
        return;
    }
    if let Err(error) = try_update(state, ids).await {
        // Поиск отстанет до следующей перестройки, но запись уже сохранена.
        tracing::warn!(%error, "collections index update failed");
    }
}

async fn try_update(state: &AppState, ids: &[Uuid]) -> Result<(), BoxError> {
    let docs: Vec<CollectionDoc> = sqlx::query_as(&format!("{DOC_SELECT} AND c.id = ANY($1)"))
        .bind(ids)
        .fetch_all(&state.db)
        .await?;
    let present: Vec<Uuid> = docs.iter().map(|doc| doc.id).collect();
    let docs = serde_json::to_value(&docs).unwrap_or(Value::Null);
    let index = state.search.index(INDEX);
    Ok(shared::search::upsert_and_delete(&state.search, &index, &docs, ids, &present).await?)
}

/// Ищет публичные коллекции: id в порядке релевантности и сколько всего нашлось.
pub async fn search(state: &AppState, q: &str, query: &PageQuery) -> AppResult<(Vec<Uuid>, i64)> {
    let (limit, offset) = shared::pagination::page_bounds(query.limit, query.offset);
    if q.chars().count() > MAX_QUERY {
        return Err(AppError::BadRequest(format!(
            "q must be at most {MAX_QUERY} characters"
        )));
    }
    let body = json!({ "q": q, "attributesToRetrieve": ["id"] });
    let path = format!("/indexes/{}/search", state.search.index(INDEX));
    let (response, total) = match search_exact(&state.search, &path, body, limit, offset).await {
        Ok(found) => found,
        // Индекса ещё нет (первый старт, перестройка не прошла) — пустая выдача, не ошибка.
        Err(error) if error.code() == Some("index_not_found") => (json!({ "hits": [] }), 0),
        Err(SearchError::Disabled) => return Err(unavailable()),
        Err(error) => {
            tracing::warn!(%error, "collections search request failed");
            return Err(unavailable());
        }
    };
    let ids = response["hits"]
        .as_array()
        .map(|hits| {
            hits.iter()
                .filter_map(|hit| hit["id"].as_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    Ok((ids, total))
}
