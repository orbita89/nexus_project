//! Ответы API по работам из индекса Tantivy: выдача, случайная работа, соседи, похожие и
//! подсказки фасетов. Параметры и контракт — те же, что и раньше, см. [`crate::search`].

use std::sync::Arc;

use serde_json::Value;
use shared::tantivy::blocking;
use shared::{AppError, AppResult, AppState};

use super::index::{WorksIndex, WorksQuery, WorksSort};
use super::sync;
use crate::models::{EntitySummary, Neighbors, Page};

/// Сколько работ выдачи просматривать, разыскивая работу для «предыдущая / следующая».
const NEIGHBORS_SCAN: usize = 5000;

fn unavailable() -> AppError {
    AppError::Unavailable("search is temporarily unavailable".into())
}

fn fail(error: tantivy::TantivyError) -> AppError {
    tracing::warn!(%error, "works index error");
    unavailable()
}

pub fn index(state: &AppState) -> AppResult<Arc<WorksIndex>> {
    match sync::open(state) {
        Some(Ok(index)) => Ok(index),
        Some(Err(error)) => Err(fail(error)),
        None => Err(unavailable()),
    }
}

fn summaries(cards: Vec<Value>) -> AppResult<Vec<EntitySummary>> {
    serde_json::from_value(Value::Array(cards))
        .map_err(|e| AppError::Internal(format!("unexpected works hits: {e}")))
}

/// Страница выдачи. `random` — случайное окно (тот же порядок, смещение случайно).
pub async fn search(
    state: &AppState,
    query: WorksQuery,
    random: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Page<EntitySummary>> {
    let index = index(state)?;
    let (cards, total, offset) = blocking(move || {
        if random {
            // Порядок релевантности, случайно только смещение окна.
            let total = index.page(&query, 1, 0)?.1 as i64;
            if total == 0 {
                return Ok((Vec::new(), 0, offset));
            }
            let offset = crate::search::random_below(total - total.min(limit) + 1);
            let (cards, _) = index.page(&query, limit as usize, offset as usize)?;
            return Ok((cards, total, offset));
        }
        let (cards, total) = index.page(&query, limit as usize, offset as usize)?;
        Ok((cards, total as i64, offset))
    })
    .await
    .map_err(fail)?;
    Ok(Page {
        items: summaries(cards)?,
        total,
        limit,
        offset,
    })
}

/// Сколько работ подходит под запрос.
pub async fn count(state: &AppState, query: WorksQuery) -> AppResult<i64> {
    let index = index(state)?;
    Ok(blocking(move || index.count(&query)).await.map_err(fail)? as i64)
}

/// Предыдущая и следующая работа в выдаче (тот же порядок, что у `search`).
pub async fn neighbors(state: &AppState, query: WorksQuery, slug: String) -> AppResult<Neighbors> {
    let index = index(state)?;
    let found = blocking(move || {
        let row = index.slugs(&query, NEIGHBORS_SCAN, 0)?;
        let Some(at) = row.iter().position(|s| *s == slug) else {
            return Ok(Vec::new());
        };
        let prev = at.checked_sub(1).map(|i| row[i].clone());
        let next = row.get(at + 1).cloned();
        let wanted: Vec<String> = [prev.clone(), next.clone()].into_iter().flatten().collect();
        let cards = index.by_slugs(&wanted)?;
        Ok(vec![
            Value::from(prev),
            Value::from(next),
            Value::Array(cards),
        ])
    })
    .await
    .map_err(fail)?;
    if found.is_empty() {
        return Ok(Neighbors {
            prev: None,
            next: None,
        });
    }
    let cards = summaries(found[2].as_array().cloned().unwrap_or_default())?;
    let pick = |v: &Value| {
        v.as_str()
            .and_then(|s| cards.iter().find(|c| c.slug == s))
            .cloned()
    };
    Ok(Neighbors {
        prev: pick(&found[0]),
        next: pick(&found[1]),
    })
}

/// Похожие: видимые того же типа с одним из `tags`, кроме `exclude`; сверху «Рекомендуемые».
pub async fn similar(
    state: &AppState,
    kind: &str,
    tags: &[String],
    exclude: &[String],
    limit: i64,
) -> AppResult<(Vec<EntitySummary>, i64)> {
    let query = WorksQuery {
        kinds: vec![kind.to_owned()],
        tags: tags.to_vec(),
        exclude_slugs: exclude.to_vec(),
        sort: Some(WorksSort::Recommended),
        ..WorksQuery::default()
    };
    let page = search(state, query, false, limit, 0).await?;
    Ok((page.items, page.total))
}

/// Подсказки фасета (`studios`, `publishers`, `museums`, `countries`, `tags`).
pub async fn facet(
    state: &AppState,
    field: &str,
    q: &str,
    kind: Option<&str>,
    limit: Option<i64>,
) -> AppResult<Vec<(String, i64)>> {
    let index = index(state)?;
    let limit = limit.unwrap_or(10).clamp(1, 50) as usize;
    let (field, q, kind) = (field.to_owned(), q.to_owned(), kind.map(str::to_owned));
    blocking(move || index.facet(&field, &q, kind.as_deref(), limit))
        .await
        .map_err(fail)
}
