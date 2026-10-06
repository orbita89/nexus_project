//! Типы БД и DTO социального модуля.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use shared::directory::{EntityRef, UserRef};
use shared::pagination::Page;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

// ---------------------------------------------------------------- рецензии

/// Рецензия: оценка, текст или и то и другое.
#[derive(Debug, Serialize, ToSchema)]
pub struct Review {
    pub id: Uuid,
    pub author: UserRef,
    pub entity: EntityRef,
    /// 1–10.
    #[schema(example = 8)]
    pub rating: Option<i16>,
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct ReviewRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub entity_id: Uuid,
    pub rating: Option<i16>,
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const REVIEW_COLUMNS: &str = "id, user_id, entity_id, rating, body, created_at, updated_at";

/// Своя рецензия: заменяется целиком. Нужна хотя бы оценка или текст.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutReview {
    /// 1–10.
    #[schema(example = 9)]
    pub rating: Option<i16>,
    /// До 10 000 символов. Пустой — без текста.
    #[schema(example = "Визуально — шедевр.")]
    pub body: Option<String>,
}

/// Сводка оценок сущности. Учитываются все рецензии с оценкой, в том числе без текста.
#[derive(Debug, Serialize, ToSchema)]
pub struct RatingSummary {
    /// Средняя оценка, округлена до 0.1. `null` — оценок нет.
    #[schema(example = 8.3)]
    pub average: Option<f64>,
    /// Сколько оценок.
    #[schema(example = 12)]
    pub count: i64,
    /// Число оценок 1, 2, ..., 10: всегда 10 элементов.
    #[schema(example = json!([0, 0, 0, 0, 1, 0, 2, 3, 4, 2]))]
    pub distribution: Vec<i64>,
}

/// Порядок рецензий сущности.
#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSort {
    /// Новые сверху.
    #[default]
    New,
    /// Высокие оценки сверху, без оценки — в конце.
    RatingDesc,
    /// Низкие оценки сверху, без оценки — в конце.
    RatingAsc,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListReviewsQuery {
    /// По умолчанию `new`.
    pub sort: Option<ReviewSort>,
    /// `true` — включая оценки без текста. По умолчанию только рецензии с текстом.
    pub all: Option<bool>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageQuery {
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------- подписки

/// Подписчик или подписка.
#[derive(Debug, Serialize, ToSchema)]
pub struct Follow {
    pub user: UserRef,
    /// Когда подписался.
    pub since: DateTime<Utc>,
}

// ---------------------------------------------------------------- профиль

/// Социальный профиль пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct SocialProfile {
    pub user: UserRef,
    #[schema(example = 12)]
    pub followers_count: i64,
    #[schema(example = 3)]
    pub following_count: i64,
    /// Все рецензии, включая оценки без текста.
    #[schema(example = 40)]
    pub reviews_count: i64,
    /// Публичные коллекции; владельцу — вместе с приватными.
    #[schema(example = 2)]
    pub collections_count: i64,
    /// Темы форума.
    #[schema(example = 3)]
    pub threads_count: i64,
    /// Сообщения на форуме, без удалённых.
    #[schema(example = 25)]
    pub posts_count: i64,
    /// Отношение вошедшего к пользователю. `null` — гость или свой профиль.
    pub relation: Option<Relation>,
}

/// Подписки между вошедшим и пользователем профиля.
#[derive(Debug, Serialize, ToSchema)]
pub struct Relation {
    /// Вошедший подписан на пользователя.
    pub following: bool,
    /// Пользователь подписан на вошедшего.
    pub followed_by: bool,
}

// ---------------------------------------------------------------- коллекции

/// Коллекция в списке.
#[derive(Debug, Serialize, ToSchema)]
pub struct Collection {
    pub id: Uuid,
    pub owner: UserRef,
    #[schema(example = "Лучшая фантастика")]
    pub title: String,
    pub description: Option<String>,
    /// `false` — видна только владельцу.
    pub is_public: bool,
    /// Сколько произведений.
    #[schema(example = 7)]
    pub items_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CollectionRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub is_public: bool,
    pub items_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Колонки [`CollectionRow`] для `FROM collections c`.
pub const COLLECTION_COLUMNS: &str = "c.id, c.user_id, c.title, c.description, c.is_public,
    (SELECT count(*) FROM collection_items i WHERE i.collection_id = c.id) AS items_count,
    c.created_at, c.updated_at";

/// Коллекция с содержимым по порядку.
#[derive(Debug, Serialize, ToSchema)]
pub struct CollectionDetail {
    #[serde(flatten)]
    pub collection: Collection,
    pub items: Vec<CollectionItem>,
}

/// Произведение в коллекции.
#[derive(Debug, Serialize, ToSchema)]
pub struct CollectionItem {
    pub entity: EntityRef,
    /// Порядок: по возрастанию, при равенстве — по времени добавления.
    pub position: i32,
    /// Комментарий владельца.
    pub note: Option<String>,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct CollectionItemRow {
    pub entity_id: Uuid,
    pub position: i32,
    pub note: Option<String>,
    pub added_at: DateTime<Utc>,
}

pub const ITEM_COLUMNS: &str = "entity_id, position, note, added_at";

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCollection {
    /// 1–200 символов.
    #[schema(example = "Лучшая фантастика")]
    pub title: String,
    /// До 2000 символов.
    pub description: Option<String>,
    /// По умолчанию `true`.
    pub is_public: Option<bool>,
}

/// Изменение коллекции: переданные поля заменяются, `null` очищает описание.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateCollection {
    pub title: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    pub is_public: Option<bool>,
}

/// Добавить произведение в коллекцию или изменить пункт.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutCollectionItem {
    /// Новый пункт без `position` встаёт в конец; у существующего без `position` порядок не меняется.
    pub position: Option<i32>,
    /// До 1000 символов. Не передан — не меняется, `null` — очищается.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub note: Option<Option<String>>,
}

/// Новый порядок: slug'и всех произведений коллекции, каждое ровно один раз.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReorderItems {
    #[schema(example = json!(["dune-novel", "dune-2021", "dune-part-two-2024"]))]
    pub entities: Vec<String>,
}

