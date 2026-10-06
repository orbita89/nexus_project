//! Пагинация списков: `?limit=&offset=` и ответ `{items, total, limit, offset}`.

use serde::Serialize;
use utoipa::ToSchema;

/// Страница списка.
#[derive(Debug, Serialize, ToSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Всего записей под фильтром (в поиске — оценка Meilisearch).
    #[schema(example = 137)]
    pub total: i64,
    #[schema(example = 20)]
    pub limit: i64,
    #[schema(example = 0)]
    pub offset: i64,
}

pub const DEFAULT_PAGE_SIZE: i64 = 20;
pub const MAX_PAGE_SIZE: i64 = 100;

/// `limit` (1–100, по умолчанию 20) и `offset` (с 0).
pub fn page_bounds(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE),
        offset.unwrap_or(0).max(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds() {
        assert_eq!(page_bounds(None, None), (20, 0));
        assert_eq!(page_bounds(Some(1000), Some(-5)), (100, 0));
        assert_eq!(page_bounds(Some(0), Some(7)), (1, 7));
    }
}
