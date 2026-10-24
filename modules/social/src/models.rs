//! Типы БД и DTO социального модуля.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use shared::directory::{EntityRef, PersonRef, StudioRef, TagRef, UserRef};
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
    /// В тексте есть спойлеры: фронтенд размывает его, пока читатель не откроет.
    pub is_spoiler: bool,
    /// Лайки. Свой голос — `GET /entities/{slug}/votes/mine`.
    #[schema(example = 12)]
    pub likes_count: i64,
    #[schema(example = 1)]
    pub dislikes_count: i64,
    /// Комментарии, без удалённых.
    #[schema(example = 3)]
    pub comments_count: i64,
    pub created_at: DateTime<Utc>,
    /// Когда автор правил рецензию (голоса и комментарии не считаются).
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct ReviewRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub entity_id: Uuid,
    pub rating: Option<i16>,
    pub body: Option<String>,
    pub is_spoiler: bool,
    pub likes_count: i64,
    pub dislikes_count: i64,
    pub comments_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const REVIEW_COLUMNS: &str = "id, user_id, entity_id, rating, body, is_spoiler, \
     likes_count, dislikes_count, comments_count, created_at, updated_at";

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
    /// В тексте есть спойлеры. По умолчанию `false`.
    #[serde(default)]
    pub is_spoiler: bool,
}

/// Комментарий к рецензии.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReviewComment {
    pub id: Uuid,
    pub review_id: Uuid,
    pub author: UserRef,
    #[schema(example = "Согласен, звук в IMAX — отдельное удовольствие.")]
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct ReviewCommentRow {
    pub id: Uuid,
    pub review_id: Uuid,
    pub author_id: Uuid,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const COMMENT_COLUMNS: &str = "id, review_id, author_id, body, created_at, updated_at";

/// Новый комментарий к рецензии.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateReviewComment {
    /// 1–2000 символов.
    #[schema(example = "Согласен, звук в IMAX — отдельное удовольствие.")]
    pub body: String,
}

/// Голос за рецензию: `1` — лайк, `-1` — дизлайк.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutVote {
    #[schema(example = 1)]
    pub value: i16,
}

/// Счётчики рецензии после голоса и свой голос (`null` — нет).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct VoteResult {
    #[schema(example = 13)]
    pub likes_count: i64,
    #[schema(example = 1)]
    pub dislikes_count: i64,
    #[schema(example = 1)]
    pub value: Option<i16>,
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
    /// Реакции зрителей. Своя — `GET /entities/{slug}/reaction`.
    pub reactions: Reactions,
}

/// Сколько зрителей выбрали каждую реакцию: всегда все четыре ключа.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct Reactions {
    /// 🔥
    #[schema(example = 12)]
    pub fire: i64,
    /// 😢
    #[schema(example = 3)]
    pub sad: i64,
    /// 🤔
    #[schema(example = 5)]
    pub think: i64,
    /// 😴
    #[schema(example = 1)]
    pub bored: i64,
}

/// Реакция на произведение: 🔥 `fire`, 😢 `sad`, 🤔 `think`, 😴 `bored`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ReactionKind {
    Fire,
    Sad,
    Think,
    Bored,
}

/// Своя реакция.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Reaction {
    pub kind: ReactionKind,
}

