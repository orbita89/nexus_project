//! Справочник сущностей для других модулей (`shared::directory::EntityDirectory`) и сводки оценок
//! от `social` для сортировок (`shared::directory::EntityRatings`).

use crate::models::VISIBLE;
use shared::directory::{async_trait, EntityDirectory, EntityRatings, EntityRef, RatingStats};
use shared::AppResult;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

const REF_COLUMNS: &str = "id, kind::text AS kind, slug, title, cover_url, release_date";

pub struct PgEntityDirectory {
    db: PgPool,
}

impl PgEntityDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

/// По slug (новые рецензии, оценки, коллекции, темы: адрес пришёл с сайта) — только видимые
/// (`VISIBLE`): невидимой на сайте нет, другие модули отвечают 404. По id (то, что уже сохранено)
/// — все: коллекции и ленты не теряют элементы, если сущность скрыли.
#[async_trait]
impl EntityDirectory for PgEntityDirectory {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<EntityRef>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {REF_COLUMNS} FROM entities e WHERE e.slug = $1 AND {VISIBLE}"
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

    async fn by_slugs(&self, slugs: &[String]) -> AppResult<HashMap<String, EntityRef>> {
        if slugs.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<EntityRef> = sqlx::query_as(&format!(
            "SELECT {REF_COLUMNS} FROM entities e WHERE e.slug = ANY($1) AND {VISIBLE}"
        ))
        .bind(slugs)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.slug.clone(), row))
            .collect())
    }
}

/// Сводки оценок в `entity_ratings` (таблица каталога).
pub struct PgEntityRatings {
    db: PgPool,
}

impl PgEntityRatings {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

/// Upsert сводок; сущностей, которых уже нет, пропускает.
async fn upsert(
    db: impl sqlx::PgExecutor<'_>,
    stats: &[(Uuid, RatingStats)],
) -> Result<(), sqlx::Error> {
    let ids: Vec<Uuid> = stats.iter().map(|(id, _)| *id).collect();
    let averages: Vec<Option<f64>> = stats.iter().map(|(_, s)| s.average).collect();
    let counts: Vec<i64> = stats.iter().map(|(_, s)| s.count).collect();
    sqlx::query(
        "INSERT INTO entity_ratings (entity_id, average, count)
         SELECT v.id, v.average, v.count
         FROM unnest($1::uuid[], $2::float8[], $3::int8[]) AS v(id, average, count)
         JOIN entities e ON e.id = v.id
         ON CONFLICT (entity_id) DO UPDATE
         SET average = EXCLUDED.average, count = EXCLUDED.count, updated_at = now()",
    )
    .bind(ids)
    .bind(averages)
    .bind(counts)
    .execute(db)
    .await?;
    Ok(())
}

#[async_trait]
impl EntityRatings for PgEntityRatings {
    async fn set(&self, stats: &[(Uuid, RatingStats)]) -> AppResult<()> {
        Ok(upsert(&self.db, stats).await?)
    }

    async fn replace_all(&self, stats: &[(Uuid, RatingStats)]) -> AppResult<()> {
        let mut tx = self.db.begin().await?;
        let ids: Vec<Uuid> = stats.iter().map(|(id, _)| *id).collect();
        // Обнулить, а не удалить: новое `updated_at` переносит обнуление в индекс поиска
        // (`search::sync_ratings`). Для сортировок нулевая сводка — то же, что её отсутствие.
        sqlx::query(
            "UPDATE entity_ratings SET average = NULL, count = 0, updated_at = now()
             WHERE NOT (entity_id = ANY($1)) AND (count > 0 OR average IS NOT NULL)",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        upsert(&mut *tx, stats).await?;
        tx.commit().await?;
        Ok(())
    }
}
