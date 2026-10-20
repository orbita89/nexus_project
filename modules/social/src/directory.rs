//! Справочник интересов для других модулей (`shared::directory::InterestDirectory`):
//! `realtime` по нему подписывает соединение на каналы сущностей, людей и тегов из интересов,
//! `catalog` — не переносит в архив сущности с пользовательскими данными и не удаляет людей,
//! нужных пользователям.

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

    async fn person_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT person_id FROM user_person_interests WHERE user_id = $1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    async fn tag_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT tag_id FROM user_tag_interests WHERE user_id = $1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    async fn people_in_use(&self, person_ids: &[Uuid]) -> AppResult<HashSet<Uuid>> {
        if person_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let used: Vec<Uuid> = sqlx::query_scalar(
            "SELECT person_id FROM user_person_interests WHERE person_id = ANY($1)
             UNION SELECT person_id FROM chat_rooms WHERE person_id = ANY($1)",
        )
        .bind(person_ids)
        .fetch_all(&self.db)
        .await?;
        Ok(used.into_iter().collect())
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
