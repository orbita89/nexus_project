//! Раздел «Награды»: премии, церемонии по годам, номинации по категориям.
//!
//! Модель реляционная (`awards`, `award_editions`, `award_categories`, `award_nominations`):
//! награда — это связь «церемония ↔ категория ↔ работа ↔ человек», из плоских строчек
//! `entities.metadata.awards` её не собрать. Строчки в метаданных остаются как были: их показывает
//! блок «Производство и факты», пока контент не переехал на эту модель.
//!
//! Чтение — без авторизации (`GET /awards`, `/awards/{slug}`, `/awards/{slug}/{year}`), правки —
//! в [`crate::admin::awards`]. Карточка работы и профиль человека получают свои награды отсюда же
//! ([`entity_awards`], [`person_awards`]), фасеты поиска — [`facets_of`].

use crate::models::{
    page_bounds, EntityKind, EntitySummary, Page, PersonSummary, ENTITY_SUMMARY_COLUMNS, VISIBLE,
};
use axum::extract::State;
use axum::Json;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AppError, AppResult, AppState};
use sqlx::PgExecutor;
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Сколько победителей главной категории показывать в превью церемонии.
const TOP_WINNERS: usize = 5;

pub const AWARD_COLUMNS: &str = "a.id, a.slug, a.title, a.full_title, a.kinds, a.country, \
     a.founded, a.description, a.logo_url, a.site_url, a.position";

/// Премия целиком (строка `awards`).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Award {
    pub id: Uuid,
    #[schema(example = "oscar")]
    pub slug: String,
    #[schema(example = "Оскар")]
    pub title: String,
    #[schema(example = "Премия Американской киноакадемии")]
    pub full_title: Option<String>,
    /// Типы работ премии: по ним вкладки раздела. У «Золотого глобуса» — `movie` и `series`.
    #[schema(example = json!(["movie"]))]
    pub kinds: Vec<String>,
    #[schema(example = "США")]
    pub country: Option<String>,
    #[schema(example = 1929)]
    pub founded: Option<i32>,
    pub description: Option<String>,
    pub logo_url: Option<String>,
    pub site_url: Option<String>,
    /// Ручной порядок в списке: крупные премии выше.
    pub position: i32,
}

/// Премия в списке раздела: на плитке пишут «1929–2024» и число церемоний.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct AwardListItem {
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub award: Award,
    #[schema(example = 96)]
    pub editions_count: i64,
    #[schema(example = 1929)]
    pub first_year: Option<i32>,
    #[schema(example = 2024)]
    pub latest_year: Option<i32>,
}

/// Премия в ссылке с карточки работы, человека или церемонии. `kinds` — чтобы фронт знал
/// раздел каталога, не разглядывая тип первой работы.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct AwardRef {
    #[schema(example = "oscar")]
    pub slug: String,
    #[schema(example = "Оскар")]
    pub title: String,
    pub logo_url: Option<String>,
    #[schema(example = json!(["movie"]))]
    pub kinds: Vec<String>,
}

/// Страница премии: она сама и её церемонии, новые сверху. Диапазон лет — теми же полями, что
/// в списке раздела, чтобы его не пересчитывали из `editions`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AwardDetail {
    #[serde(flatten)]
    pub award: Award,
    #[schema(example = 1929)]
    pub first_year: Option<i32>,
    #[schema(example = 2024)]
    pub latest_year: Option<i32>,
    #[schema(example = 96)]
    pub editions_count: i64,
    pub editions: Vec<AwardEditionBrief>,
}

/// Церемония в сетке годов на странице премии.
#[derive(Debug, Serialize, ToSchema)]
pub struct AwardEditionBrief {
    #[schema(example = 2024)]
    pub year: i32,
    /// Год работ, если он отличается от года церемонии: «Оскар 2024 — за 2023-й».
    #[schema(example = 2023)]
    pub season_year: Option<i32>,
    #[schema(example = 96)]
    pub number: Option<i32>,
    pub held_on: Option<NaiveDate>,
    pub cover_url: Option<String>,
    #[schema(example = 121)]
    pub nominations_count: i64,
    /// 3–5 победителей главной (первой по порядку) категории — превью на плитке года.
    pub top: Vec<EntitySummary>,
}

/// Церемония целиком: категории и номинации.
#[derive(Debug, Serialize, ToSchema)]
pub struct AwardEditionDetail {
    pub award: AwardRef,
    #[schema(example = 2024)]
    pub year: i32,
    pub season_year: Option<i32>,
    pub number: Option<i32>,
    pub held_on: Option<NaiveDate>,
    #[schema(example = "Dolby Theatre, Лос-Анджелес")]
    pub place: Option<String>,
    pub cover_url: Option<String>,
    pub categories: Vec<AwardCategory>,
}

