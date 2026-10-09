//! Сущности: список с фильтрами и карточка. Чтение без авторизации.

use crate::models::{
    page_bounds, Entity, EntityCredit, EntityCreditRow, EntityDetail, EntitySummary,
    ListEntitiesQuery, Page, RelatedWorks, SimilarQuery, Tag, ENTITY_COLUMNS,
    ENTITY_SUMMARY_COLUMNS, TAG_COLUMNS, TAG_ORDER,
};
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AppError, AppResult, AppState};
use sqlx::PgExecutor;
use std::collections::HashMap;
use uuid::Uuid;

/// Общий WHERE списка: $1 kind, $2 slug тега, $3 год, $4 шаблон ILIKE.
const LIST_FILTER: &str = "
    FROM entities e
    WHERE ($1::entity_kind IS NULL OR e.kind = $1)
      AND ($2::text IS NULL OR EXISTS (
            SELECT 1 FROM entity_tags et JOIN tags t ON t.id = et.tag_id
            WHERE et.entity_id = e.id AND t.slug = $2))
      AND ($3::int IS NULL OR (e.release_date >= make_date($3, 1, 1)
                               AND e.release_date < make_date($3 + 1, 1, 1)))
      AND ($4::text IS NULL OR e.title ILIKE $4 OR e.original_title ILIKE $4)";

/// Список сущностей: новинки сверху, без даты — в конце.
#[utoipa::path(
    get, operation_id = "list_entities", path = "/entities", tag = "catalog",
    params(ListEntitiesQuery),
    responses(
        (status = 200, description = "Страница сущностей", body = Page<EntitySummary>),
        (status = 400, description = "Неверный фильтр", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListEntitiesQuery>,
) -> AppResult<Json<Page<EntitySummary>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    if let Some(year) = query.year {
        check_year(year)?;
    }
    let pattern = query.q.as_deref().and_then(like_pattern);

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {LIST_FILTER}"))
        .bind(query.kind)
        .bind(&query.tag)
        .bind(query.year)
        .bind(&pattern)
        .fetch_one(&state.db)
        .await?;
    let items = sqlx::query_as(&format!(
        "SELECT {ENTITY_SUMMARY_COLUMNS} {LIST_FILTER}
         ORDER BY e.release_date DESC NULLS LAST, e.title, e.id
         LIMIT $5 OFFSET $6"
    ))
    .bind(query.kind)
    .bind(&query.tag)
    .bind(query.year)
    .bind(&pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Карточка сущности: поля, теги и участники в порядке титров.
///
/// Читается из PostgreSQL, без кэша и Meilisearch: публичные страницы отдаёт статика
/// фронтенда (SSG/ISR), сюда приходят её пересборка и админка.
#[utoipa::path(
    get, operation_id = "get_entity", path = "/entities/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Карточка", body = EntityDetail),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<EntityDetail>> {
    let entity: Option<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS} FROM entities e WHERE e.slug = $1"
    ))
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(
        detail(&state.db, entity.ok_or(AppError::NotFound)?).await?,
    ))
}

