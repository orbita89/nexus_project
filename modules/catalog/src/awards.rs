//! Вкладка «Награды»: список премий, годы премии и фильмы премии.
//!
//! Награды фильма лежат в его же строке (`entities.metadata.awards`: `{award, category, title,
//! year, won}`) — правило Flat First, таблицы связей нет. Таблица `awards` — только справочник
//! премий: slug для URL, название и сколько фильмов её упоминают.
//!
//! Фильмы премии ищутся контейнментом по GIN-индексу `metadata`, как фильмы студии:
//! `metadata @> '{"awards": [{"award": "Оскар"}]}'`. Год и «получена / номинация» — тем же
//! контейнментом с добавленными ключами, поэтому фильтры не стоят ни одного лишнего индекса.
//!
//! Строки создаёт [`sync`]: при перестройке поиска, после правок в админке и вручную
//! (`POST /admin/awards/sync` — им пользуется импорт из Wikidata).

use crate::archive::enrich::slugify;
use crate::models::{EntitySummary, Page, VISIBLE};
use crate::models::{ListAwardEntitiesQuery, ListAwardsQuery, ENTITY_SUMMARY_COLUMNS};
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// Вставка новых премий пачками.
const BATCH: usize = 1_000;

/// Премия в списке вкладки и в шапке её страницы.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct AwardRef {
    pub id: Uuid,
    #[schema(example = "oskar")]
    pub slug: String,
    #[schema(example = "Оскар")]
    pub name: String,
    /// Сколько фильмов каталога её упоминают (и награды, и номинации).
    #[schema(example = 1240)]
    pub entities_count: i64,
}

/// Год премии: сколько в нём наград и номинаций — список годов на странице премии.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct AwardYear {
    #[schema(example = 2024)]
    pub year: i32,
    /// Сколько фильмов что-то получили в этом году.
    #[schema(example = 18)]
    pub won_count: i64,
    /// Сколько номинировались.
    #[schema(example = 97)]
    pub nominated_count: i64,
}

/// Страница премии: она сама и её годы, новые сверху.
#[derive(Debug, Serialize, ToSchema)]
pub struct AwardDetail {
    #[serde(flatten)]
    pub award: AwardRef,
    pub years: Vec<AwardYear>,
}

/// Фильм в списке премии: карточка плюс за что именно он здесь.
#[derive(Debug, Serialize, ToSchema)]
pub struct AwardEntity {
    #[serde(flatten)]
    pub entity: EntitySummary,
    /// Все записи этой премии у фильма за выбранный год (или за все годы).
    pub awards: Vec<crate::metadata::Award>,
}

/// Строка списка премии: карточка плюс сырые награды фильма — из них отбираются записи премии.
#[derive(sqlx::FromRow)]
struct AwardEntityRow {
    metadata: serde_json::Value,
    #[sqlx(flatten)]
    entity: EntitySummary,
}

/// Условие «у фильма есть запись этой премии»: контейнмент по GIN-индексу `metadata`.
fn filter(name: &str, query: &ListAwardEntitiesQuery) -> serde_json::Value {
    let mut entry = json!({ "award": name });
    if let Some(year) = query.year {
        entry["year"] = json!(year);
    }
    if let Some(won) = query.won {
        entry["won"] = json!(won);
    }
    json!({ "awards": [entry] })
}

