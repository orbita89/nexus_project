//! Справочник интересов для других модулей (`shared::directory::InterestDirectory`):
//! `realtime` по нему подписывает соединение на каналы сущностей, людей и тегов из интересов,
//! `catalog` — не переносит в архив сущности с пользовательскими данными и не удаляет людей,
//! нужных пользователям.

use shared::directory::{async_trait, InterestDirectory, PersonLikes, UserRatings};
use shared::AppResult;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
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

    async fn room_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>> {
        Ok(
            // Системные комнаты подписаны у всех: своих строк в chat_room_members у них нет.
            sqlx::query_scalar(
                "SELECT cm.room_id FROM chat_room_members cm JOIN chat_rooms r ON r.id = cm.room_id
                  WHERE cm.user_id = $1
                    AND r.award_id IS NULL AND r.award_edition_id IS NULL
                 UNION SELECT id FROM chat_rooms WHERE system",
            )
            .bind(user_id)
            .fetch_all(&self.db)
            .await?,
        )
    }

    async fn award_ids(&self, user_id: Uuid) -> AppResult<Vec<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT coalesce(r.award_id, r.award_edition_id) FROM chat_room_members cm
               JOIN chat_rooms r ON r.id = cm.room_id
              WHERE cm.user_id = $1 AND (r.award_id IS NOT NULL OR r.award_edition_id IS NOT NULL)",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
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

/// Лайки людей каталога для `catalog` (`shared::directory::PersonLikes`): таблица `person_likes`
/// наша, страница человека — его.
pub struct PgPersonLikes {
    db: PgPool,
}

impl PgPersonLikes {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl PersonLikes for PgPersonLikes {
    async fn counts(&self, person_ids: &[Uuid]) -> AppResult<HashMap<Uuid, i64>> {
        if person_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT person_id, count(*) FROM person_likes WHERE person_id = ANY($1)
             GROUP BY person_id",
        )
        .bind(person_ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().collect())
    }

    async fn mine(&self, user_id: Uuid, person_ids: &[Uuid]) -> AppResult<HashSet<Uuid>> {
        if person_ids.is_empty() {
            return Ok(HashSet::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT person_id FROM person_likes WHERE user_id = $1 AND person_id = ANY($2)",
        )
        .bind(user_id)
        .bind(person_ids)
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .collect())
    }
}

/// Свои оценки пользователя для `catalog` (`shared::directory::UserRatings`): таблица `reviews`
/// наша, а таблетке сезона в шапке сериала нужна и своя оценка — одним запросом на всю шапку.
pub struct PgUserRatings {
    db: PgPool,
}

impl PgUserRatings {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UserRatings for PgUserRatings {
    async fn mine(&self, user_id: Uuid, entity_ids: &[Uuid]) -> AppResult<HashMap<Uuid, i16>> {
        if entity_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Рецензия без оценки (только текст) своей оценкой не считается.
        let rows: Vec<(Uuid, i16)> = sqlx::query_as(
            "SELECT entity_id, rating FROM reviews
             WHERE user_id = $1 AND entity_id = ANY($2) AND rating IS NOT NULL",
        )
        .bind(user_id)
        .bind(entity_ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().collect())
    }
}
