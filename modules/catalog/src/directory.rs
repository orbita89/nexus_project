//! Справочник сущностей для других модулей (`shared::directory::EntityDirectory`).

use shared::directory::{async_trait, EntityDirectory, EntityRef};
use shared::AppResult;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

const REF_COLUMNS: &str = "id, kind::text AS kind, slug, title, cover_url";

pub struct PgEntityDirectory {
    db: PgPool,
}

impl PgEntityDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl EntityDirectory for PgEntityDirectory {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<EntityRef>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {REF_COLUMNS} FROM entities WHERE slug = $1"
        ))
        .bind(slug)
        .fetch_optional(&self.db)
        .await?)
    }

    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, EntityRef>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<EntityRef> = sqlx::query_as(&format!(
            "SELECT {REF_COLUMNS} FROM entities WHERE id = ANY($1)"
        ))
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }
}
