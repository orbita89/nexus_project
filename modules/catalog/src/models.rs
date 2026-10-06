//! Типы БД и DTO каталога.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Тип сущности (Postgres enum `entity_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "entity_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Movie,
    Series,
    Book,
    Game,
}

impl EntityKind {
    pub const ALL: [EntityKind; 4] = [Self::Movie, Self::Series, Self::Book, Self::Game];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
            Self::Book => "book",
            Self::Game => "game",
        }
    }
}

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

/// Сущность в списке.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct EntitySummary {
    pub id: Uuid,
    pub kind: EntityKind,
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    #[schema(example = "Dune")]
    pub original_title: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
}

pub const ENTITY_SUMMARY_COLUMNS: &str =
    "e.id, e.kind, e.slug, e.title, e.original_title, e.release_date, e.cover_url";

/// Сущность целиком (строка `entities`).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Entity {
    pub id: Uuid,
    pub kind: EntityKind,
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    #[schema(example = "Dune")]
    pub original_title: Option<String>,
    pub description: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
    /// Поля, специфичные для `kind`.
    #[schema(value_type = crate::metadata::Metadata)]
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const ENTITY_COLUMNS: &str = "e.id, e.kind, e.slug, e.title, e.original_title, e.description, \
     e.release_date, e.cover_url, e.metadata, e.created_at, e.updated_at";

/// Карточка: сущность, теги и участники в порядке титров.
#[derive(Debug, Serialize, ToSchema)]
pub struct EntityDetail {
    #[serde(flatten)]
    pub entity: Entity,
    pub tags: Vec<Tag>,
    pub credits: Vec<EntityCredit>,
}

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Tag {
    pub id: Uuid,
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct TagWithCount {
    pub id: Uuid,
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
    /// Сколько сущностей с этим тегом.
    #[schema(example = 12)]
    pub entities_count: i64,
}

/// Человек в титрах сущности.
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonRef {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub photo_url: Option<String>,
}

/// Участие человека в сущности (строка `entity_credits` + человек).
#[derive(Debug, Serialize, ToSchema)]
pub struct EntityCredit {
    /// id записи участия: по нему участие удаляется.
    pub id: Uuid,
    #[schema(example = "director")]
    pub role: String,
    #[schema(example = "Пол Атрейдес")]
    pub character_name: Option<String>,
    pub position: i32,
    pub person: PersonRef,
}

#[derive(sqlx::FromRow)]
pub struct EntityCreditRow {
    pub id: Uuid,
    pub role: String,
    pub character_name: Option<String>,
    pub position: i32,
    pub person_id: Uuid,
    pub person_slug: String,
    pub person_full_name: String,
    pub person_photo_url: Option<String>,
}

impl From<EntityCreditRow> for EntityCredit {
    fn from(row: EntityCreditRow) -> Self {
        Self {
            id: row.id,
            role: row.role,
            character_name: row.character_name,
            position: row.position,
            person: PersonRef {
                id: row.person_id,
                slug: row.person_slug,
                full_name: row.person_full_name,
                photo_url: row.person_photo_url,
            },
        }
    }
}

/// Человек в списке.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct PersonSummary {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
}

/// Человек целиком (строка `people`).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Person {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
    pub bio: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const PERSON_COLUMNS: &str =
    "p.id, p.slug, p.full_name, p.birth_date, p.photo_url, p.bio, p.created_at, p.updated_at";

/// Человек и его фильмография (новые работы сверху).
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonDetail {
    #[serde(flatten)]
    pub person: Person,
    pub credits: Vec<PersonCredit>,
}

/// Участие человека в сущности, со стороны человека.
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonCredit {
    pub id: Uuid,
    #[schema(example = "director")]
    pub role: String,
    pub character_name: Option<String>,
    pub entity: EntitySummary,
}

#[derive(sqlx::FromRow)]
pub struct PersonCreditRow {
    pub id: Uuid,
    pub role: String,
    pub character_name: Option<String>,
    #[sqlx(flatten)]
    pub entity: EntitySummary,
}

impl From<PersonCreditRow> for PersonCredit {
    fn from(row: PersonCreditRow) -> Self {
        Self {
            id: row.id,
            role: row.role,
            character_name: row.character_name,
            entity: row.entity,
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListEntitiesQuery {
    /// Тип сущности.
    pub kind: Option<EntityKind>,
    /// Slug тега.
    #[param(example = "sci-fi")]
    pub tag: Option<String>,
    /// Год выхода.
    #[param(example = 2021)]
    pub year: Option<i32>,
    /// Подстрока в названии или оригинальном названии, без учёта регистра.
    #[param(example = "дюна")]
    pub q: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListPeopleQuery {
    /// Подстрока в имени, без учёта регистра.
    #[param(example = "кинг")]
    pub q: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    /// Поисковый запрос: название, оригинальное название, люди, теги, описание. Опечатки допустимы.
    #[param(example = "дюан")]
    pub q: Option<String>,
    pub kind: Option<EntityKind>,
    /// Slug тега.
    pub tag: Option<String>,
    pub year: Option<i32>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

// ------------------------------------------------------------------ admin

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEntity {
    pub kind: EntityKind,
    /// Латиница в нижнем регистре, цифры и дефисы.
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    #[schema(example = "Dune")]
    pub original_title: Option<String>,
    pub description: Option<String>,
    pub release_date: Option<NaiveDate>,
    /// `http(s)://...`
    pub cover_url: Option<String>,
    /// Поля для `kind`. По умолчанию `{}`.
    #[schema(value_type = Option<crate::metadata::Metadata>)]
    pub metadata: Option<Value>,
    /// Slug'и существующих тегов.
    #[serde(default)]
    #[schema(example = json!(["sci-fi"]))]
    pub tags: Vec<String>,
}

/// Изменение сущности: переданные поля заменяются, `null` очищает необязательное поле.
/// `kind` не меняется. `metadata` заменяется целиком.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateEntity {
    pub slug: Option<String>,
    pub title: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub original_title: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<NaiveDate>)]
    pub release_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub cover_url: Option<Option<String>>,
    #[schema(value_type = Option<crate::metadata::Metadata>)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTags {
    /// Slug'и тегов: заменяют текущий набор целиком.
    #[schema(example = json!(["sci-fi", "space-opera"]))]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddCredit {
    pub person_id: Uuid,
    /// `actor`, `director`, `writer`, `author`, `composer`, `developer`, ...
    /// (латиница в нижнем регистре и `_`).
    #[schema(example = "actor")]
    pub role: String,
    #[schema(example = "Пол Атрейдес")]
    pub character_name: Option<String>,
    /// Порядок в титрах, по умолчанию 0.
    pub position: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePerson {
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
    pub bio: Option<String>,
}

/// Изменение человека: переданные поля заменяются, `null` очищает необязательное поле.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePerson {
    pub slug: Option<String>,
    pub full_name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<NaiveDate>)]
    pub birth_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub photo_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub bio: Option<Option<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTag {
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTag {
    pub slug: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReindexResult {
    /// Сколько сущностей в новом индексе.
    #[schema(example = 45)]
    pub indexed: usize,
}

/// Отличает отсутствующее поле (`None`) от `null` (`Some(None)`). Вместе с `#[serde(default)]`.
fn nullable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
