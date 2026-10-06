//! Состояние приложения, общее для всех модулей.

use crate::auth::Jwt;
use crate::directory::{Directories, EntityDirectory, InterestDirectory, UserDirectory};
use crate::events::EventBus;
use crate::mail::Mailer;
use crate::search::Search;
use crate::Config;
use sqlx::PgPool;
use std::sync::Arc;

/// Клонируется на каждый запрос, поэтому внутри только дешёвые для клонирования хэндлы.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub jwt: Arc<Jwt>,
    pub mailer: Mailer,
    /// Meilisearch. В тестах по умолчанию выключен (`Search::disabled`).
    pub search: Search,
    /// Сущности каталога для других модулей (реализует `catalog`).
    pub entities: Arc<dyn EntityDirectory>,
    /// Пользователи для других модулей (реализует `auth`).
    pub users: Arc<dyn UserDirectory>,
    /// Интересы пользователей для других модулей (реализует `social`).
    pub interests: Arc<dyn InterestDirectory>,
    /// Шина событий: модули публикуют, `realtime` рассылает клиентам.
    pub events: EventBus,
}

impl AppState {
    pub fn new(config: Config, db: PgPool, mailer: Mailer, directories: Directories) -> Self {
        let jwt = Jwt::new(config.jwt_secret.as_bytes());
        let search = Search::new(&config.meili_url, config.meili_master_key.clone(), "");
        Self {
            config: Arc::new(config),
            db,
            jwt: Arc::new(jwt),
            mailer,
            search,
            entities: directories.entities,
            users: directories.users,
            interests: directories.interests,
            events: EventBus::default(),
        }
    }
}
