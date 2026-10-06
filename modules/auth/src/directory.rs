//! Справочник пользователей для других модулей (`shared::directory::UserDirectory`).

use shared::directory::{async_trait, UserDirectory, UserRef};
use shared::AppResult;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

const REF_COLUMNS: &str = "id, username::text AS username, display_name, avatar_url";

pub struct PgUserDirectory {
    db: PgPool,
}

impl PgUserDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl UserDirectory for PgUserDirectory {
    async fn by_username(&self, username: &str) -> AppResult<Option<UserRef>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {REF_COLUMNS} FROM users WHERE username = $1::citext AND is_active"
        ))
        .bind(username)
        .fetch_optional(&self.db)
        .await?)
    }

    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, UserRef>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<UserRef> = sqlx::query_as(&format!(
            "SELECT {REF_COLUMNS} FROM users WHERE id = ANY($1)"
        ))
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }
}
