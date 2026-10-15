//! Студии: страница студии (краткая информация) и ссылки на неё из карточек фильмов.
//!
//! Какие у фильма студии — `entities.metadata.studios` (названия, как их отдаёт TMDB; правит
//! админка). Фильмы студии — `GET /entities?studio=<slug>`: `metadata @> {"studios": [name]}` по
//! GIN-индексу `metadata`. Таблица `studios` — сами студии: slug для URL и данные компании TMDB.
//!
//! Строки создаёт [`sync`] — для новых названий из `metadata.studios`: TMDB id, логотип и страна —
//! из сырья `entity_tmdb_data` (`production_companies`), slug — из названия. Полная — в начале
//! перестройки поиска (первый раз ~1 мин на 53 тыс. студий, дальше — только новые названия),
//! точечная — при публикации правок и возврате фильма из архива. Описание, штаб-квартира, сайт и
//! материнская компания — `GET /3/company/{id}` при первом открытии страницы студии.

use crate::archive::enrich::{self, slugify, TmdbError};
use crate::models::VISIBLE;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppError, AppResult, AppState};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

/// Логотипы TMDB (PNG с прозрачностью).
const LOGO: &str = "https://image.tmdb.org/t/p/w300";
const MAX_TEXT: usize = 20_000;
/// Вставка новых студий пачками.
const BATCH: usize = 5_000;

/// Студия в карточке фильма: ссылка на её страницу.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct StudioRef {
    #[schema(example = "warner-bros-pictures")]
    pub slug: String,
    #[schema(example = "Warner Bros. Pictures")]
    pub name: String,
    pub logo_url: Option<String>,
}

/// Страница студии. Фильмы — `GET /entities?studio=<slug>`.
#[derive(Debug, Serialize, ToSchema)]
pub struct StudioDetail {
    #[schema(example = "warner-bros-pictures")]
    pub slug: String,
    #[schema(example = "Warner Bros. Pictures")]
    pub name: String,
    pub logo_url: Option<String>,
    /// Страна (ISO 3166-1 alpha-2).
    #[schema(example = "US")]
    pub origin_country: Option<String>,
    #[schema(example = "Burbank, California, United States")]
    pub headquarters: Option<String>,
    #[schema(example = "https://www.warnerbros.com")]
    pub homepage: Option<String>,
    /// Описание из TMDB (обычно по-английски, у многих студий его нет).
    pub description: Option<String>,
    /// Материнская компания, если она тоже есть в каталоге.
    pub parent: Option<StudioRef>,
    /// Видимых вышедших сущностей студии (как в `GET /entities?studio=`).
    #[schema(example = 1343)]
    pub entities_count: i64,
}

#[derive(sqlx::FromRow)]
struct StudioRow {
    id: Uuid,
    slug: String,
    name: String,
    tmdb_id: Option<i32>,
    logo_url: Option<String>,
    origin_country: Option<String>,
    headquarters: Option<String>,
    homepage: Option<String>,
    description: Option<String>,
    parent_tmdb_id: Option<i32>,
    details_fetched: bool,
}

/// Фильтр «фильм этой студии» для `metadata`: `{"studios": [name]}`.
pub(crate) fn metadata_filter(name: &str) -> Value {
    json!({ "studios": [name] })
}

