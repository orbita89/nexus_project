//! Книжный поиск в API: `GET /search`, соседи, похожие и подсказки фильтров для книг идут в
//! индекс Tantivy ([`super::index`]), когда запрос — только про книги
//! (`kind=book`). Остальные разделы — индекс работ ([`crate::works`]); смешанные запросы
//! чередуют оба ([`crate::search`]).
//!
//! Параметры и ответы — те же, что у остальных разделов: фронтенд не различает, кто ответил. Отличия:
//! - `q` ищет по названию, авторам и серии (у работ — название и студии);
//! - в строке выдачи есть `authors`;
//! - работает фильтр `has_cover`;
//! - `sort=trending` — то же, что `popular`, `awarded` — то же, что `recommended`: своей
//!   статистики (TMDB, награды) у книг нет;
//! - фильтры, которых у книг не бывает (`studio`, `museum`, `country`, `award`, `awarded=true`),
//!   дают пустую выдачу.

use std::sync::Arc;

use shared::{AppError, AppResult, AppState};

use super::index::{BookForm, BookIndex, BookQuery, BookSort, Found, IndexError};
use super::sync;
use crate::models::{
    EntityKind, EntitySort, EntitySummary, Neighbors, Page, SearchQuery, SearchSort,
};

/// Сколько книг выдачи просматривать, разыскивая работу для «предыдущая / следующая».
const NEIGHBORS_SCAN: usize = 5000;
/// Тегов в подсказках книг: жанров FB2 больше двухсот, а панель фильтров показывает их все.
pub const MAX_TAG_HITS: usize = 300;

/// Запрос — только про книги, и книжный поиск включён: отвечает Tantivy.
pub fn routed(state: &AppState, kinds: &[EntityKind]) -> bool {
    sync::enabled(state) && kinds == [EntityKind::Book]
}

fn unavailable() -> AppError {
    AppError::Unavailable("books search is not available".into())
}

impl From<IndexError> for AppError {
    fn from(error: IndexError) -> Self {
        tracing::warn!(%error, "books index error");
        unavailable()
    }
}

fn open(state: &AppState) -> AppResult<Arc<BookIndex>> {
    Ok(sync::open(state).ok_or_else(unavailable)??)
}

/// Работа с индексом (mmap может уйти в page fault, разбор и подсчёт — процессор) — вне
/// потоков async.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IndexError> + Send + 'static,
) -> AppResult<T> {
    Ok(tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(format!("books index task failed: {e}")))??)
}

/// Запрос к индексу из параметров `GET /search` (уже проверенных `search::filters_of`).
/// `None` — фильтр, которого у книг не бывает: выдача пуста.
pub fn query_of(query: &SearchQuery, countries: &[String]) -> Option<BookQuery> {
    let foreign = query.studio.is_some()
        || query.studio_slug.is_some()
        || query.museum.is_some()
        || query.award.is_some()
        || query.award_year.is_some()
        || query.awarded == Some(true)
        || !countries.is_empty();
    if foreign {
        return None;
    }
    let (year_from, year_to) = match query.year {
        Some(year) => (Some(year), Some(year)),
        None => (query.year_from, query.year_to),
    };
    Some(BookQuery {
        q: query.q.as_deref().unwrap_or_default().trim().to_string(),
        sort: match query.sort {
            Some(SearchSort::Recommended | SearchSort::Awarded) => BookSort::Recommended,
            Some(SearchSort::Trending | SearchSort::Popular) => BookSort::Popular,
            Some(SearchSort::TopRated | SearchSort::Rating) => BookSort::Rating,
            Some(SearchSort::New) => BookSort::New,
            Some(SearchSort::Soon) => BookSort::Soon,
            Some(SearchSort::Title) => BookSort::Title,
            Some(SearchSort::Random) | None => BookSort::Relevance,
        },
        tag: query.tag.clone(),
        year_from,
        year_to,
        publisher: query.publisher.as_ref().map(|p| p.trim().to_string()),
        person: query.person.clone(),
        role: query.role.as_ref().map(|r| r.trim().to_string()),
        has_cover: query.has_cover,
        forms: query.book_form.as_deref().map(forms_of).unwrap_or_default(),
        ..BookQuery::default()
    })
}

/// Значение `book_form`: виды через запятую или `all`. `None` — неизвестный вид (400).
/// Пустой список — значение по умолчанию (только книги).
pub fn parse_forms(value: &str) -> Option<Vec<BookForm>> {
    let mut forms = Vec::new();
    for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if part == "all" {
            return Some(BookForm::ALL.to_vec());
        }
        let form = BookForm::parse(part)?;
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    Some(forms)
}

fn forms_of(value: &str) -> Vec<BookForm> {
    parse_forms(value).unwrap_or_default()
}