/// Похожие на сущность: тот же тип, больше общих тегов — выше (жанр весит вдвое), при равенстве —
/// новее. Без самой
/// сущности и её связанных (они и так на карточке). Нет тегов — пустая страница.
///
/// Первая версия без ML: один запрос с подсчётом пересечения тегов по `entity_tags (tag_id)`.
#[utoipa::path(
    get, operation_id = "list_similar_entities", path = "/entities/{slug}/similar", tag = "catalog",
    params(
        ("slug" = String, Path, description = "slug сущности", example = "dune-2021"),
        SimilarQuery,
    ),
    responses(
        (status = 200, description = "Похожие, самые похожие сверху", body = Page<EntitySummary>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Не найдена", body = ErrorBody),
    )
)]
pub async fn similar(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<SimilarQuery>,
) -> AppResult<Json<Page<EntitySummary>>> {
    #[derive(sqlx::FromRow)]
    struct Similar {
        total: i64,
        #[sqlx(flatten)]
        entity: EntitySummary,
    }

    let limit = query.limit.unwrap_or(12).clamp(1, 24);
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM entities WHERE slug = $1")
        .bind(&slug)
        .fetch_optional(&state.db)
        .await?;
    let id = id.ok_or(AppError::NotFound)?;
    let rows: Vec<Similar> = sqlx::query_as(&format!(
        "WITH scored AS (
             SELECT other.entity_id, sum(CASE WHEN t.kind = 'genre' THEN 2 ELSE 1 END) AS score
             FROM entity_tags mine
             JOIN tags t ON t.id = mine.tag_id
             JOIN entity_tags other ON other.tag_id = mine.tag_id AND other.entity_id <> mine.entity_id
             WHERE mine.entity_id = $1
             GROUP BY other.entity_id
         )
         SELECT count(*) OVER () AS total, {ENTITY_SUMMARY_COLUMNS}
         FROM scored s JOIN entities e ON e.id = s.entity_id
         WHERE e.kind = (SELECT kind FROM entities WHERE id = $1)
           AND NOT EXISTS (SELECT 1 FROM entity_relations r
                           WHERE (r.from_id = $1 AND r.to_id = e.id)
                              OR (r.to_id = $1 AND r.from_id = e.id))
         ORDER BY s.score DESC, e.release_date DESC NULLS LAST, e.id
         LIMIT $2"
    ))
    .bind(id)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page {
        total: rows.first().map_or(0, |row| row.total),
        items: rows.into_iter().map(|row| row.entity).collect(),
        limit,
        offset: 0,
    }))
}

/// Карточка по id из БД: для админских ответов, событий публикации и документа поискового индекса.
pub(crate) async fn detail_by_id(
    db: impl PgExecutor<'_> + Copy,
    id: Uuid,
) -> AppResult<EntityDetail> {
    let entity: Option<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS} FROM entities e WHERE e.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    detail(db, entity.ok_or(AppError::NotFound)?).await
}

async fn detail(db: impl PgExecutor<'_> + Copy, entity: Entity) -> AppResult<EntityDetail> {
    let mut cards = details(db, vec![entity]).await?;
    Ok(cards.pop().expect("one card per entity"))
}

/// Карточки пачкой: три запроса на любое число сущностей (перестройка индекса). Порядок — как у
/// `entities`.
pub(crate) async fn details(
    db: impl PgExecutor<'_> + Copy,
    entities: Vec<Entity>,
) -> AppResult<Vec<EntityDetail>> {
    #[derive(sqlx::FromRow)]
    struct TagOf {
        entity_id: Uuid,
        #[sqlx(flatten)]
        tag: Tag,
    }
    #[derive(sqlx::FromRow)]
    struct CreditOf {
        entity_id: Uuid,
        #[sqlx(flatten)]
        row: EntityCreditRow,
    }

    let ids: Vec<Uuid> = entities.iter().map(|entity| entity.id).collect();
    let tags: Vec<TagOf> = sqlx::query_as(&format!(
        "SELECT et.entity_id, {TAG_COLUMNS} FROM entity_tags et JOIN tags t ON t.id = et.tag_id
             WHERE et.entity_id = ANY($1) ORDER BY {TAG_ORDER}"
    ))
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let credits: Vec<CreditOf> = sqlx::query_as(&format!(
        "SELECT c.entity_id, {CREDIT_COLUMNS} FROM entity_credits c JOIN people p ON p.id = c.person_id
         WHERE c.entity_id = ANY($1) ORDER BY c.position, p.full_name, c.id"
    ))
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let mut tags_by: HashMap<Uuid, Vec<Tag>> = HashMap::new();
    for TagOf { entity_id, tag } in tags {
        tags_by.entry(entity_id).or_default().push(tag);
    }
    let mut credits_by: HashMap<Uuid, Vec<EntityCredit>> = HashMap::new();
    for CreditOf { entity_id, row } in credits {
        credits_by.entry(entity_id).or_default().push(row.into());
    }
    let mut related_by = related(db, &ids).await?;
    Ok(entities
        .into_iter()
        .map(|entity| EntityDetail {
            tags: tags_by.remove(&entity.id).unwrap_or_default(),
            credits: credits_by.remove(&entity.id).unwrap_or_default(),
            related: related_by.remove(&entity.id).unwrap_or_default(),
            entity,
        })
        .collect())
}