/// Создаёт студии для названий из `metadata.studios`, которых ещё нет: всех сущностей (`None`) или
/// только `entities`. Возвращает число созданных.
pub async fn sync(db: &PgPool, entities: Option<&[Uuid]>) -> sqlx::Result<u64> {
    let scope = if entities.is_some() {
        "AND e.id = ANY($1)"
    } else {
        "AND $1::uuid[] IS NULL"
    };
    let names: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT s FROM entities e, jsonb_array_elements_text(e.metadata -> 'studios') s
         WHERE jsonb_typeof(e.metadata -> 'studios') = 'array' {scope}
           AND NOT EXISTS (SELECT 1 FROM studios st WHERE st.name = s)"
    ))
    .bind(entities)
    .fetch_all(db)
    .await?;
    let names: Vec<String> = names.into_iter().filter(|n| !n.trim().is_empty()).collect();
    if names.is_empty() {
        return Ok(0);
    }

    // Компании TMDB с такими названиями из сырья фильмов, где они указаны.
    let companies: Vec<Company> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (c ->> 'name') c ->> 'name' AS name, (c ->> 'id')::int AS tmdb_id,
                nullif(c ->> 'logo_path', '') AS logo_path,
                nullif(c ->> 'origin_country', '') AS origin_country
         FROM entities e
         JOIN entity_tmdb_data d ON d.entity_id = e.id,
              jsonb_array_elements(d.data -> 'production_companies') c
         WHERE e.metadata -> 'studios' ?| $2 AND c ->> 'name' = ANY($2) {scope}
         ORDER BY c ->> 'name', (c ->> 'id')::int"
    ))
    .bind(entities)
    .bind(&names)
    .fetch_all(db)
    .await?;
    let mut companies: HashMap<String, Company> =
        companies.into_iter().map(|c| (c.name.clone(), c)).collect();
    // TMDB id уже у другой студии (компанию переименовали) — без id.
    let ids: Vec<i32> = companies.values().filter_map(|c| c.tmdb_id).collect();
    let taken_ids: HashSet<i32> =
        sqlx::query_scalar("SELECT tmdb_id FROM studios WHERE tmdb_id = ANY($1)")
            .bind(&ids)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();

    let rows: Vec<NewStudio> = names
        .into_iter()
        .map(|name| {
            let company = companies.remove(&name);
            let tmdb_id = company
                .as_ref()
                .and_then(|c| c.tmdb_id)
                .filter(|id| !taken_ids.contains(id));
            let slug = slugify(&name);
            let base = match (slug.is_empty(), tmdb_id) {
                (false, _) => slug,
                (true, Some(id)) => format!("studio-{id}"),
                (true, None) => "studio".to_string(),
            };
            let (logo_path, origin_country) = company
                .map(|c| (c.logo_path, c.origin_country))
                .unwrap_or_default();
            NewStudio {
                name,
                tmdb_id,
                logo_url: logo_path.map(|p| format!("{LOGO}{p}")),
                origin_country,
                base,
            }
        })
        .collect();
    let bases: Vec<&str> = unique_strs(rows.iter().map(|r| r.base.as_str()));
    let mut taken: HashSet<String> = sqlx::query_scalar(
        "SELECT slug FROM studios WHERE slug = ANY($1) OR slug LIKE ANY(
             SELECT b || '-%' FROM unnest($1::text[]) b)",
    )
    .bind(&bases)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    let mut seen_ids = HashSet::new();
    let mut inserted = 0;
    for chunk in rows.chunks(BATCH) {
        let (mut names, mut slugs, mut ids, mut logos, mut countries) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for row in chunk {
            // Два названия одной компании TMDB — id у первого.
            let id = row.tmdb_id.filter(|id| seen_ids.insert(*id));
            slugs.push(free_slug(&row.base, id, &mut taken));
            names.push(row.name.clone());
            ids.push(id);
            logos.push(row.logo_url.clone());
            countries.push(row.origin_country.clone());
        }
        inserted += sqlx::query(
            "INSERT INTO studios (name, slug, tmdb_id, logo_url, origin_country)
             SELECT * FROM unnest($1::text[], $2::text[], $3::int[], $4::text[], $5::text[])
             ON CONFLICT DO NOTHING",
        )
        .bind(&names)
        .bind(&slugs)
        .bind(&ids)
        .bind(&logos)
        .bind(&countries)
        .execute(db)
        .await?
        .rows_affected();
    }
    Ok(inserted)
}

/// Компания TMDB из `production_companies` сырья фильма.
#[derive(sqlx::FromRow)]
struct Company {
    name: String,
    tmdb_id: Option<i32>,
    logo_path: Option<String>,
    origin_country: Option<String>,
}

/// Новая студия; `base` — slug из названия до проверки занятости.
struct NewStudio {
    name: String,
    tmdb_id: Option<i32>,
    logo_url: Option<String>,
    origin_country: Option<String>,
    base: String,
}

fn unique_strs<'a>(values: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    values.filter(|v| seen.insert(*v)).collect()
}

/// Свободный slug: `base`, занят — `base-<tmdb_id>`, затем `base-2`, `base-3`, …
fn free_slug(base: &str, tmdb_id: Option<i32>, taken: &mut HashSet<String>) -> String {
    let candidates = std::iter::once(base.to_string())
        .chain(tmdb_id.map(|id| format!("{base}-{id}")))
        .chain((2..).map(|n| format!("{base}-{n}")));
    for slug in candidates {
        if taken.insert(slug.clone()) {
            return slug;
        }
    }
    unreachable!("infinite candidates")
}

/// Студии по названиям (порядок — как в `names`); названий без студии нет в ответе.
pub(crate) async fn refs(
    db: impl sqlx::PgExecutor<'_>,
    names: &[String],
) -> sqlx::Result<HashMap<String, StudioRef>> {
    let rows: Vec<StudioRef> =
        sqlx::query_as("SELECT slug, name, logo_url FROM studios WHERE name = ANY($1)")
            .bind(names)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|r| (r.name.clone(), r)).collect())
}