/// `GET /search` для книг. `random` — случайное окно выдачи (как у остальных разделов).
pub async fn search(
    state: &AppState,
    query: Option<BookQuery>,
    random: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Page<EntitySummary>> {
    let empty = Page {
        items: Vec::new(),
        total: 0,
        limit,
        offset: if random { 0 } else { offset },
    };
    let Some(query) = query else {
        return Ok(empty);
    };
    let index = open(state)?;
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

/// Запрос к индексу для админского списка книг (`GET /admin/entities?kind=book`): все виды
/// текста; без `sort` — наш порядок (ручные места, затем балл; с `q` — релевантность с местами).
pub fn admin_query(
    q: Option<&str>,
    sort: Option<EntitySort>,
    tag: Option<String>,
    year: Option<i32>,
) -> BookQuery {
    BookQuery {
        q: q.unwrap_or_default().trim().to_string(),
        sort: match sort {
            None => BookSort::Relevance,
            Some(EntitySort::Recommended) => BookSort::Recommended,
            Some(EntitySort::Trending | EntitySort::Popular) => BookSort::Popular,
            Some(EntitySort::TopRated | EntitySort::Rating) => BookSort::Rating,
            Some(EntitySort::New) => BookSort::New,
            Some(EntitySort::Soon) => BookSort::Soon,
            Some(EntitySort::Title) => BookSort::Title,
        },
        tag,
        year_from: year,
        year_to: year,
        forms: BookForm::ALL.to_vec(),
        ..BookQuery::default()
    }
}

/// Сколько книг под запрос.
pub async fn count(state: &AppState, query: BookQuery) -> AppResult<usize> {
    let index = open(state)?;
    blocking(move || index.count(&query)).await
}

/// Соседи книги `slug` в выдаче `query`. Книги в ряду нет (или ряд длиннее
/// [`NEIGHBORS_SCAN`]) — оба края `None`.
pub async fn neighbors(
    state: &AppState,
    query: Option<BookQuery>,
    slug: String,
) -> AppResult<Neighbors> {
    let none = Neighbors {
        prev: None,
        next: None,
    };
    let Some(query) = query else {
        return Ok(none);
    };
    let index = open(state)?;
    let found = blocking(move || {
        let row = index.slugs(&query, NEIGHBORS_SCAN)?;
        let Some(at) = row.iter().position(|s| *s == slug) else {
            return Ok(None);
        };
        let prev = at.checked_sub(1).map(|i| row[i].clone());
        let next = row.get(at + 1).cloned();
        let wanted: Vec<String> = prev.iter().chain(next.iter()).cloned().collect();
        Ok(Some((prev, next, index.by_slugs(&wanted)?)))
    })
    .await?;
    let Some((prev, next, cards)) = found else {
        return Ok(none);
    };
    let pick = |slug: Option<String>| {
        slug.and_then(|slug| cards.iter().find(|card| card.slug == slug).cloned())
    };
    Ok(Neighbors {
        prev: pick(prev),
        next: pick(next),
    })
}

/// Похожие книги: есть хотя бы один из тегов `tags`, кроме `exclude`; сверху — «Рекомендуемые».
pub async fn similar(
    state: &AppState,
    tags: &[String],
    exclude: &[String],
    limit: i64,
) -> AppResult<(Vec<EntitySummary>, i64)> {
    let index = open(state)?;
    let query = BookQuery {
        sort: BookSort::Recommended,
        any_tags: tags.to_vec(),
        exclude_slugs: exclude.to_vec(),
        ..BookQuery::default()
    };
    let found = blocking(move || index.search(&query, limit.max(0) as usize, 0)).await?;
    Ok((found.items, found.total as i64))
}

/// Теги и жанры книг со счётчиками, самые частые сверху: подсказки `GET /search/tags`.
pub async fn tags(state: &AppState, limit: Option<i64>) -> AppResult<Vec<(String, i64)>> {
    let index = open(state)?;
    let limit = limit.unwrap_or(10).clamp(1, MAX_TAG_HITS as i64) as usize;
    let tags = blocking(move || index.tags()).await?;
    Ok(tags
        .iter()
        .take(limit)
        .map(|(slug, count)| (slug.clone(), *count as i64))
        .collect())
}

/// Издательства по части названия: подсказки `GET /search/publishers`.
pub async fn publishers(
    state: &AppState,
    q: &str,
    limit: Option<i64>,
) -> AppResult<Vec<(String, i64)>> {
    let index = open(state)?;
    let limit = limit.unwrap_or(10).clamp(1, 50) as usize;
    let q = q.to_string();
    let hits = blocking(move || index.publishers(&q, limit)).await?;
    Ok(hits
        .into_iter()
        .map(|(name, count)| (name, count as i64))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(change: impl FnOnce(&mut SearchQuery)) -> SearchQuery {
        let mut query: SearchQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        change(&mut query);
        query
    }

    #[test]
    fn search_params_map_to_the_index_query() {
        let q = query_of(
            &query(|q| {
                q.q = Some("  собачье ".into());
                q.year = Some(1925);
                q.person = Some("mikhail-bulgakov".into());
                q.role = Some("author".into());
                q.has_cover = Some(true);
                q.sort = Some(SearchSort::TopRated);
            }),
            &[],
        )
        .unwrap();
        assert_eq!(q.q, "собачье");
        assert_eq!((q.year_from, q.year_to), (Some(1925), Some(1925)));
        assert_eq!(q.person.as_deref(), Some("mikhail-bulgakov"));
        assert_eq!(q.role.as_deref(), Some("author"));
        assert_eq!(q.has_cover, Some(true));
        assert_eq!(q.sort, BookSort::Rating);
    }

    #[test]
    fn filters_books_never_have_give_an_empty_result() {
        assert!(query_of(&query(|q| q.studio = Some("Warner".into())), &[]).is_none());
        assert!(query_of(&query(|q| q.awarded = Some(true)), &[]).is_none());
        assert!(query_of(&query(|_| {}), &["US".into()]).is_none());
        // «Без наград» книгам подходит всегда.
        assert!(query_of(&query(|q| q.awarded = Some(false)), &[]).is_some());
    }

    #[test]
    fn book_form_param() {
        assert_eq!(parse_forms(""), Some(vec![]));
        assert_eq!(
            parse_forms("collection, book,collection"),
            Some(vec![BookForm::Collection, BookForm::Book])
        );
        assert_eq!(parse_forms("all"), Some(BookForm::ALL.to_vec()));
        assert_eq!(parse_forms("magazine"), None);
        let q = query_of(&query(|q| q.book_form = Some("article".into())), &[]).unwrap();
        assert_eq!(q.forms, [BookForm::Article]);
        assert!(query_of(&query(|_| {}), &[]).unwrap().forms.is_empty());
    }
}
