//! Типы единого индекса: запрос, сортировка, документы слоя А и слоя Б.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use shared::tantivy::normalize_key;
use uuid::Uuid;

use crate::models::{EntityKind, EntitySummary, ReleaseStatus, SummaryAuthor};

/// Вид текста у книги. Обычная выдача — только книги; остальное — по фильтру `book_form`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BookForm {
    #[default]
    Book,
    /// Несколько разных произведений в одном файле (`metadata.collection`).
    Collection,
    /// Статья, рецензия, интервью (`metadata.kind = article`).
    Article,
    /// Журнал, газета, документ (`metadata.kind = document`).
    Document,
}

impl BookForm {
    pub const ALL: [BookForm; 4] = [Self::Book, Self::Collection, Self::Article, Self::Document];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Book => "book",
            Self::Collection => "collection",
            Self::Article => "article",
            Self::Document => "document",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == value)
    }
}

/// Сортировка выдачи сущностей.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Sort {
    /// По совпадению с запросом; без запроса — как [`Sort::Recommended`].
    #[default]
    Relevance,
    Recommended,
    Trending,
    TopRated,
    /// Только вышедшие, новые сверху.
    New,
    /// Только не вышедшие, ближайшие сверху.
    Soon,
    Title,
    /// Больше оценок Nexus — выше.
    Popular,
    /// Выше средняя оценка Nexus.
    Rating,
    /// Больше наград — выше.
    Awarded,
}

/// Запрос к индексу сущностей: текст, фильтры (через И) и сортировка. Пустые фильтры не
/// действуют.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Query {
    pub q: String,
    pub sort: Sort,
    /// Виды (ИЛИ). Пусто — все.
    pub kinds: Vec<EntityKind>,
    /// Хотя бы один из тегов или жанров (ИЛИ).
    pub tags: Vec<String>,
    /// Коды стран (ИЛИ).
    pub countries: Vec<String>,
    pub year_from: Option<i32>,
    pub year_to: Option<i32>,
    /// Студия — название без учёта регистра и знаков.
    pub studio: Option<String>,
    pub studio_slug: Option<String>,
    /// Издатель (издательство, издатель и разработчик игры) — как студия.
    pub publisher: Option<String>,
    pub museum: Option<String>,
    pub award: Option<String>,
    pub award_year: Option<i32>,
    pub awarded: Option<bool>,
    /// Slug человека и роль (`None` — любая): термы `person:`/`role:` поля `facet` у всех видов.
    pub person: Option<String>,
    pub role: Option<String>,
    /// Только эти номера.
    pub only_ords: Option<Vec<u32>>,
    /// Кроме этих номеров (похожие).
    pub exclude_ords: Vec<u32>,
    pub has_cover: Option<bool>,
    /// Код языка книги (`ru`).
    pub language: Option<String>,
    /// Виды текста у книг (ИЛИ). Пусто — только [`BookForm::Book`].
    pub forms: Vec<BookForm>,
}

/// Страница выдачи.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub items: Vec<EntitySummary>,
    pub total: usize,
    /// Строгий запрос ничего не нашёл — выдача собрана с исправлением опечатки.
    pub approximate: bool,
}

/// Строка выдачи, как она хранится в индексе (поле `card`).
#[derive(Debug, Serialize, Deserialize)]
pub struct Card {
    pub id: Uuid,
    pub kind: EntityKind,
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<NaiveDate>,
    /// У книг не хранится: «вышла» не должно зависеть от дня сборки индекса.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_status: Option<ReleaseStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_height: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb_rating: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb_votes: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<SummaryAuthor>,
}

impl Card {
    pub fn summary(self, today: NaiveDate) -> EntitySummary {
        EntitySummary {
            id: self.id,
            kind: self.kind,
            slug: self.slug,
            title: self.title,
            original_title: self.original_title,
            release_status: self.release_status.unwrap_or(match self.release_date {
                Some(date) if date > today => ReleaseStatus::Announced,
                _ => ReleaseStatus::Released,
            }),
            release_date: self.release_date,
            cover_url: self.cover_url,
            cover_width: self.cover_width,
            cover_height: self.cover_height,
            imdb_rating: self.imdb_rating,
            imdb_votes: self.imdb_votes,
            authors: self.authors,
        }
    }
}

