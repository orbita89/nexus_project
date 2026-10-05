//! Состояние приложения, общее для всех модулей.

use crate::Config;
use sqlx::PgPool;
use std::sync::Arc;

/// Клонируется на каждый запрос, поэтому внутри только дешёвые для клонирования хэндлы.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
}

impl AppState {
    pub fn new(config: Config, db: PgPool) -> Self {
        Self {
            config: Arc::new(config),
            db,
        }
    }
}