// ---------------------------------------------------------------- форум

/// Тема в списке: без текста и сообщений.
#[derive(Debug, Serialize, ToSchema)]
pub struct Thread {
    pub id: Uuid,
    pub author: UserRef,
    #[schema(example = "Дюна: книга против фильма")]
    pub title: String,
    /// Сущности темы по порядку: первая — главная.
    pub entities: Vec<EntityRef>,
    /// Сообщений в теме, без удалённых.
    #[schema(example = 14)]
    pub posts_count: i32,
    /// Последнее сообщение; у темы без ответов — время создания.
    pub last_post_at: DateTime<Utc>,
    /// Закрыта для ответов.
    pub is_locked: bool,
    /// Когда автор правил тему; `null` — не правил.
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct ThreadRow {
    pub id: Uuid,
    pub author_id: Uuid,
    pub title: String,
    pub body: String,
    pub posts_count: i32,
    pub last_post_at: DateTime<Utc>,
    pub is_locked: bool,
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Колонки [`ThreadRow`] для `FROM forum_threads t`.
pub const THREAD_COLUMNS: &str = "t.id, t.author_id, t.title, t.body, t.posts_count,
    t.last_post_at, t.is_locked, t.edited_at, t.created_at";

/// Тема с текстом и страницей сообщений.
#[derive(Debug, Serialize, ToSchema)]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub thread: Thread,
    #[schema(example = "Сравниваем роман Херберта и фильм Вильнёва.")]
    pub body: String,
    /// Сообщения по времени, включая заглушки удалённых, на которые есть ответы.
    pub posts: Page<Post>,
}

/// Сообщение в теме. Ветки — через `parent_id`: список плоский, по времени.
#[derive(Debug, Serialize, ToSchema)]
pub struct Post {
    pub id: Uuid,
    pub thread_id: Uuid,
    /// На какое сообщение ответ; `null` — ответ на тему.
    pub parent_id: Option<Uuid>,
    /// Автор сообщения, на которое ответ; `null` — ответ на тему или то сообщение удалено.
    pub reply_to: Option<UserRef>,
    /// `null` у удалённого.
    pub author: Option<UserRef>,
    /// `null` у удалённого.
    #[schema(example = "У Херберта Пол куда мрачнее.")]
    pub body: Option<String>,
    /// Сообщение удалено, но на него есть ответы: показывается как «сообщение удалено».
    pub deleted: bool,
    /// Когда автор правил сообщение; `null` — не правил.
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct PostRow {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub author_id: Uuid,
    pub parent_id: Option<Uuid>,
    /// Автор родителя, если тот не удалён.
    pub parent_author_id: Option<Uuid>,
    pub body: Option<String>,
    pub edited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Колонки [`PostRow`] для `FROM forum_posts p`.
pub const POST_COLUMNS: &str = "p.id, p.thread_id, p.author_id, p.parent_id,
    (SELECT parent.author_id FROM forum_posts parent
     WHERE parent.id = p.parent_id AND parent.deleted_at IS NULL) AS parent_author_id,
    p.body, p.edited_at, p.created_at";

/// Порядок тем.
#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSort {
    /// Недавние сообщения сверху.
    #[default]
    Active,
    /// Новые темы сверху.
    New,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListThreadsQuery {
    /// По умолчанию `active`.
    pub sort: Option<ThreadSort>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PostsQuery {
    /// Сообщений на странице: 1–100, по умолчанию 50.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

/// Новая тема.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateThread {
    /// 1–200 символов.
    #[schema(example = "Дюна: книга против фильма")]
    pub title: String,
    /// 1–20 000 символов.
    #[schema(example = "Сравниваем роман Херберта и фильм Вильнёва.")]
    pub body: String,
    /// slug'и сущностей, 1–10 без повторов. Первая — главная.
    #[schema(example = json!(["dune-novel", "dune-2021"]))]
    pub entities: Vec<String>,
}

/// Изменение темы: переданные поля заменяются, `entities` — набор целиком.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateThread {
    pub title: Option<String>,
    pub body: Option<String>,
    #[schema(example = json!(["dune-novel", "dune-2021", "dune-part-two-2024"]))]
    pub entities: Option<Vec<String>>,
}

/// Новое сообщение в теме.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePost {
    /// 1–10 000 символов.
    #[schema(example = "У Херберта Пол куда мрачнее.")]
    pub body: String,
    /// Ответ на сообщение этой темы; не передан — ответ на тему.
    pub parent_id: Option<Uuid>,
}

/// Изменение своего сообщения.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePost {
    /// 1–10 000 символов.
    pub body: String,
}

// ---------------------------------------------------------------- интересы

/// Сущность в интересах пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct Interest {
    pub entity: EntityRef,
    /// Когда добавлена.
    pub since: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct InterestRow {
    pub entity_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Отличает «поле не передано» (`None`) от `null` (`Some(None)`).
fn nullable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