/// Оценка сущности в списке: средняя (округлена до 0.1, `null` — оценок нет) и сколько оценок.
#[derive(Debug, Serialize, ToSchema)]
pub struct RatingBrief {
    #[schema(example = 8.3)]
    pub average: Option<f64>,
    #[schema(example = 12)]
    pub count: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RatingsQuery {
    /// slug'и через запятую, до 100.
    #[param(example = "dune-2021,dune-1984")]
    pub slugs: String,
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

/// Лайк профиля.
#[derive(Debug, Serialize, ToSchema)]
pub struct Like {
    pub user: UserRef,
    /// Когда лайкнул.
    pub since: DateTime<Utc>,
}

// ---------------------------------------------------------------- профиль

/// Социальный профиль пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct SocialProfile {
    pub user: UserRef,
    /// «О себе» из профиля (`auth`), до 1000 символов.
    pub bio: Option<String>,
    /// Открыл ли пользователь свою аналитику всем. В своём профиле — своё значение.
    pub stats_public: bool,
    /// Когда зарегистрировался.
    pub registered_at: chrono::DateTime<chrono::Utc>,
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
    /// Сколько лайков у профиля (дизлайков нет).
    #[schema(example = 17)]
    pub likes_count: i64,
    /// Лайкнул ли вошедший этот профиль. Гостю и в своём профиле — `false`.
    pub liked: bool,
    /// Отношение вошедшего к пользователю. `null` — гость или свой профиль.
    pub relation: Option<Relation>,
    /// Вошедший заблокировал этого пользователя. Гостю и в своём профиле — `false`.
    pub blocked: bool,
    /// Этот пользователь заблокировал вошедшего; `null` — гость или свой профиль.
    /// По паре `blocked` + `blocked_by` фронтенд решает, показывать ли «Написать».
    pub blocked_by: Option<bool>,
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
    /// Особая коллекция (помечена admin): показывается на главной и выглядит иначе.
    pub is_featured: bool,
    /// Порядок среди особых; у обычной — `null`.
    pub featured_position: Option<i32>,
    /// Читаемый адрес особой коллекции (`/collections/{slug}`); у обычной — `null`.
    #[schema(example = "russkiy-top-2021")]
    pub slug: Option<String>,
    /// Обои особой: широкий фон шапки страницы (21:9 или 16:9, от 1920 px); у обычной — `null`.
    pub wallpaper_url: Option<String>,
    /// Обложка карточки особой (16:9, от 1280 px); у обычной — `null`.
    pub cover_url: Option<String>,
    /// Тема оформления особой — id пресета фронтенда (`halloween`, `new-year`, ...); неизвестную
    /// фронтенд игнорирует. У обычной — `null`.
    #[schema(example = "halloween")]
    pub theme: Option<String>,
    /// Где обои: `header` — только в шапке, `page` — фоном всей страницы.
    #[schema(example = "header")]
    pub wallpaper_mode: String,
    /// Затемнение обоев, % (0–100); `null` — по теме на фронтенде.
    pub wallpaper_dim: Option<i16>,
    /// Размытие обоев, px (0–20).
    pub wallpaper_blur: i16,
    /// Декоративные эффекты темы (капли крови, снег…).
    pub effects: bool,
    /// Свой акцентный цвет `#rrggbb` поверх цвета темы; `null` — цвет темы.
    #[schema(example = "#b3001b")]
    pub accent_color: Option<String>,
    /// Шапка страницы особой: `styled` — в теме и с обоями, `plain` — нейтральная, `none` — без
    /// шапки (заголовок над списком).
    #[schema(example = "styled")]
    pub header_mode: String,
    /// Комментариев в обсуждении (без удалённых).
    pub comments_count: i64,
    /// Обсуждение закрыто: читать можно, писать — только admin.
    pub comments_locked: bool,
    /// Сколько произведений.
    #[schema(example = 7)]
    pub items_count: i64,
    /// Первые 4 произведения по порядку: обложки в карточке коллекции.
    pub preview: Vec<EntityRef>,
    /// Есть ли в коллекции сущность из `?contains=`; без параметра поля нет.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contains: Option<bool>,
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
    pub featured_position: Option<i32>,
    pub slug: Option<String>,
    pub wallpaper_url: Option<String>,
    pub cover_url: Option<String>,
    pub theme: Option<String>,
    pub wallpaper_mode: String,
    pub wallpaper_dim: Option<i16>,
    pub wallpaper_blur: i16,
    pub effects: bool,
    pub accent_color: Option<String>,
    pub header_mode: String,
    pub comments_count: i64,
    pub comments_locked: bool,
    pub items_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Колонки [`CollectionRow`] для `FROM collections c`.
pub const COLLECTION_COLUMNS: &str =
    "c.id, c.user_id, c.title, c.description, c.is_public, c.featured_position, c.slug,
    c.wallpaper_url, c.cover_url, c.theme, c.wallpaper_mode, c.wallpaper_dim, c.wallpaper_blur,
    c.effects, c.accent_color, c.header_mode,
    (SELECT count(*) FROM collection_comments cc
     WHERE cc.collection_id = c.id AND cc.deleted_at IS NULL) AS comments_count,
    c.comments_locked,
    (SELECT count(*) FROM collection_items i WHERE i.collection_id = c.id) AS items_count,
    c.created_at, c.updated_at";

/// Пометить коллекцию особой.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct FeatureCollection {
    /// Порядок среди особых; без него — в конец (при повторном PUT — не меняется).
    #[schema(example = 0)]
    pub position: Option<i32>,
    /// Адрес `/collections/{slug}`: a-z, 0-9, дефисы между ними, 3–100, не UUID и не служебное
    /// слово. Без него — при первой пометке транслит названия (занят — `-2`, `-3`…), при
    /// повторном PUT — не меняется.
    #[schema(example = "russkiy-top-2021")]
    pub slug: Option<String>,
    /// Обои: https-URL до 2048 символов. Не передано — не меняется, `null` — убрать.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub wallpaper_url: Option<Option<String>>,
    /// Обложка карточки: https-URL до 2048 символов. Не передано — не меняется, `null` — убрать.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub cover_url: Option<Option<String>>,
    /// Тема: `^[a-z0-9-]{1,32}$`. Не передано — не меняется, `null` — убрать.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>, example = "halloween")]
    pub theme: Option<Option<String>>,
    /// `header` или `page`; `null` — `header`. Не передано — не меняется.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>, example = "page")]
    pub wallpaper_mode: Option<Option<serde_json::Value>>,
    /// Целое 0–100; `null` — по теме. Не передано — не меняется.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>, example = 60)]
    pub wallpaper_dim: Option<Option<serde_json::Value>>,
    /// Целое 0–20; `null` — 0. Не передано — не меняется.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i32>, example = 4)]
    pub wallpaper_blur: Option<Option<serde_json::Value>>,
    /// Эффекты темы; `null` — `true`. Не передано — не меняется.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<bool>, example = true)]
    pub effects: Option<Option<serde_json::Value>>,
    /// `#rrggbb` (регистр не важен, хранится в нижнем); `null` — цвет темы. Не передано — не
    /// меняется.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>, example = "#b3001b")]
    pub accent_color: Option<Option<serde_json::Value>>,
    /// `styled`, `plain` или `none`; `null` — `styled`. Не передано — не меняется.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>, example = "plain")]
    pub header_mode: Option<Option<serde_json::Value>>,
}

/// Админ меняет название и описание любой коллекции (`is_public` здесь не меняется — 400).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminUpdateCollection {
    /// 1–200 символов. Не передано — не меняется.
    pub title: Option<String>,
    /// До 2000 символов. Не передано — не меняется, `null` — очищается.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// Закрыть (`true`) или открыть обсуждение. Не передано — не меняется.
    pub comments_locked: Option<bool>,
}