/// Список премий: сначала те, у кого больше фильмов.
#[utoipa::path(
    get, operation_id = "list_awards", path = "/awards", tag = "catalog",
    params(ListAwardsQuery),
    responses(
        (status = 200, description = "Страница премий", body = Page<AwardRef>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListAwardsQuery>,
) -> AppResult<Json<Page<AwardRef>>> {
    let (limit, offset) = crate::models::page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM awards WHERE entities_count > 0")
        .fetch_one(&state.db)
        .await?;
    let items: Vec<AwardRef> = sqlx::query_as(
        "SELECT id, slug, name, entities_count FROM awards WHERE entities_count > 0
         ORDER BY entities_count DESC, name LIMIT $1 OFFSET $2",
    )
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

/// Премия и её годы: по ним на вкладке выбирают год.
#[utoipa::path(
    get, operation_id = "get_award", path = "/awards/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug премии", example = "oskar")),
    responses(
        (status = 200, description = "Премия с годами", body = AwardDetail),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<AwardDetail>> {
    let award: Option<AwardRef> =
        sqlx::query_as("SELECT id, slug, name, entities_count FROM awards WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(&state.db)
            .await?;
    let award = award.ok_or(AppError::NotFound)?;

    // Годы собираются разворачиванием массива наград: отбор фильмов идёт по GIN-индексу,
    // разворачивается только то, что уже отобрано.
    let years: Vec<AwardYear> = sqlx::query_as(&format!(
        "SELECT (a->>'year')::int AS year,
                count(DISTINCT e.id) FILTER (WHERE (a->>'won')::boolean) AS won_count,
                count(DISTINCT e.id) FILTER (WHERE NOT (a->>'won')::boolean) AS nominated_count
           FROM entities e, jsonb_array_elements(e.metadata -> 'awards') a
          WHERE e.metadata @> $1::jsonb AND {VISIBLE}
            AND a->>'award' = $2 AND a->>'year' IS NOT NULL
          GROUP BY 1 ORDER BY 1 DESC"
    ))
    .bind(json!({ "awards": [{ "award": award.name }] }))
    .bind(&award.name)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(AwardDetail { award, years }))
}

/// Фильмы премии: `?year=` и `?won=` сужают до «что получило “Оскар” в 2024-м».
#[utoipa::path(
    get, operation_id = "list_award_entities", path = "/awards/{slug}/entities", tag = "catalog",
    params(
        ("slug" = String, Path, description = "slug премии", example = "oskar"),
        ListAwardEntitiesQuery,
    ),
    responses(
        (status = 200, description = "Страница фильмов", body = Page<AwardEntity>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
    )
)]
pub async fn entities(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ListAwardEntitiesQuery>,
) -> AppResult<Json<Page<AwardEntity>>> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM awards WHERE slug = $1")
        .bind(&slug)
        .fetch_optional(&state.db)
        .await?;
    let name = name.ok_or(AppError::NotFound)?;
    let (limit, offset) = crate::models::page_bounds(query.limit, query.offset);
    let contains = filter(&name, &query);

    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM entities e WHERE e.metadata @> $1::jsonb AND {VISIBLE}"
    ))
    .bind(&contains)
    .fetch_one(&state.db)
    .await?;
    let rows: Vec<AwardEntityRow> = sqlx::query_as(&format!(
        "SELECT e.metadata, {ENTITY_SUMMARY_COLUMNS} FROM entities e
          WHERE e.metadata @> $1::jsonb AND {VISIBLE}
          ORDER BY e.release_date DESC NULLS LAST, e.title, e.id LIMIT $2 OFFSET $3"
    ))
    .bind(&contains)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    let items = rows
        .into_iter()
        .map(|row| AwardEntity {
            awards: crate::metadata::awards_of(&row.metadata)
                .into_iter()
                .filter(|award| {
                    award.award.as_deref() == Some(name.as_str())
                        && query.year.is_none_or(|year| award.year == Some(year))
                        && query.won.is_none_or(|won| award.won == won)
                })
                .collect(),
            entity: row.entity,
        })
        .collect();

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Пересобрать справочник премий: новые названия из `metadata.awards[].award` и счётчики.
#[utoipa::path(
    post, operation_id = "admin_sync_awards", path = "/admin/awards/sync", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Справочник обновлён", body = SyncResult),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn sync_handler(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<Json<SyncResult>> {
    let created = sync(&state.db).await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM awards")
        .fetch_one(&state.db)
        .await?;
    tracing::info!(admin_id = %admin.id, created, total, "awards synced by admin");
    Ok(Json(SyncResult { created, total }))
}

/// Сколько премий завелось и сколько их всего.
#[derive(Debug, Serialize, ToSchema)]
pub struct SyncResult {
    #[schema(example = 42)]
    pub created: u64,
    #[schema(example = 130)]
    pub total: i64,
}

/// Заводит премии, которых ещё нет, и пересчитывает счётчики фильмов. Возвращает, сколько
/// завелось. Зовётся при перестройке поиска, после правок в админке и из `/admin/awards/sync`.
pub async fn sync(db: &PgPool) -> sqlx::Result<u64> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT a->>'award' FROM entities e,
                jsonb_array_elements(e.metadata -> 'awards') a
          WHERE jsonb_typeof(e.metadata -> 'awards') = 'array' AND a->>'award' IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM awards w WHERE w.name = a->>'award')",
    )
    .fetch_all(db)
    .await?;
    let names: Vec<String> = names.into_iter().filter(|n| !n.trim().is_empty()).collect();

    let mut created = 0;
    for chunk in names.chunks(BATCH) {
        // slug занят другой премией (разные названия сворачиваются в один slug) — с суффиксом.
        let slugs: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let slug = slugify(name);
                if slug.is_empty() {
                    format!("award-{i}")
                } else {
                    slug
                }
            })
            .collect();
        created += sqlx::query(
            "INSERT INTO awards (slug, name)
             SELECT CASE WHEN EXISTS (SELECT 1 FROM awards w WHERE w.slug = u.slug)
                         THEN u.slug || '-' || substr(md5(u.name), 1, 6) ELSE u.slug END,
                    u.name
               FROM unnest($1::text[], $2::text[]) AS u(slug, name)
             ON CONFLICT DO NOTHING",
        )
        .bind(&slugs)
        .bind(chunk)
        .execute(db)
        .await?
        .rows_affected();
    }

    // Счётчики: один проход по наградам всех фильмов.
    sqlx::query(
        "UPDATE awards w SET entities_count = COALESCE(c.n, 0)
           FROM (SELECT w2.id, count(DISTINCT e.id) AS n
                   FROM awards w2
                   LEFT JOIN entities e
                     ON e.metadata @> jsonb_build_object('awards',
                            jsonb_build_array(jsonb_build_object('award', w2.name)))
                  GROUP BY w2.id) c
          WHERE c.id = w.id AND w.entities_count <> COALESCE(c.n, 0)",
    )
    .execute(db)
    .await?;
    Ok(created)
}