/// Категория церемонии: номинации уже отсортированы, победители первыми.
#[derive(Debug, Serialize, ToSchema)]
pub struct AwardCategory {
    #[schema(example = "best-picture")]
    pub slug: String,
    #[schema(example = "Лучший фильм")]
    pub title: String,
    pub position: i32,
    pub nominations: Vec<Nomination>,
}

/// Номинация: работа и человек сразу (актёрская категория), либо просто имя.
#[derive(Debug, Serialize, ToSchema)]
pub struct Nomination {
    pub id: Uuid,
    /// Работа, если она есть в каталоге и видима: фронту нужен постер.
    pub entity: Option<EntitySummary>,
    /// Человек, если он есть в каталоге: фронту нужно фото.
    pub person: Option<PersonSummary>,
    /// Кого/что номинировали, если ни работы, ни человека в каталоге нет.
    #[schema(example = "Pixar")]
    pub name: Option<String>,
    #[schema(example = "режиссёр")]
    pub role: Option<String>,
    /// Название категории в этот год, если оно отличалось от нынешнего.
    pub title_override: Option<String>,
    pub won: bool,
}

/// Награда на карточке работы или в профиле человека — ссылкой на церемонию.
#[derive(Debug, Serialize, ToSchema)]
pub struct EntityAward {
    pub award: AwardRef,
    #[schema(example = 2024)]
    pub year: i32,
    pub category: AwardCategoryRef,
    /// Работа, за которую номинирован человек (в профиле человека): «Оппенгеймер».
    pub entity: Option<EntitySummary>,
    pub role: Option<String>,
    pub won: bool,
}

/// Категория в ссылке с карточки.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AwardCategoryRef {
    #[schema(example = "best-picture")]
    pub slug: String,
    #[schema(example = "Лучший фильм")]
    pub title: String,
}

/// Фильтры списка премий.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListAwardsQuery {
    /// Вкладка раздела: только премии этого типа работ.
    pub kind: Option<EntityKind>,
    /// Поиск по названию (полному тоже).
    pub q: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

