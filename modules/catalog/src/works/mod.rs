//! Поиск работ (фильмы, сериалы, игры, картины) на Tantivy: индекс `works`
//! (`<SEARCH_INDEX_DIR>/works.v<ms>`). Схема, запросы и
//! сортировки — [`index`], наполнение — [`sync`], ответы API — [`search`].

pub mod index;
pub mod search;
pub mod sync;