/// Связанные произведения сущностей `ids`. Сиквел в таблице — `from → to`: для `from` это сиквел,
/// для `to` — приквел; ремейк виден с обеих сторон.
async fn related(db: impl PgExecutor<'_>, ids: &[Uuid]) -> AppResult<HashMap<Uuid, RelatedWorks>> {
    #[derive(sqlx::FromRow)]
    struct RelatedOf {
        owner: Uuid,
        group_name: String,
        #[sqlx(flatten)]
        entity: EntitySummary,
    }
    let rows: Vec<RelatedOf> = sqlx::query_as(&format!(
        "SELECT v.owner, v.group_name, {ENTITY_SUMMARY_COLUMNS}
         FROM (
             SELECT from_id AS owner, to_id AS other, 'sequels' AS group_name, position
             FROM entity_relations WHERE kind = 'sequel' AND from_id = ANY($1)
             UNION ALL
             SELECT to_id, from_id, 'prequels', position
             FROM entity_relations WHERE kind = 'sequel' AND to_id = ANY($1)
             UNION ALL
             SELECT from_id, to_id, 'remakes', position
             FROM entity_relations WHERE kind = 'remake' AND from_id = ANY($1)
             UNION ALL
             SELECT to_id, from_id, 'remakes', position
             FROM entity_relations WHERE kind = 'remake' AND to_id = ANY($1)
         ) v JOIN entities e ON e.id = v.other
         ORDER BY e.release_date NULLS LAST, v.position, e.title, e.id"
    ))
    .bind(ids)
    .fetch_all(db)
    .await?;

    let mut by: HashMap<Uuid, RelatedWorks> = HashMap::new();
    for RelatedOf {
        owner,
        group_name,
        entity,
    } in rows
    {
        let works = by.entry(owner).or_default();
        match group_name.as_str() {
            "sequels" => works.sequels.push(entity),
            "prequels" => works.prequels.push(entity),
            _ => works.remakes.push(entity),
        }
    }
    Ok(by)
}

pub(crate) async fn tags_of(db: impl PgExecutor<'_>, entity_id: Uuid) -> AppResult<Vec<Tag>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {TAG_COLUMNS} FROM entity_tags et JOIN tags t ON t.id = et.tag_id
         WHERE et.entity_id = $1 ORDER BY {TAG_ORDER}"
    ))
    .bind(entity_id)
    .fetch_all(db)
    .await?)
}

/// Колонки для [`EntityCreditRow`]: `entity_credits c JOIN people p`.
pub(crate) const CREDIT_COLUMNS: &str = "c.id, c.role, c.character_name, c.position, \
     p.id AS person_id, p.slug AS person_slug, p.full_name AS person_full_name, \
     p.photo_url AS person_photo_url";

pub(crate) fn check_year(year: i32) -> AppResult<()> {
    if (1..=9998).contains(&year) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "year must be between 1 and 9998".into(),
        ))
    }
}

/// Шаблон `ILIKE '%...%'` с экранированием `%`, `_` и `\`. Пустой запрос — без фильтра.
pub(crate) fn like_pattern(q: &str) -> Option<String> {
    let q = q.trim();
    if q.is_empty() {
        return None;
    }
    let mut pattern = String::with_capacity(q.len() + 2);
    pattern.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    Some(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("  "), None);
        assert_eq!(like_pattern("дюна").as_deref(), Some("%дюна%"));
        assert_eq!(like_pattern("100%_\\").as_deref(), Some("%100\\%\\_\\\\%"));
    }
}