/// Список премий: ручной порядок, затем алфавит. `?kind=` — вкладки раздела.
#[utoipa::path(
    get, operation_id = "list_awards", path = "/awards", tag = "catalog",
    params(ListAwardsQuery),
    responses(
        (status = 200, description = "Страница премий", body = Page<AwardListItem>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListAwardsQuery>,
) -> AppResult<Json<Page<AwardListItem>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let kind = query.kind.map(|kind| kind.as_str().to_owned());
    let pattern = query.q.as_deref().and_then(crate::entities::like_pattern);
    const WHERE: &str = "($1::text IS NULL OR a.kinds @> ARRAY[$1]::text[])
         AND ($2::text IS NULL OR a.title ILIKE $2 OR a.full_title ILIKE $2)";

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM awards a WHERE {WHERE}"))
        .bind(&kind)
        .bind(&pattern)
        .fetch_one(&state.db)
        .await?;
    let items: Vec<AwardListItem> = sqlx::query_as(&format!(
        "SELECT {AWARD_COLUMNS},
                coalesce(e.count, 0) AS editions_count, e.first_year, e.latest_year
           FROM awards a
           LEFT JOIN LATERAL (SELECT count(*) AS count, min(year) AS first_year,
                                     max(year) AS latest_year
                                FROM award_editions ed WHERE ed.award_id = a.id) e ON true
          WHERE {WHERE}
          ORDER BY a.position DESC, a.title, a.id LIMIT $3 OFFSET $4"
    ))
    .bind(&kind)
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

/// Премия и её церемонии: по ним выбирают год.
#[utoipa::path(
    get, operation_id = "get_award", path = "/awards/{slug}", tag = "catalog",
    params(("slug" = String, Path, description = "slug премии", example = "oscar")),
    responses(
        (status = 200, description = "Премия с церемониями", body = AwardDetail),
        (status = 404, description = "Нет такой премии", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<AwardDetail>> {
    let award = by_slug(&state.db, &slug).await?;

    #[derive(sqlx::FromRow)]
    struct EditionRow {
        id: Uuid,
        year: i32,
        season_year: Option<i32>,
        number: Option<i32>,
        held_on: Option<NaiveDate>,
        cover_url: Option<String>,
        nominations_count: i64,
    }

    let rows: Vec<EditionRow> = sqlx::query_as(
        "SELECT ed.id, ed.year, ed.season_year, ed.number, ed.held_on, ed.cover_url,
                (SELECT count(*) FROM award_nominations n WHERE n.edition_id = ed.id)
                    AS nominations_count
           FROM award_editions ed WHERE ed.award_id = $1 ORDER BY ed.year DESC",
    )
    .bind(award.id)
    .fetch_all(&state.db)
    .await?;

    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let years: Vec<i32> = rows.iter().map(|row| row.year).collect();
    let mut top = top_winners(&state.db, &ids).await?;
    let editions: Vec<AwardEditionBrief> = rows
        .into_iter()
        .map(|row| AwardEditionBrief {
            year: row.year,
            season_year: row.season_year,
            number: row.number,
            held_on: row.held_on,
            cover_url: row.cover_url,
            nominations_count: row.nominations_count,
            top: top.remove(&row.id).unwrap_or_default(),
        })
        .collect();

    Ok(Json(AwardDetail {
        first_year: years.iter().min().copied(),
        latest_year: years.iter().max().copied(),
        editions_count: editions.len() as i64,
        award,
        editions,
    }))
}

/// Победители главной категории церемоний одним запросом: по [`TOP_WINNERS`] на церемонию.
async fn top_winners(
    db: impl PgExecutor<'_>,
    editions: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<EntitySummary>>> {
    #[derive(sqlx::FromRow)]
    struct TopRow {
        edition_id: Uuid,
        #[sqlx(flatten)]
        entity: EntitySummary,
    }

    // Работа попадает в превью один раз, сколько бы побед у неё ни было на этой церемонии:
    // сначала по одной строке на работу (лучшая её категория), потом уже нумерация.
    let rows: Vec<TopRow> = sqlx::query_as(&format!(
        "SELECT t.edition_id, {ENTITY_SUMMARY_COLUMNS} FROM (
             SELECT w.edition_id, w.entity_id,
                    row_number() OVER (PARTITION BY w.edition_id ORDER BY w.rank) AS rn
               FROM (SELECT n.edition_id, n.entity_id,
                            min(ARRAY[c.position, n.position]) AS rank
                       FROM award_nominations n
                       JOIN award_categories c ON c.id = n.category_id
                      WHERE n.edition_id = ANY($1) AND n.won AND n.entity_id IS NOT NULL
                      GROUP BY n.edition_id, n.entity_id) w
         ) t JOIN entities e ON e.id = t.entity_id
          WHERE t.rn <= $2 AND {VISIBLE}
          ORDER BY t.rn"
    ))
    .bind(editions)
    .bind(TOP_WINNERS as i64)
    .fetch_all(db)
    .await?;

    let mut by_edition: HashMap<Uuid, Vec<EntitySummary>> = HashMap::new();
    for row in rows {
        by_edition
            .entry(row.edition_id)
            .or_default()
            .push(row.entity);
    }
    Ok(by_edition)
}

/// Церемония: категории по порядку, в каждой победители первыми.
#[utoipa::path(
    get, operation_id = "get_award_edition", path = "/awards/{slug}/{year}", tag = "catalog",
    params(
        ("slug" = String, Path, description = "slug премии", example = "oscar"),
        ("year" = i32, Path, description = "год церемонии", example = 2024),
    ),
    responses(
        (status = 200, description = "Церемония", body = AwardEditionDetail),
        (status = 404, description = "Нет такой премии или года", body = ErrorBody),
    )
)]
pub async fn edition(
    State(state): State<AppState>,
    Path((slug, year)): Path<(String, i32)>,
) -> AppResult<Json<AwardEditionDetail>> {
    Ok(Json(edition_detail(&state.db, &slug, year).await?))
}

/// Церемония из БД: публичное чтение и ответы админки.
pub(crate) async fn edition_detail(
    db: impl PgExecutor<'_> + Copy,
    slug: &str,
    year: i32,
) -> AppResult<AwardEditionDetail> {
    #[derive(sqlx::FromRow)]
    struct EditionRow {
        id: Uuid,
        year: i32,
        season_year: Option<i32>,
        number: Option<i32>,
        held_on: Option<NaiveDate>,
        place: Option<String>,
        cover_url: Option<String>,
        award_slug: String,
        award_title: String,
        award_logo_url: Option<String>,
        award_kinds: Vec<String>,
    }

    let row: Option<EditionRow> = sqlx::query_as(
        "SELECT ed.id, ed.year, ed.season_year, ed.number, ed.held_on, ed.place, ed.cover_url,
                a.slug AS award_slug, a.title AS award_title, a.logo_url AS award_logo_url,
            a.kinds AS award_kinds,
                a.kinds AS award_kinds
           FROM award_editions ed JOIN awards a ON a.id = ed.award_id
          WHERE a.slug = $1 AND ed.year = $2",
    )
    .bind(slug)
    .bind(year)
    .fetch_optional(db)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;

    Ok(AwardEditionDetail {
        award: AwardRef {
            slug: row.award_slug,
            title: row.award_title,
            logo_url: row.award_logo_url,
            kinds: row.award_kinds,
        },
        year: row.year,
        season_year: row.season_year,
        number: row.number,
        held_on: row.held_on,
        place: row.place,
        cover_url: row.cover_url,
        categories: categories(db, row.id).await?,
    })
}

/// Номинация церемонии: ссылки на работу и человека разворачиваются пачкой.
#[derive(sqlx::FromRow)]
struct NominationRow {
    id: Uuid,
    entity_id: Option<Uuid>,
    person_id: Option<Uuid>,
    name: Option<String>,
    role: Option<String>,
    title_override: Option<String>,
    won: bool,
    category_slug: String,
    category_title: String,
    category_position: i32,
}

/// Категории церемонии с номинациями: один запрос на номинации, по одному на работы и людей.
async fn categories(
    db: impl PgExecutor<'_> + Copy,
    edition: Uuid,
) -> AppResult<Vec<AwardCategory>> {
    let rows: Vec<NominationRow> = sqlx::query_as(
        "SELECT n.id, n.entity_id, n.person_id, n.name, n.role, n.title_override, n.won,
                c.slug AS category_slug, c.title AS category_title, c.position AS category_position
           FROM award_nominations n JOIN award_categories c ON c.id = n.category_id
          WHERE n.edition_id = $1
          ORDER BY c.position, c.title, n.won DESC, n.position, n.id",
    )
    .bind(edition)
    .fetch_all(db)
    .await?;

    let entities = entity_summaries(db, &ids_of(rows.iter().map(|row| row.entity_id))).await?;
    let people = person_summaries(db, &ids_of(rows.iter().map(|row| row.person_id))).await?;

    let mut categories: Vec<AwardCategory> = Vec::new();
    for row in rows {
        let nomination = Nomination {
            id: row.id,
            entity: row.entity_id.and_then(|id| entities.get(&id).cloned()),
            person: row.person_id.and_then(|id| people.get(&id).cloned()),
            name: row.name,
            role: row.role,
            title_override: row.title_override,
            won: row.won,
        };
        match categories.last_mut() {
            Some(last) if last.slug == row.category_slug => last.nominations.push(nomination),
            _ => categories.push(AwardCategory {
                slug: row.category_slug,
                title: row.category_title,
                position: row.category_position,
                nominations: vec![nomination],
            }),
        }
    }
    Ok(categories)
}

fn ids_of(ids: impl Iterator<Item = Option<Uuid>>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = ids.flatten().collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Карточки работ по id — только видимые: скрытую номинация покажет именем.
async fn entity_summaries(
    db: impl PgExecutor<'_>,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, EntitySummary>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<EntitySummary> = sqlx::query_as(&format!(
        "SELECT {ENTITY_SUMMARY_COLUMNS} FROM entities e WHERE e.id = ANY($1) AND {VISIBLE}"
    ))
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|entity| (entity.id, entity)).collect())
}