/// Комментарий в обсуждении коллекции. Ответ — `parent_id` корневого комментария (один уровень).
#[derive(Debug, Serialize, ToSchema)]
pub struct CollectionComment {
    pub id: Uuid,
    pub collection_id: Uuid,
    /// Корневой комментарий, на который это ответ; `null` — сам корневой.
    pub parent_id: Option<Uuid>,
    pub author: UserRef,
    #[schema(example = "Добавьте «Бегущего по лезвию»!")]
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct CollectionCommentRow {
    pub id: Uuid,
    pub collection_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub author_id: Uuid,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const COLLECTION_COMMENT_COLUMNS: &str =
    "id, collection_id, parent_id, author_id, body, created_at, updated_at";

/// Написать в обсуждение коллекции.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCollectionComment {
    /// 1–2000 символов после обрезки пробелов.
    #[schema(example = "Добавьте «Бегущего по лезвию»!")]
    pub body: String,
    /// Ответ на комментарий этой коллекции; ответ на ответ привязывается к его корню.
    pub parent_id: Option<Uuid>,
}

/// Ответ `GET /collections/by-slug/{slug}`: коллекция или куда перенаправить (301 на фронтенде).
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum CollectionBySlug {
    /// Текущий slug особой коллекции.
    Collection(Box<CollectionDetail>),
    /// Бывший slug, коллекция всё ещё особая: её текущий адрес.
    RedirectSlug { redirect_slug: String },
    /// Бывший slug, коллекция больше не особая, но публичная: `/collections/{id}`.
    RedirectId { redirect_id: Uuid },
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserCollectionsQuery {
    /// slug сущности: у каждой коллекции появится `contains` — есть ли она там. Кнопка «В коллекцию»
    /// без запроса каждой коллекции.
    #[param(example = "dune-2021")]
    pub contains: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

/// Коллекция с содержимым по порядку.
#[derive(Debug, Serialize, ToSchema)]
pub struct CollectionDetail {
    #[serde(flatten)]
    pub collection: Collection,
    pub items: Vec<CollectionItem>,
    /// Связанные подборки (задаёт admin) по порядку, только публичные; `[]` — связей нет.
    pub related: Vec<Collection>,
}

/// Поиск коллекций в админке (редактор связей, «Подборки»).
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AdminCollectionsQuery {
    /// Подстрока названия, без учёта регистра, до 200 символов; пусто — все.
    pub q: Option<String>,
    /// Только публичные (`true`) или только приватные (`false`).
    pub public: Option<bool>,
    /// Только особые (`true`) или только обычные (`false`).
    pub featured: Option<bool>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

/// Заменить список связанных подборок.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetRelated {
    /// id публичных коллекций, до 12, без повторов и без самой коллекции; порядок — порядок показа.
    pub ids: Vec<Uuid>,
}

/// Добавить связанную подборку.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddRelated {
    /// Куда вставить (со сдвигом остальных); без него — в конец.
    #[schema(example = 0)]
    pub position: Option<i32>,
}

/// Произведение в коллекции.
#[derive(Debug, Serialize, ToSchema)]
pub struct CollectionItem {
    pub entity: CollectionEntity,
    /// Порядок: по возрастанию, при равенстве — по времени добавления.
    pub position: i32,
    /// Комментарий владельца.
    pub note: Option<String>,
    pub added_at: DateTime<Utc>,
}

/// Сущность в коллекции: ссылка на карточку и краткое описание.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CollectionEntity {
    #[serde(flatten)]
    pub entity: EntityRef,
    /// Краткое описание: без HTML, до 1000 символов; `null` — описания нет.
    pub overview: Option<String>,
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
    /// Закрыть (`true`) или открыть обсуждение.
    pub comments_locked: Option<bool>,
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

/// Человек в интересах пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonInterest {
    pub person: PersonRef,
    /// Когда добавлен.
    pub since: DateTime<Utc>,
}

/// Тег или жанр в интересах пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct TagInterest {
    pub tag: TagRef,
    /// Когда добавлен.
    pub since: DateTime<Utc>,
}

