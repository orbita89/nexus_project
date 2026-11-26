//! Ответы API из единого индекса: выдача, случайная работа, соседи, похожие, подсказки
//! фасетов. Параметры и контракт HTTP — те же, что и раньше, см. [`crate::search`].

use std::sync::Arc;

use shared::{AppError, AppResult, AppState};

use super::{sync, CatalogIndex, Found, IndexError, Query, Sort};
use crate::models::{EntityKind, EntitySummary, Neighbors, Page};

/// Сколько сущностей выдачи просматривать, разыскивая запись для «предыдущая / следующая».
const NEIGHBORS_SCAN: usize = 5000;

fn unavailable() -> AppError {
    AppError::Unavailable("search is temporarily unavailable".into())
}

impl From<IndexError> for AppError {
    fn from(error: IndexError) -> Self {
        tracing::warn!(%error, "catalog index error");
        unavailable()
    }
}

/// Индекс каталога; выключен или не открылся — 503.
pub fn index(state: &AppState) -> AppResult<Arc<CatalogIndex>> {
    Ok(sync::open(state).ok_or_else(unavailable)??)
}

/// Работа с индексом (mmap может уйти в page fault, разбор и подсчёт — процессор) — вне
/// потоков async.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IndexError> + Send + 'static,
) -> AppResult<T> {
    Ok(tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(format!("catalog index task failed: {e}")))??)
}

/// Номера сущностей по slug'ам (неизвестные пропускаются) — из индекса (термы `slug:`), не из
/// БД.
pub async fn ords_of_slugs(state: &AppState, slugs: &[String]) -> AppResult<Vec<u32>> {
    if slugs.is_empty() {
        return Ok(Vec::new());
    }
    let index = index(state)?;
    let slugs = slugs.to_vec();
    blocking(move || index.ords_of_slugs(&slugs)).await
}

/// Страница выдачи. `random` — случайное окно (тот же порядок, смещение случайно).
pub async fn search(
    state: &AppState,
    query: Query,
    random: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Page<EntitySummary>> {
    let index = index(state)?;
    let (found, offset) = blocking(move || {
        let (limit, mut offset) = (limit.max(0) as usize, offset.max(0) as usize);
        if random {
            let total = index.count(&query)?;
            offset = crate::search::random_below((total - total.min(limit) + 1) as i64) as usize;
        }
        Ok::<(Found, usize), IndexError>((index.search(&query, limit, offset)?, offset))
    })
    .await?;
    Ok(Page {
        items: found.items,
        total: found.total as i64,
        limit,
        offset: offset as i64,
    })
}

/// Сколько сущностей подходит под запрос.
pub async fn count(state: &AppState, query: Query) -> AppResult<usize> {
    let index = index(state)?;
    blocking(move || index.count(&query)).await
}

/// Предыдущая и следующая запись в выдаче (тот же порядок, что у [`search`]). Записи в ряду нет
/// (или ряд длиннее [`NEIGHBORS_SCAN`]) — оба края `None`.
pub async fn neighbors(state: &AppState, query: Query, slug: String) -> AppResult<Neighbors> {
    let none = Neighbors {
        prev: None,
        next: None,
    };
    let Some(ord) = ords_of_slugs(state, &[slug]).await?.into_iter().next() else {
        return Ok(none);
    };
    let index = index(state)?;
    let found = blocking(move || {
        let row = index.ords(&query, NEIGHBORS_SCAN)?;
        let Some(at) = row.iter().position(|o| *o == ord) else {
            return Ok(None);
        };
        let prev = at.checked_sub(1).map(|i| row[i]);
        let next = row.get(at + 1).copied();
        let wanted: Vec<u32> = prev.iter().chain(next.iter()).copied().collect();
        let mut cards = index.by_ords(&wanted)?.into_iter();
        // `by_ords` пропускает неизвестные: соседей берём по порядку запроса.
        let first = prev.and_then(|_| cards.next());
        let second = next.and_then(|_| cards.next());
        Ok(Some((first, second)))
    })
    .await?;
    Ok(match found {
        Some((prev, next)) => Neighbors { prev, next },
        None => none,
    })
}

/// Похожие: видимые того же типа с одним из `tags`, кроме `exclude`; сверху «Рекомендуемые».
pub async fn similar(
    state: &AppState,
    kind: EntityKind,
    tags: &[String],
    exclude: &[String],
    limit: i64,
) -> AppResult<(Vec<EntitySummary>, i64)> {
    let query = Query {
        kinds: vec![kind],
        tags: tags.to_vec(),
        exclude_ords: ords_of_slugs(state, exclude).await?,
        sort: Sort::Recommended,
        ..Query::default()
    };
    let page = search(state, query, false, limit, 0).await?;
    Ok((page.items, page.total))
}

/// Подсказки значений фасета: `studios`, `publishers`, `museums`, `countries`, `tags` — по части
/// названия (без регистра и `ё`); без `q` — самые частые. `kind` считает внутри раздела.
pub async fn facet(
    state: &AppState,
    field: &str,
    q: &str,
    kind: Option<EntityKind>,
    limit: usize,
) -> AppResult<Vec<(String, i64)>> {
    let group = match field {
        "studios" => "studio",
        "publishers" => "publisher",
        "museums" => "museum",
        "countries" => "country",
        "tags" => "tag",
        _ => return Ok(Vec::new()),
    };
    let index = index(state)?;
    let q = q.to_owned();
    let hits = blocking(move || Ok(index.hints(group, &q, kind, limit))).await?;
    Ok(hits
        .into_iter()
        .map(|(name, count)| (name, count as i64))
        .collect())
}