async fn person_summaries(
    db: impl PgExecutor<'_>,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, PersonSummary>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<PersonSummary> = sqlx::query_as(
        "SELECT p.id, p.slug, p.full_name, p.birth_date, p.photo_url FROM people p
          WHERE p.id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|person| (person.id, person)).collect())
}

/// Премия по slug или 404.
pub(crate) async fn by_slug(db: impl PgExecutor<'_>, slug: &str) -> AppResult<Award> {
    let award: Option<Award> = sqlx::query_as(&format!(
        "SELECT {AWARD_COLUMNS} FROM awards a WHERE a.slug = $1"
    ))
    .bind(slug)
    .fetch_optional(db)
    .await?;
    award.ok_or(AppError::NotFound)
}

// ------------------------------------------------- награды работы и человека

/// Строка награды для карточки: общая для работ и людей.
#[derive(sqlx::FromRow)]
struct EntityAwardRow {
    owner: Uuid,
    entity_id: Option<Uuid>,
    year: i32,
    role: Option<String>,
    won: bool,
    award_slug: String,
    award_title: String,
    award_logo_url: Option<String>,
    award_kinds: Vec<String>,
    category_slug: String,
    category_title: String,
}

/// Общая часть запроса наград: `{owner}` — чьи они, `n.entity_id` / `n.person_id`.
const AWARDS_OF: &str = "SELECT {owner} AS owner, n.entity_id, ed.year, n.role, n.won,
            a.slug AS award_slug, a.title AS award_title, a.logo_url AS award_logo_url,
            a.kinds AS award_kinds,
            coalesce(n.title_override, c.title) AS category_title, c.slug AS category_slug
       FROM award_nominations n
       JOIN award_editions ed ON ed.id = n.edition_id
       JOIN awards a ON a.id = ed.award_id
       JOIN award_categories c ON c.id = n.category_id
      WHERE {owner} = ANY($1)
      ORDER BY n.won DESC, ed.year DESC, a.position DESC, c.position";

