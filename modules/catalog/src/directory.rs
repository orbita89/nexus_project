//! Справочники сущностей, людей и тегов для других модулей (`shared::directory::EntityDirectory`,
//! `PersonDirectory`, `TagDirectory`) и сводки оценок от `social` для сортировок
//! (`shared::directory::EntityRatings`).

use crate::models::VISIBLE;
use crate::people::PUBLIC_PERSON;
use shared::directory::{
    async_trait, EntityDirectory, EntityRatings, EntityRef, PersonDirectory, PersonRef,
    RatingStats, StudioDirectory, StudioRef, TagDirectory, TagRef,
};
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

    async fn kind_counts(&self, ids: &[Uuid]) -> AppResult<HashMap<String, i64>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            // kind — enum entity_kind: читаем как текст, иначе sqlx не разберёт колонку.
            "SELECT kind::text, count(*) FROM entities WHERE id = ANY($1) GROUP BY kind",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().collect())
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

    async fn overviews(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // С запасом на теги и пробелы: обрезка — в `overview`.
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, left(description, 3000) FROM entities
             WHERE id = ANY($1) AND description IS NOT NULL",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, text)| Some((id, overview(&text)?)))
            .collect())
    }
}

/// Длина краткого описания (`EntityDirectory::overviews`).
pub const MAX_OVERVIEW: usize = 1000;

/// Краткое описание: без HTML-тегов, пробелы схлопнуты, не длиннее [`MAX_OVERVIEW`] символов —
/// по концу предложения, если он не раньше середины, иначе по слову, с «…». Пустое — `None`.
pub fn overview(text: &str) -> Option<String> {
    let mut plain = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                plain.push(' ');
            }
            _ if !in_tag => plain.push(c),
            _ => {}
        }
    }
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if plain.is_empty() {
        return None;
    }
    if plain.chars().count() <= MAX_OVERVIEW {
        return Some(plain);
    }
    let cut: String = plain.chars().take(MAX_OVERVIEW - 1).collect();
    let half = cut.len() / 2;
    let sentence = cut
        .rmatch_indices(['.', '!', '?'])
        .map(|(i, _)| i)
        .find(|&i| i >= half);
    Some(match sentence {
        Some(i) => cut[..=i].to_string(),
        None => {
            let word = cut.rfind(' ').unwrap_or(cut.len());
            format!(
                "{}…",
                cut[..word].trim_end_matches([',', ';', ':', '-', ' '])
            )
        }
    })
}

pub struct PgPersonDirectory {
    db: PgPool,
}

impl PgPersonDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

/// По slug — только публичные (`PUBLIC_PERSON`: скрытого на сайте нет, другие модули отвечают
/// 404), по id — все.
#[async_trait]
impl PersonDirectory for PgPersonDirectory {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<PersonRef>> {
        Ok(sqlx::query_as(&format!(
            "SELECT id, slug, full_name, photo_url FROM people p
             WHERE p.slug = $1 AND {PUBLIC_PERSON}"
        ))
        .bind(slug)
        .fetch_optional(&self.db)
        .await?)
    }

    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, PersonRef>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<PersonRef> =
            sqlx::query_as("SELECT id, slug, full_name, photo_url FROM people WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&self.db)
                .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }
}

pub struct PgTagDirectory {
    db: PgPool,
}

impl PgTagDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl TagDirectory for PgTagDirectory {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<TagRef>> {
        Ok(
            sqlx::query_as("SELECT id, slug, name, kind FROM tags WHERE slug = $1")
                .bind(slug)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, TagRef>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<TagRef> =
            sqlx::query_as("SELECT id, slug, name, kind FROM tags WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&self.db)
                .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
    }
}

/// Студии: страница студии открыта всем, скрытых студий нет.
pub struct PgStudioDirectory {
    db: PgPool,
}

impl PgStudioDirectory {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl StudioDirectory for PgStudioDirectory {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<StudioRef>> {
        Ok(
            sqlx::query_as("SELECT id, slug, name, logo_url FROM studios WHERE slug = $1")
                .bind(slug)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, StudioRef>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<StudioRef> =
            sqlx::query_as("SELECT id, slug, name, logo_url FROM studios WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&self.db)
                .await?;
        Ok(rows.into_iter().map(|row| (row.id, row)).collect())
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

#[cfg(test)]
mod overview_tests {
    use super::*;

    #[test]
    fn plain_and_short() {
        assert_eq!(
            overview("  <p>Пол  Атрейдес</p><br>едет  на Арракис. "),
            Some("Пол Атрейдес едет на Арракис.".into())
        );
        assert_eq!(overview(" <br> "), None);
    }

    #[test]
    fn long_is_cut_by_sentence_or_word() {
        let sentences = "Первое предложение довольно длинное. ".repeat(40);
        let got = overview(&sentences).unwrap();
        assert!(
            got.chars().count() <= MAX_OVERVIEW && got.ends_with('.'),
            "{got}"
        );
        let words = "слово ".repeat(400);
        let got = overview(&words).unwrap();
        assert!(
            got.chars().count() <= MAX_OVERVIEW && got.ends_with("слово…"),
            "{got}"
        );
    }
}
