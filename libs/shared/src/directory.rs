//! Справочники: явный интерфейс, через который модуль читает чужие данные.
//!
//! Модуль не ходит SQL'ем в чужие таблицы (см. «Правила границ» в `documents/architecture.md`).
//! Если `social` нужно название сущности или имя автора, он вызывает трейт отсюда, а реализует
//! трейт модуль-владелец данных: [`EntityDirectory`], [`PersonDirectory`], [`TagDirectory`] и
//! [`EntityRatings`] — `catalog`,
//! [`UserDirectory`] — `auth`, [`InterestDirectory`] — `social` (интересы и «сущность нужна
//! пользователям»).
//! Реализации собирает `app` и кладёт в [`AppState`](crate::AppState); в тестах их можно
//! подменить: `state.entities = Arc::new(Fake)`.
//!
//! Методы пакетные (`by_ids`): список из 20 рецензий — один запрос за авторами, а не 20.

use crate::{AppResult, AppState};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

pub use async_trait::async_trait;

/// Сущность каталога в чужих ответах: ровно столько, сколько нужно для ссылки на карточку.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct EntityRef {
    pub id: Uuid,
    /// `movie`, `series`, `book`, `game`.
    #[schema(example = "movie")]
    pub kind: String,
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    pub cover_url: Option<String>,
    /// Дата выхода: по году фронтенд различает «Дюна (1984)» и «Дюна (2021)». `null` — неизвестна.
    #[schema(example = "2021-10-22")]
    pub release_date: Option<chrono::NaiveDate>,
}

/// Человек каталога (автор, режиссёр, актёр) в чужих ответах: ссылка на его страницу.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct PersonRef {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub photo_url: Option<String>,
}

/// Тег или жанр каталога в чужих ответах.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct TagRef {
    pub id: Uuid,
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
    /// `genre` или `tag`.
    #[schema(example = "genre")]
    pub kind: String,
}

/// Пользователь в чужих ответах (автор рецензии, владелец коллекции, подписчик).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ToSchema)]
pub struct UserRef {
    pub id: Uuid,
    #[schema(example = "user")]
    pub username: String,
    #[schema(example = "Обычный пользователь")]
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Сущности каталога. Реализует `catalog`.
#[async_trait]
pub trait EntityDirectory: Send + Sync {
    /// Сущность по slug, `None` — нет такой.
    async fn by_slug(&self, slug: &str) -> AppResult<Option<EntityRef>>;
    /// Сущности по id. Несуществующих id в ответе нет.
    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, EntityRef>>;
    /// Сущности по slug'ам (ключ — slug). Несуществующих в ответе нет.
    async fn by_slugs(&self, slugs: &[String]) -> AppResult<HashMap<String, EntityRef>>;
}

/// Пользователи. Реализует `auth`.
#[async_trait]
pub trait UserDirectory: Send + Sync {
    /// Активный пользователь по username (без учёта регистра). Заблокированный — `None`.
    async fn by_username(&self, username: &str) -> AppResult<Option<UserRef>>;
    /// Пользователи по id, включая заблокированных: их рецензии и коллекции остаются видны.
    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, UserRef>>;
}

/// Люди каталога. Реализует `catalog`. По slug — только публичные (есть работы в видимых
/// сущностях или работ нет вовсе), по id — все.
#[async_trait]
pub trait PersonDirectory: Send + Sync {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<PersonRef>>;
    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, PersonRef>>;
}

/// Теги и жанры каталога. Реализует `catalog`.
#[async_trait]
pub trait TagDirectory: Send + Sync {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<TagRef>>;
    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, TagRef>>;
}

/// Интересы пользователей. Реализует `social`, пользуется `realtime` (автоподписка).
#[async_trait]
pub trait InterestDirectory: Send + Sync {
    /// Сущности, которые пользователь добавил в интересы.
    async fn entity_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>>;
    /// Люди в интересах пользователя.
    async fn person_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>>;
    /// Теги в интересах пользователя.
    async fn tag_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>>;
    /// Какие из `person_ids` нужны пользователям (в интересах, есть чат). `catalog` не удаляет
    /// таких, даже когда у человека не осталось работ (перенос в архив).
    async fn people_in_use(&self, person_ids: &[Uuid]) -> AppResult<HashSet<Uuid>>;
    /// Какие из `entity_ids` нужны пользователям: в интересах, рецензиях, коллекциях, отметках,
    /// реакциях или темах форума. `catalog` не переносит такие сущности в холодный архив —
    /// удаление строки стёрло бы каскадом пользовательские данные.
    async fn entities_in_use(&self, entity_ids: &[Uuid]) -> AppResult<HashSet<Uuid>>;
}

/// Сводка оценок сущности: средняя (без округления) и сколько оценок.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatingStats {
    /// `None` — оценок нет.
    pub average: Option<f64>,
    pub count: i64,
}

/// Оценки сущностей для каталога: `social` (владелец рецензий) сообщает сводки, `catalog` хранит
/// их у себя для сортировок списка (`popular`, `rating`). Реализует `catalog`.
///
/// Значения абсолютные, не приращения: повтор и пропущенный вызов исправляет следующий.
#[async_trait]
pub trait EntityRatings: Send + Sync {
    /// Заменить сводки этих сущностей. Удалённые сущности пропускаются.
    async fn set(&self, stats: &[(Uuid, RatingStats)]) -> AppResult<()>;
    /// Заменить все сводки: у сущностей, которых нет в `stats`, оценок нет.
    async fn replace_all(&self, stats: &[(Uuid, RatingStats)]) -> AppResult<()>;
}

/// Комната чата: сущность, человек или тег каталога (по id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatRoom {
    Entity(Uuid),
    Person(Uuid),
    Tag(Uuid),
}

/// Чат для `realtime`: сообщения, пришедшие по WebSocket (`chat.send`, `chat.read`), пишет
/// владелец таблиц чата. Реализует `social`. Событие (`chat.message.created`, `chat.read`)
/// реализация публикует сама — после `commit`, через `state.events`.
#[async_trait]
pub trait Chat: Send + Sync {
    /// Отправить сообщение от `user_id` (ответ на `reply_to` той же комнаты). Возвращает
    /// сообщение в виде для клиента. Ошибки: 400 — текст или `reply_to`, 404 — нет сущности,
    /// 429 — лимит частоты.
    async fn send(
        &self,
        state: &AppState,
        user_id: Uuid,
        room: ChatRoom,
        body: &str,
        reply_to: Option<Uuid>,
    ) -> AppResult<Value>;
    /// Прочитано до `message_id` включительно (назад не двигается). 404 — нет такого сообщения
    /// в комнате.
    async fn read(
        &self,
        state: &AppState,
        user_id: Uuid,
        room: ChatRoom,
        message_id: Uuid,
    ) -> AppResult<()>;
}

/// Все справочники разом: аргумент [`AppState::new`](crate::AppState::new).
#[derive(Clone)]
pub struct Directories {
    pub entities: Arc<dyn EntityDirectory>,
    pub users: Arc<dyn UserDirectory>,
    pub interests: Arc<dyn InterestDirectory>,
    pub ratings: Arc<dyn EntityRatings>,
    pub chat: Arc<dyn Chat>,
    pub people: Arc<dyn PersonDirectory>,
    pub tags: Arc<dyn TagDirectory>,
}