/// Документ слоя А: всё, что кладётся в Tantivy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDoc {
    pub kind: &'static str,
    /// Значение колонки `ord` ([`super::collect::Space::raw`]).
    pub ord: u64,
    /// Название, оригинальное название, синонимы.
    pub titles: Vec<String>,
    pub people: Vec<String>,
    pub extra: Vec<String>,
    /// Термы поля `facet` ([`super::sidecar::text_facets`]).
    pub facets: Vec<String>,
    pub card: String,
}

impl TextDoc {
    /// Отпечаток того, что документ кладёт в Tantivy. Совпал с прошлым
    /// (`text_hash` в таблицах номеров) — переписывать документ незачем: оценки, теги и места идут
    /// только в слой Б. FNV-1a: стабилен между версиями Rust (хранится в БД).
    pub fn text_hash(&self) -> i64 {
        let text = serde_json::to_string(&(
            self.kind,
            self.ord,
            &self.titles,
            &self.people,
            &self.extra,
            &self.facets,
            &self.card,
        ))
        .unwrap_or_default();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in text.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash as i64
    }
}

/// Сущность для индекса: слой А ([`EntityDoc::text`]) и слой Б. Собирается из БД
/// ([`super::sync`]).
#[derive(Debug, Clone, Default)]
pub struct EntityDoc {
    pub id: Uuid,
    /// Плотный номер сущности (`catalog_entity_ords.ord`).
    pub ord: u32,
    pub kind: EntityKind,
    pub slug: String,
    pub title: String,
    pub original_title: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub release_status: ReleaseStatus,
    pub cover_url: Option<String>,
    pub cover_width: Option<i32>,
    pub cover_height: Option<i32>,
    pub imdb_rating: Option<f64>,
    pub imdb_votes: Option<i32>,
    /// Авторы книги по порядку (в карточку выдачи).
    pub authors: Vec<SummaryAuthor>,
    /// Имена для текстового поля `people`: авторы книги, режиссёры и первые в титрах.
    pub people: Vec<String>,
    /// Серия книги, студии: поле `extra`.
    pub extra: Vec<String>,
    /// Ручное место в общем списке (`catalog_manual_ranks`), `0` — нет.
    pub manual_rank: u16,
    pub has_description: bool,
    /// Сколько работ у самого плодовитого из авторов книги: мера известности автора.
    pub author_works: u32,
    /// Участники книги: роль и slug человека (фильтр `person` + `role`). У остальных видов пусто.
    pub credits: Vec<(String, String)>,
    pub tags: Vec<String>,
    pub countries: Vec<String>,
    /// Названия студий и slug'и их страниц.
    pub studios: Vec<String>,
    pub studio_slugs: Vec<String>,
    /// Издатели (издательство книги, издатель и разработчик игры).
    pub publishers: Vec<String>,
    pub museums: Vec<String>,
    pub award_slugs: Vec<String>,
    pub award_years: Vec<i32>,
    pub award_wins: i64,
    pub language: Option<String>,
    pub form: BookForm,
    /// Оценки Nexus: число и средняя.
    pub ratings_count: i64,
    pub rating_avg: Option<f64>,
    /// Статистика (`entity_stats`).
    pub rank_score: Option<f64>,
    pub popularity: Option<f64>,
    pub quality: Option<f64>,
}

pub(crate) fn day_of(date: NaiveDate) -> u64 {
    date.num_days_from_ce().max(0) as u64
}

/// Ключ сортировки по названию: первые 9 знаков ключа названия, по 7 бит — порядок «пробел,
/// кириллица, латиница, цифры» (русский сайт: А–Я, затем A–Z, затем «1984», «0%»). Названия с общим началом длиннее 9 знаков идут в порядке номеров.
/// Знаки в начале («...», «"») пропускаются; название без букв и цифр («=|=») — в самом конце.
pub(crate) fn title_sort_key(title: &str) -> u64 {
    let norm = normalize_key(title);
    let start = norm.trim_start_matches(|c: char| !c.is_alphanumeric());
    if start.is_empty() {
        return u64::MAX;
    }
    let mut key = 0u64;
    let mut chars = start.chars().collect::<Vec<_>>().into_iter();
    for _ in 0..9 {
        let code = match chars.next() {
            None => 0,
            Some(' ') => 1,
            Some(c @ 'а'..='я') => 2 + (c as u64 - 'а' as u64),
            Some(c @ 'a'..='z') => 34 + (c as u64 - 'a' as u64),
            Some(c @ '0'..='9') => 60 + (c as u64 - '0' as u64),
            Some(_) => 70,
        };
        key = (key << 7) | code;
    }
    key
}

