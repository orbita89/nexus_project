//! Справочник интересов для других модулей (`shared::directory::InterestDirectory`):
//! `realtime` по нему подписывает соединение на каналы сущностей из интересов.

use shared::directory::{async_trait, InterestDirectory};
use shared::AppResult;
use sqlx::PgPool;
use uuid::Uuid;

pub struct PgInterestDirectory {
    db: PgPool,
}

impl PgInterestDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl InterestDirectory for PgInterestDirectory {
    async fn entity_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT entity_id FROM user_interests WHERE user_id = $1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }
}
