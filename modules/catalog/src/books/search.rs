//! Книжный поиск в API: `GET /search`, соседи, похожие и подсказки фильтров для запросов только
//! про книги (`kind=book`) идут в единый индекс ([`crate::index`]) с фильтром вида `book`.
//! Остальные разделы — тот же индекс с другими видами; смешанные запросы чередуют оба
//! ([`crate::search`]).
//!
//! Параметры и ответы — те же, что у остальных разделов: фронтенд не различает, кто ответил. Отличия:
//! - `q` ищет по названию, авторам и серии (у работ — название, люди и студии);
//! - в строке выдачи есть `authors`;
//! - работает фильтр `has_cover`;
//! - `sort=trending` — то же, что `popular`, `awarded` — то же, что `recommended`: своей
//!   статистики (TMDB, награды) у книг нет;
//! - фильтры, которых у книг не бывает (`studio`, `museum`, `country`, `award`, `awarded=true`),
//!   дают пустую выдачу.

use shared::{AppResult, AppState};

use crate::index::{sync, BookForm, Query, Sort};
use crate::models::{
    EntityKind, EntitySort, EntitySummary, Neighbors, Page, SearchQuery, SearchSort,
};

/// Тегов в подсказках книг: жанров FB2 больше двухсот, а панель фильтров показывает их все.
pub const MAX_TAG_HITS: usize = 300;

/// Запрос — только про книги, и поиск включён: отвечает единый индекс.
pub fn routed(state: &AppState, kinds: &[EntityKind]) -> bool {
    sync::enabled(state) && kinds == [EntityKind::Book]
}

/// Запрос к индексу из параметров `GET /search` (уже проверенных `search::filters_of`).
/// `None` — фильтр, которого у книг не бывает: выдача пуста.
pub fn query_of(query: &SearchQuery, countries: &[String]) -> Option<Query> {
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
    Some(Query {
        q: query.q.as_deref().unwrap_or_default().trim().to_string(),
        kinds: vec![EntityKind::Book],
        sort: match query.sort {
            Some(SearchSort::Recommended | SearchSort::Awarded) => Sort::Recommended,
            Some(SearchSort::Trending | SearchSort::Popular) => Sort::Popular,
            Some(SearchSort::TopRated | SearchSort::Rating) => Sort::Rating,
            Some(SearchSort::New) => Sort::New,
            Some(SearchSort::Soon) => Sort::Soon,
            Some(SearchSort::Title) => Sort::Title,
            Some(SearchSort::Random) | None => Sort::Relevance,
        },
        tags: query.tag.iter().cloned().collect(),
        year_from,
        year_to,
        publisher: query.publisher.as_ref().map(|p| p.trim().to_string()),
        person: query.person.clone(),
        role: query.role.as_ref().map(|r| r.trim().to_string()),
        has_cover: query.has_cover,
        forms: query.book_form.as_deref().map(forms_of).unwrap_or_default(),
        ..Query::default()
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
    query: Option<Query>,
    random: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Page<EntitySummary>> {
    let Some(query) = query else {
        return Ok(Page {
            items: Vec::new(),
            total: 0,
            limit,
            offset: if random { 0 } else { offset },
        });
    };
    crate::index::search::search(state, query, random, limit, offset).await
}

/// Запрос к индексу для админского списка книг (`GET /admin/entities?kind=book`): все виды
/// текста; без `sort` — наш порядок (ручные места, затем балл; с `q` — релевантность с местами).
pub fn admin_query(
    q: Option<&str>,
    sort: Option<EntitySort>,
    tag: Option<String>,
    year: Option<i32>,
) -> Query {
    Query {
        q: q.unwrap_or_default().trim().to_string(),
        sort: match sort {
            None => Sort::Relevance,
            Some(EntitySort::Recommended) => Sort::Recommended,
            Some(EntitySort::Trending | EntitySort::Popular) => Sort::Popular,
            Some(EntitySort::TopRated | EntitySort::Rating) => Sort::Rating,
            Some(EntitySort::New) => Sort::New,
            Some(EntitySort::Soon) => Sort::Soon,
            Some(EntitySort::Title) => Sort::Title,
        },
        kinds: vec![EntityKind::Book],
        tags: tag.into_iter().collect(),
        year_from: year,
        year_to: year,
        forms: BookForm::ALL.to_vec(),
        ..Query::default()
    }
}

/// Сколько книг под запрос.
pub async fn count(state: &AppState, query: Query) -> AppResult<usize> {
    crate::index::search::count(state, query).await
}

/// Соседи книги `slug` в выдаче `query`. Книги в ряду нет (или ряд слишком длинный) — оба края
/// `None`.
pub async fn neighbors(
    state: &AppState,
    query: Option<Query>,
    slug: String,
) -> AppResult<Neighbors> {
    match query {
        Some(query) => crate::index::search::neighbors(state, query, slug).await,
        None => Ok(Neighbors {
            prev: None,
            next: None,
        }),
    }
}

/// Похожие книги: есть хотя бы один из тегов `tags`, кроме `exclude`; сверху — «Рекомендуемые».
pub async fn similar(
    state: &AppState,
    tags: &[String],
    exclude: &[String],
    limit: i64,
) -> AppResult<(Vec<EntitySummary>, i64)> {
    crate::index::search::similar(state, EntityKind::Book, tags, exclude, limit).await
}

/// Теги и жанры книг со счётчиками, самые частые сверху: подсказки `GET /search/tags`.
pub async fn tags(state: &AppState, limit: Option<i64>) -> AppResult<Vec<(String, i64)>> {
    let limit = limit.unwrap_or(10).clamp(1, MAX_TAG_HITS as i64) as usize;
    crate::index::search::facet(state, "tags", "", Some(EntityKind::Book), limit).await
}

/// Издательства по части названия: подсказки `GET /search/publishers`.
pub async fn publishers(
    state: &AppState,
    q: &str,
    limit: Option<i64>,
) -> AppResult<Vec<(String, i64)>> {
    let limit = limit.unwrap_or(10).clamp(1, 50) as usize;
    crate::index::search::facet(state, "publishers", q, Some(EntityKind::Book), limit).await
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
        assert_eq!(q.sort, Sort::Rating);
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
