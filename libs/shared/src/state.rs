//! Состояние приложения, общее для всех модулей.

use crate::auth::Jwt;
use crate::Config;
use sqlx::PgPool;
use std::sync::Arc;

/// Клонируется на каждый запрос, поэтому внутри только дешёвые для клонирования хэндлы.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub jwt: Arc<Jwt>,
}

impl AppState {
    pub fn new(config: Config, db: PgPool) -> Self {
        let jwt = Jwt::new(config.jwt_secret.as_bytes());
        Self {
            config: Arc::new(config),
            db,
            jwt: Arc::new(jwt),
        }
    }
}