impl EntityDoc {
    pub fn card(&self) -> Card {
        Card {
            id: self.id,
            kind: self.kind,
            slug: self.slug.clone(),
            title: self.title.clone(),
            original_title: self.original_title.clone(),
            release_date: self.release_date,
            release_status: (self.kind != EntityKind::Book).then_some(self.release_status),
            cover_url: self.cover_url.clone(),
            cover_width: self.cover_width,
            cover_height: self.cover_height,
            imdb_rating: self.imdb_rating,
            imdb_votes: self.imdb_votes,
            authors: self.authors.clone(),
        }
    }

    /// Слой А: название, оригинальное название, люди, серия/студии и карточка.
    pub fn text(&self) -> TextDoc {
        TextDoc {
            kind: self.kind.as_str(),
            ord: u64::from(self.ord),
            titles: std::iter::once(self.title.clone())
                .chain(self.original_title.clone())
                .collect(),
            people: self.people.clone(),
            extra: self.extra.clone(),
            facets: super::sidecar::text_facets(self),
            card: serde_json::to_string(&self.card()).unwrap_or_default(),
        }
    }

    /// Балл «Рекомендуемые» без ручного места. Книги: оценки Nexus, затем обложка, аннотация и
    /// автор, затем известность автора, затем год (своей статистики, как у фильмов, у них нет).
    /// Остальные: `rank_score` (TMDB, IMDb, награды), при равенстве — новее.
    pub(crate) fn base_rank(&self) -> u64 {
        base_rank(self.rank_score, self.release_date.map_or(0, day_of))
    }

    /// Популярность для релевантности поиска: балл «Рекомендуемые» вида (`crate::rank_kinds`).
    pub(crate) fn popularity_meta(&self) -> u32 {
        popularity_meta(self.rank_score)
    }
}

/// Балл «Рекомендуемые» с датой выхода (день) — ключ порядка: балл выше, затем новее.
pub(crate) fn base_rank(rank_score: Option<f64>, day: u64) -> u64 {
    (scaled(rank_score, 1000.0) << 32) | (day & 0xFFFF_FFFF)
}

/// Балл в целое для [`super::sidecar::SortMeta::popularity`] (балл — дробь, без масштаба был бы 0).
pub(crate) fn popularity_meta(rank_score: Option<f64>) -> u32 {
    scaled(rank_score, 1_000_000.0) as u32
}

/// Неотрицательное число в целое с масштабом; нет значения — `0`, потолок — `u32::MAX`.
pub(crate) fn scaled(value: Option<f64>, scale: f64) -> u64 {
    (value.unwrap_or(0.0).max(0.0) * scale).min(f64::from(u32::MAX)) as u64
}

impl Default for EntityKind {
    fn default() -> Self {
        Self::Book
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_sort_orders_digits_latin_cyrillic() {
        assert!(title_sort_key("Азбука") < title_sort_key("Яблоко"));
        assert!(title_sort_key("Azbuka") < title_sort_key("1984"));
        assert!(title_sort_key("Яблоко") < title_sort_key("Zebra"));
        // Знаки в начале не считаются; без букв и цифр — в конце.
        assert_eq!(title_sort_key("«Азбука»"), title_sort_key("Азбука"));
        assert!(title_sort_key("Яблоко") < title_sort_key("=|="));
        assert_eq!(title_sort_key("=|="), u64::MAX);
    }

    #[test]
    fn text_hash_changes_with_text_only() {
        let mut doc = EntityDoc {
            title: "Дюна".into(),
            slug: "dune".into(),
            ..EntityDoc::default()
        };
        let before = doc.text().text_hash();
        doc.ratings_count = 5;
        doc.tags = vec!["fantastika".into()];
        assert_eq!(doc.text().text_hash(), before);
        doc.title = "Дюна 2".into();
        assert_ne!(doc.text().text_hash(), before);
    }
}
