//! Состояние приложения, общее для всех модулей.

use crate::auth::Jwt;
use crate::cache::{Cache, CacheSettings};
use crate::directory::{
    AwardDirectory, Chat, Directories, EntityDirectory, EntityRatings, InterestDirectory,
    PersonDirectory, PersonLikes, StudioDirectory, TagDirectory, UserDirectory, UserRatings,
};
use crate::events::EventBus;
use crate::isr::Isr;
use crate::mail::Mailer;
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
    /// Кэш в памяти процесса (L1 moka + блокировки); каталог берёт из него блокировки перестройки. В тестах выключен.
    pub cache: Cache,
    /// Ревалидация статики фронтенда. Выключена, если не заданы `ISR_URL` и `ISR_SECRET`.
    pub isr: Isr,
    /// Сущности каталога для других модулей (реализует `catalog`).
    pub entities: Arc<dyn EntityDirectory>,
    /// Пользователи для других модулей (реализует `auth`).
    pub users: Arc<dyn UserDirectory>,
    /// Интересы пользователей для других модулей (реализует `social`).
    pub interests: Arc<dyn InterestDirectory>,
    /// Сводки оценок для сортировок каталога (реализует `catalog`, пишет `social`).
    pub ratings: Arc<dyn EntityRatings>,
    /// Запись сообщений чата из WebSocket (реализует `social`, вызывает `realtime`).
    pub chat: Arc<dyn Chat>,
    /// Люди каталога для других модулей (реализует `catalog`).
    pub people: Arc<dyn PersonDirectory>,
    /// Теги каталога для других модулей (реализует `catalog`).
    pub tags: Arc<dyn TagDirectory>,
    /// Премии каталога и их церемонии для других модулей (реализует `catalog`).
    pub awards: Arc<dyn AwardDirectory>,
    /// Студии каталога для других модулей (реализует `catalog`).
    pub studios: Arc<dyn StudioDirectory>,
    /// Лайки людей каталога (реализует `social`, читает `catalog`).
    pub person_likes: Arc<dyn PersonLikes>,
    /// Свои оценки пользователя (реализует `social`, читает `catalog`: таблетки сезонов).
    pub user_ratings: Arc<dyn UserRatings>,
    /// Шина событий: модули публикуют, `realtime` рассылает клиентам.
    pub events: EventBus,
    /// Остановка сервера (SIGINT/SIGTERM): WebSocket-соединения закрываются, `main` дописывает
    /// очередь чата (`state.chat.flush()`).
    pub shutdown: tokio_util::sync::CancellationToken,
}

impl AppState {
    pub fn new(config: Config, db: PgPool, mailer: Mailer, directories: Directories) -> Self {
        let jwt = Jwt::new(config.jwt_secret.as_bytes());
        let cache = Cache::local(CacheSettings::from_config(&config));
        let isr = Isr::from_config(&config);
        Self {
            config: Arc::new(config),
            db,
            jwt: Arc::new(jwt),
            mailer,
            cache,
            isr,
            entities: directories.entities,
            users: directories.users,
            interests: directories.interests,
            ratings: directories.ratings,
            chat: directories.chat,
            people: directories.people,
            tags: directories.tags,
            awards: directories.awards,
            studios: directories.studios,
            person_likes: directories.person_likes,
            user_ratings: directories.user_ratings,
            events: EventBus::default(),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }
}
