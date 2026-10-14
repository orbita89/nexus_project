//! Справочник интересов для других модулей (`shared::directory::InterestDirectory`):
//! `realtime` по нему подписывает соединение на каналы сущностей из интересов, `catalog` — не
//! переносит в архив сущности с пользовательскими данными.

use shared::directory::{async_trait, InterestDirectory};
use shared::AppResult;
use sqlx::PgPool;
use std::collections::HashSet;
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

    async fn entities_in_use(&self, entity_ids: &[Uuid]) -> AppResult<HashSet<Uuid>> {
        let used: Vec<Uuid> = sqlx::query_scalar(
            "SELECT entity_id FROM user_interests WHERE entity_id = ANY($1)
             UNION SELECT entity_id FROM reviews WHERE entity_id = ANY($1)
             UNION SELECT entity_id FROM collection_items WHERE entity_id = ANY($1)
             UNION SELECT entity_id FROM entity_marks WHERE entity_id = ANY($1)
             UNION SELECT entity_id FROM entity_reactions WHERE entity_id = ANY($1)
             UNION SELECT entity_id FROM forum_thread_entities WHERE entity_id = ANY($1)",
        )
        .bind(entity_ids)
        .fetch_all(&self.db)
        .await?;
        Ok(used.into_iter().collect())
    }
}