/// Награды работ `ids`: полученные сверху, затем по годам (новые первыми).
pub(crate) async fn entity_awards(
    db: impl PgExecutor<'_> + Copy,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<EntityAward>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<EntityAwardRow> = sqlx::query_as(&AWARDS_OF.replace("{owner}", "n.entity_id"))
        .bind(ids)
        .fetch_all(db)
        .await?;
    Ok(group(rows, &HashMap::new()))
}

/// Награды людей `ids`: с работой, за которую номинирован, — профиль показывает её строкой.
pub(crate) async fn person_awards(
    db: impl PgExecutor<'_> + Copy,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, Vec<EntityAward>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<EntityAwardRow> = sqlx::query_as(&AWARDS_OF.replace("{owner}", "n.person_id"))
        .bind(ids)
        .fetch_all(db)
        .await?;
    let entities = entity_summaries(db, &ids_of(rows.iter().map(|row| row.entity_id))).await?;
    Ok(group(rows, &entities))
}

fn group(
    rows: Vec<EntityAwardRow>,
    entities: &HashMap<Uuid, EntitySummary>,
) -> HashMap<Uuid, Vec<EntityAward>> {
    let mut by_owner: HashMap<Uuid, Vec<EntityAward>> = HashMap::new();
    for row in rows {
        by_owner.entry(row.owner).or_default().push(EntityAward {
            award: AwardRef {
                slug: row.award_slug,
                title: row.award_title,
                logo_url: row.award_logo_url,
                kinds: row.award_kinds,
            },
            year: row.year,
            category: AwardCategoryRef {
                slug: row.category_slug,
                title: row.category_title,
            },
            entity: row.entity_id.and_then(|id| entities.get(&id).cloned()),
            role: row.role,
            won: row.won,
        });
    }
    by_owner
}

// ----------------------------------------------------------- фасеты поиска

/// Фасеты наград работы для документа Meilisearch: фильтр «есть „Оскар“» и сортировка по числу
/// наград идут только индексом.
#[derive(Debug, Default, Clone, Serialize, sqlx::FromRow)]
pub struct AwardFacets {
    /// Slug'и премий работы: фильтр `award`.
    pub award_slugs: Vec<String>,
    /// Сколько наград получено.
    pub award_wins: i64,
    /// Сколько всего номинаций (включая победы).
    pub award_nominations: i64,
    /// Годы церемоний: фасет «лауреаты 2024».
    pub award_years: Vec<i32>,
}

/// Фасеты наград для пачки работ: один запрос на пачку.
pub(crate) async fn facets_of(
    db: impl PgExecutor<'_>,
    ids: &[Uuid],
) -> sqlx::Result<HashMap<Uuid, AwardFacets>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    #[derive(sqlx::FromRow)]
    struct FacetRow {
        entity_id: Uuid,
        #[sqlx(flatten)]
        facets: AwardFacets,
    }

    let rows: Vec<FacetRow> = sqlx::query_as(
        "SELECT n.entity_id,
                array_agg(DISTINCT a.slug) AS award_slugs,
                array_agg(DISTINCT ed.year) AS award_years,
                count(*) FILTER (WHERE n.won) AS award_wins,
                count(*) AS award_nominations
           FROM award_nominations n
           JOIN award_editions ed ON ed.id = n.edition_id
           JOIN awards a ON a.id = ed.award_id
          WHERE n.entity_id = ANY($1)
          GROUP BY n.entity_id",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.entity_id, row.facets))
        .collect())
}