/// Названия студий из `metadata` сущности.
pub(crate) fn names_of(metadata: &Value) -> Vec<String> {
    metadata["studios"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// Страница студии: название, логотип, страна, штаб-квартира, сайт, описание, материнская
/// компания и число фильмов. Список фильмов — `GET /entities?studio=<slug>` (сортировки и
/// пагинация как у каталога). Описание, штаб-квартира и сайт подтягиваются из TMDB при первом
/// открытии и сохраняются; TMDB не ответил — страница без них (попробуется в следующий раз).
#[utoipa::path(
    get, operation_id = "get_studio", path = "/studios/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug студии", example = "warner-bros-pictures")),
    responses(
        (status = 200, description = "Студия", body = StudioDetail),
        (status = 404, description = "Нет такой студии", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<StudioDetail>> {
    let row: Option<StudioRow> = sqlx::query_as(
        "SELECT id, slug, name, tmdb_id, logo_url, origin_country, headquarters, homepage,
                description, parent_tmdb_id, details_fetched_at IS NOT NULL AS details_fetched
         FROM studios WHERE slug = $1",
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    let mut row = row.ok_or(AppError::NotFound)?;
    if let (false, Some(tmdb_id)) = (row.details_fetched, row.tmdb_id) {
        fetch_details(&state, &mut row, tmdb_id).await?;
    }
    let parent: Option<StudioRef> = match row.parent_tmdb_id {
        Some(parent) => {
            sqlx::query_as("SELECT slug, name, logo_url FROM studios WHERE tmdb_id = $1")
                .bind(parent)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    };
    let entities_count: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM entities e WHERE e.metadata @> $1 AND {VISIBLE}
           AND NOT coalesce(e.release_date > current_date, false)"
    ))
    .bind(metadata_filter(&row.name))
    .fetch_one(&state.db)
    .await?;
    Ok(Json(StudioDetail {
        slug: row.slug,
        name: row.name,
        logo_url: row.logo_url,
        origin_country: row.origin_country,
        headquarters: row.headquarters,
        homepage: row.homepage,
        description: row.description,
        parent,
        entities_count,
    }))
}

/// `GET /3/company/{id}` → поля студии, `details_fetched_at`. Нет ключа или TMDB не ответил — без
/// изменений (в следующий раз попробуем снова); 404 — запоминается, что данных нет.
async fn fetch_details(state: &AppState, row: &mut StudioRow, tmdb_id: i32) -> AppResult<()> {
    let data = match enrich::tmdb_get(&state.config, &format!("/company/{tmdb_id}"), &[]).await {
        Ok(data) => data,
        Err(TmdbError::Disabled) => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, studio = %row.slug, "studio details: TMDB request failed");
            return Ok(());
        }
    };
    let text = |key: &str, max: usize| {
        data.as_ref()
            .and_then(|d| d[key].as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.chars().take(max).collect::<String>())
    };
    let homepage = text("homepage", 2048)
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"));
    row.headquarters = text("headquarters", 300).or(row.headquarters.take());
    row.homepage = homepage.or(row.homepage.take());
    row.description = text("description", MAX_TEXT).or(row.description.take());
    row.origin_country = text("origin_country", 2).or(row.origin_country.take());
    row.logo_url = text("logo_path", 300)
        .map(|p| format!("{LOGO}{p}"))
        .or(row.logo_url.take());
    row.parent_tmdb_id = data
        .as_ref()
        .and_then(|d| d["parent_company"]["id"].as_i64())
        .and_then(|id| i32::try_from(id).ok())
        .or(row.parent_tmdb_id);
    sqlx::query(
        "UPDATE studios SET headquarters = $2, homepage = $3, description = $4,
                origin_country = $5, logo_url = $6, parent_tmdb_id = $7, details_fetched_at = now()
         WHERE id = $1",
    )
    .bind(row.id)
    .bind(&row.headquarters)
    .bind(&row.homepage)
    .bind(&row.description)
    .bind(&row.origin_country)
    .bind(&row.logo_url)
    .bind(row.parent_tmdb_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_slugs() {
        let mut taken: HashSet<String> = ["mosfilm".to_string()].into();
        assert_eq!(free_slug("lenfilm", Some(7), &mut taken), "lenfilm");
        assert_eq!(free_slug("mosfilm", Some(5), &mut taken), "mosfilm-5");
        assert_eq!(free_slug("mosfilm", Some(5), &mut taken), "mosfilm-2");
        assert_eq!(free_slug("mosfilm", None, &mut taken), "mosfilm-3");
    }
}