/// Строка интереса к человеку или тегу: id и когда добавлен.
#[derive(Debug, sqlx::FromRow)]
pub struct TargetRow {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------- отметки

/// Личная отметка: `later` — «посмотреть (прочитать, пройти) позже», `done` — «просмотрено».
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum MarkKind {
    Later,
    Done,
}

/// Своя отметка на карточке.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub kind: MarkKind,
}

/// Сущность в своём списке отметок.
#[derive(Debug, Serialize, ToSchema)]
pub struct MarkedEntity {
    pub entity: EntityRef,
    pub kind: MarkKind,
    /// Когда поставлена эта отметка (смена `later` → `done` — заново).
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct MarkRow {
    pub entity_id: Uuid,
    pub kind: MarkKind,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MarksQuery {
    /// Только отметки этого вида; не передан — все.
    #[param(inline)]
    pub kind: Option<MarkKind>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------- лента

/// Тип записи ленты.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeedItemType {
    Review,
    Thread,
    Collection,
}

/// Запись ленты: заполнено ровно одно из `review`, `thread`, `collection` — по `type`.
#[derive(Debug, Serialize, ToSchema)]
pub struct FeedItem {
    #[serde(rename = "type")]
    pub item_type: FeedItemType,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<Collection>,
    /// Почему запись в ленте; у записи может быть несколько причин.
    pub reasons: Vec<FeedReason>,
}

/// Причина, по которой запись попала в ленту.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedReason {
    /// Автор — тот, на кого подписан пользователь.
    Follow { user: UserRef },
    /// Запись о сущности из интересов.
    Interest { entity: EntityRef },
    /// Популярная тема: личное закончилось или его нет.
    Popular,
}

/// Страница ленты. Следующая — `?cursor=<next_cursor>`; `null` — лента закончилась.
#[derive(Debug, Serialize, ToSchema)]
pub struct FeedPage {
    pub items: Vec<FeedItem>,
    pub next_cursor: Option<String>,
    #[schema(example = 20)]
    pub limit: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FeedQuery {
    /// Только записи этого типа.
    #[serde(rename = "type")]
    #[param(rename = "type", inline)]
    pub item_type: Option<FeedItemType>,
    /// `next_cursor` предыдущей страницы.
    pub cursor: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
}

/// Реакции на коллекцию: сколько каких и своя.
#[derive(Debug, Serialize, ToSchema)]
pub struct CollectionReactions {
    /// Всегда все четыре ключа.
    pub reactions: Reactions,
    /// Своя реакция; `null` — гость или не ставил.
    pub mine: Option<ReactionKind>,
}

// ---------------------------------------------------------------- комментарии к целям

/// Комментарий к произведению, человеку или студии.
#[derive(Debug, Serialize, ToSchema)]
pub struct TargetComment {
    pub id: Uuid,
    /// Цель в виде `kind:slug`, как в запросе: `entity:dune-2021`, `person:denis-villeneuve`,
    /// `studio:legendary-pictures`.
    #[schema(example = "entity:dune-2021")]
    pub target: String,
    /// Корневой комментарий; `null` — сам корневой. Ответ на ответ привязан к корню.
    pub parent_id: Option<Uuid>,
    pub author: UserRef,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct TargetCommentRow {
    pub id: Uuid,
    pub target_kind: String,
    pub target_id: Uuid,
    pub author_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Колонки `comments` для [`TargetCommentRow`].
pub const TARGET_COMMENT_COLUMNS: &str =
    "id, target_kind, target_id, author_id, parent_id, body, created_at, updated_at";

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTargetComment {
    /// Цель: `entity:dune-2021`, `person:denis-villeneuve`, `studio:legendary-pictures`.
    #[schema(example = "entity:dune-2021")]
    pub target: String,
    /// 1–2000 символов.
    pub body: String,
    /// Ответ на комментарий той же цели; ответ на ответ привяжется к корню.
    pub parent_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TargetCommentsQuery {
    /// Цель: `kind:slug`, где `kind` — `entity`, `person` или `studio`.
    #[param(example = "entity:dune-2021")]
    pub target: String,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------- мои комментарии

/// Комментарий пользователя в ленте «мои комментарии» вместе с тем, где он оставлен.
#[derive(Debug, Serialize, ToSchema)]
pub struct UserComment {
    pub id: Uuid,
    /// Где комментарий оставлен: вид цели в `kind`, рядом — её данные.
    pub target: UserCommentTarget,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Цель комментария. `kind` — `entity`, `person`, `studio`, `review` или `collection`;
/// остальные поля зависят от него.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum UserCommentTarget {
    Entity {
        entity: EntityRef,
    },
    Person {
        person: PersonRef,
    },
    Studio {
        studio: StudioRef,
    },
    /// Комментарий к рецензии: сама рецензия и произведение, о котором она.
    Review {
        review_id: Uuid,
        entity: EntityRef,
    },
    /// Сообщение в обсуждении коллекции.
    Collection {
        collection_id: Uuid,
        title: String,
    },
}

/// Строка объединённой ленты комментариев (`comments` + `review_comments` +
/// `collection_comments`). `source` — вид цели или таблица-источник.
#[derive(Debug, sqlx::FromRow)]
pub struct UserCommentRow {
    pub id: Uuid,
    pub source: String,
    pub target_id: Uuid,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------- аналитика профиля

/// Что пользователь успел на площадке. Числа считаются по его строкам: чужие ограничения
/// видимости (приватные коллекции) сюда не попадают.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProfileStats {
    /// Оценки: среднее, сколько их и распределение по 1–10.
    pub ratings: ProfileRatings,
    /// Рецензии с текстом (оценка без текста сюда не идёт).
    #[schema(example = 12)]
    pub reviews_with_text: i64,
    /// Отметки «посмотрю позже» и «просмотрено».
    pub marks: MarkCounts,
    /// Свои реакции на произведения: всегда все четыре ключа.
    pub reactions: Reactions,
    /// Комментарии: к рецензиям и в обсуждениях коллекций, без удалённых.
    pub comments: CommentCounts,
    /// Сколько оценённого и просмотренного каждого типа (`movie`, `series`, `book`, `game`):
    /// типы берутся у каталога через справочник, ключей без единой сущности нет.
    pub by_kind: ByKind,
}

/// Оценки пользователя. Без оценок — `average: null`, `count: 0`, нули в распределении.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProfileRatings {
    /// Округлено до 0.1. `null` — не оценивал.
    #[schema(example = 8.3)]
    pub average: Option<f64>,
    #[schema(example = 42)]
    pub count: i64,
    /// Сколько оценок 1, 2, … 10 — ровно 10 чисел.
    pub distribution: Vec<i64>,
}

/// Строка `GROUP BY kind` по `entity_marks`: вид отметки читаем как текст, чтобы незнакомый
/// (появится новый) не ломал запрос.
#[derive(Debug, sqlx::FromRow)]
pub struct MarkCountsRow {
    pub kind: String,
    pub count: i64,
}

#[derive(Debug, Default, Serialize, ToSchema)]
pub struct MarkCounts {
    /// «Посмотрю позже».
    #[schema(example = 7)]
    pub later: i64,
    /// «Просмотрено» (у книг «прочитано», у игр «пройдено»).
    #[schema(example = 30)]
    pub done: i64,
}

#[derive(Debug, Default, Serialize, ToSchema)]
pub struct CommentCounts {
    /// Комментарии к рецензиям.
    #[schema(example = 4)]
    pub reviews: i64,
    /// Сообщения в обсуждениях коллекций.
    #[schema(example = 2)]
    pub collections: i64,
}

/// Разрез по типам контента: `{"movie": 30, "book": 12}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ByKind {
    /// Что оценено.
    pub rated: std::collections::HashMap<String, i64>,
    /// Что отмечено «просмотрено».
    pub done: std::collections::HashMap<String, i64>,
}

/// Оценённое произведение в списке «что я оценил».
#[derive(Debug, Serialize, ToSchema)]
pub struct RatedEntity {
    pub entity: EntityRef,
    /// 1–10.
    #[schema(example = 8)]
    pub rating: i16,
    /// Есть ли у оценки текст рецензии.
    pub has_text: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct RatedRow {
    pub entity_id: Uuid,
    pub rating: i16,
    pub has_text: bool,
    pub created_at: DateTime<Utc>,
}

/// Порядок в списке оценок.
#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RatingsSort {
    /// Свежие сверху (по умолчанию).
    #[default]
    New,
    /// Сначала высокие оценки.
    RatingDesc,
    /// Сначала низкие.
    RatingAsc,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UserRatingsQuery {
    /// `new` (по умолчанию), `rating_desc`, `rating_asc`.
    #[param(inline)]
    pub sort: Option<RatingsSort>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

/// Отличает «поле не передано» (`None`) от `null` (`Some(None)`).
fn nullable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
