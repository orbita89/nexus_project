//! Справочники: явный интерфейс, через который модуль читает чужие данные.
//!
//! Модуль не ходит SQL'ем в чужие таблицы (см. «Правила границ» в `documents/architecture.md`).
//! Если `social` нужно название сущности или имя автора, он вызывает трейт отсюда, а реализует
//! трейт модуль-владелец данных: [`EntityDirectory`] — `catalog`, [`UserDirectory`] — `auth`.
//! Реализации собирает `app` и кладёт в [`AppState`](crate::AppState); в тестах их можно
//! подменить: `state.entities = Arc::new(Fake)`.
//!
//! Методы пакетные (`by_ids`): список из 20 рецензий — один запрос за авторами, а не 20.

use crate::AppResult;
use serde::Serialize;
use std::collections::HashMap;
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
}

/// Пользователи. Реализует `auth`.
#[async_trait]
pub trait UserDirectory: Send + Sync {
    /// Активный пользователь по username (без учёта регистра). Заблокированный — `None`.
    async fn by_username(&self, username: &str) -> AppResult<Option<UserRef>>;
    /// Пользователи по id, включая заблокированных: их рецензии и коллекции остаются видны.
    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, UserRef>>;
}

/// Все справочники разом: аргумент [`AppState::new`](crate::AppState::new).
#[derive(Clone)]
pub struct Directories {
    pub entities: Arc<dyn EntityDirectory>,
    pub users: Arc<dyn UserDirectory>,
}
