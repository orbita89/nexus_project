//! Сезоны сериалов и их серии.
//!
//! Сезон — сущность (`entities.kind = 'season'`) со своим slug'ом (`arcane-season-2`) и своим
//! сериалом в `entities.parent_id`: оценка, рецензия, лайк, отметка и коллекция работают по
//! `/api/v1/social/entities/{slug}` без единой новой социальной ручки. Оценка сериала и оценки
//! сезонов не смешиваются — это разные сущности, и у каждой своя сводка в `entity_ratings`.
//!
//! Серия — не сущность: строка `episodes`, без slug'а, рецензий и подборок. Видео у нас нет,
//! поэтому ни «просмотрено», ни прогресса, ни позиции в серии здесь не появится.
//!
//! В списки каталога и в общий поиск сезоны не попадают ([`IN_CATALOG`]): в разделе «Сериалы»
//! должен быть «Аркейн», а не «Аркейн, сезон 1» и «Аркейн, сезон 2».

use crate::entities::is_admin;
use crate::metadata::SeriesStatus;
use crate::models::{
    page_bounds, Entity, EntityDetail, EntityKind, EntitySummary, Page, ENTITY_COLUMNS_LEAN,
    ENTITY_STATS_JOIN, ENTITY_SUMMARY_COLUMNS, VISIBLE,
};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::{AppError, AppResult, AppState, AuthUser};
use sqlx::PgPool;
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Серий на странице по умолчанию: в сезоне их редко больше.
const EPISODES_PAGE: i64 = 50;

/// Сезоны сериала по возрастанию номера; спецвыпуски (0) — в конце.
const SEASON_ORDER: &str = "((e.metadata ->> 'number')::int = 0), (e.metadata ->> 'number')::int, \
     e.release_date NULLS LAST, e.id";

/// Средняя оценка сущности и число оценок: те же числа и та же семантика, что у карточки
/// (`entity_ratings`, сводки присылает `social`).
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct RatingValue {
    /// Среднее, округлённое до 0,1; оценок нет — `null`.
    #[schema(example = 9.2)]
    pub value: Option<f64>,
    #[schema(example = 420)]
    pub count: i64,
}

/// Таблетка сезона в шапке сериала: `9.5 ★ · 6 серий` рисуется прямо отсюда, второго запроса
/// за рейтингами быть не должно.
#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonSummary {
    pub id: Uuid,
    #[schema(example = "arcane-season-2")]
    pub slug: String,
    /// Номер сезона; 0 — спецвыпуски.
    #[schema(example = 2)]
    pub number: Option<i32>,
    #[schema(example = "Сезон 2")]
    pub title: String,
    #[schema(example = "Arcane: Season 2")]
    pub original_title: Option<String>,
    /// Дата выхода первой серии (`release_date` сущности).
    pub premiere_date: Option<NaiveDate>,
    /// Дата выхода последней вышедшей серии.
    pub finale_date: Option<NaiveDate>,
    #[schema(example = 6)]
    pub episode_count: Option<i32>,
    pub status: Option<SeriesStatus>,
    /// Постер сезона и размеры файла в пикселях (точное `aspect-ratio` до загрузки).
    pub cover_url: Option<String>,
    pub cover_width: Option<i32>,
    pub cover_height: Option<i32>,
    pub rating: RatingValue,
    /// Своя оценка зрителя, 1–10; гостю и без оценки — `null`.
    #[schema(example = 10)]
    pub my_rating: Option<i16>,
}

/// Сезоны сериала одним ответом: без `total`, их всегда немного.
#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonList {
    pub items: Vec<SeasonSummary>,
}

/// Карточка сезона: поля сущности (`metadata` — [`crate::metadata::SeasonMetadata`]), сериал для
/// шапки и хлебных крошек и серии сезона.
#[derive(Debug, Serialize, ToSchema)]
pub struct SeasonDetail {
    #[serde(flatten)]
    pub entity: EntityDetail,
    /// Сериал, которому принадлежит сезон; родителя нет (данные битые) — `null`.
    pub series: Option<EntitySummary>,
    pub rating: RatingValue,
    /// Своя оценка зрителя, 1–10; гостю и без оценки — `null`.
    pub my_rating: Option<i16>,
    /// Серии сезона по возрастанию номера, не больше [`MAX_PAGE_SIZE`](shared::pagination::MAX_PAGE_SIZE);
    /// длиннее — `GET /seasons/{slug}/episodes`.
    pub episodes: Vec<Episode>,
}

/// Серия сезона. Своего slug'а, рецензий и подборок у серии нет — она не сущность, а данные
/// сезона: хранится элементом массива `entities.metadata.episodes`, а не строкой таблицы.
///
/// Пустые поля в хранилище не лежат (`skip_serializing_if`), поэтому все они с `serde(default)`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Episode {
    pub id: Uuid,
    #[schema(example = 1)]
    pub number: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Тяжёлые времена")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Welcome to the Playground")]
    pub original_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_date: Option<NaiveDate>,
    /// Целые минуты.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 42)]
    pub runtime_minutes: Option<i32>,
    /// Кадр из серии, 16:9 — не постер сериала и не постер сезона. Кадра нет — `null`, и
    /// заглушку (постер сезона, иначе сериала) подставляет фронтенд: пропорции у них другие.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still_url: Option<String>,
    /// Размеры кадра в пикселях: карточка держит точное `aspect-ratio` до загрузки.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still_width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still_height: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Массив серий сезона из `metadata`, уже отсортированный записью: `'[]'` там, где ключа нет.
pub(crate) const EPISODES_JSON: &str =
    "CASE WHEN jsonb_typeof(e.metadata -> 'episodes') = 'array' \
          THEN e.metadata -> 'episodes' ELSE '[]'::jsonb END";

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EpisodesQuery {
    /// 1–100, по умолчанию 50.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Сезоны сериала: переключатель `Весь сериал · 1 сезон · 2 сезон` в шапке.
///
/// Порядок — по номеру, спецвыпуски (0) в конце. `rating` и `my_rating` — те же поля и та же
/// семантика, что у сущности в карточке; гостю `my_rating: null`. Гостю ответ кэшируется.
///
/// Сериал не найден или скрыт — 404 (админу видны и скрытые, вместе со скрытыми сезонами).
#[utoipa::path(
    get, operation_id = "list_entity_seasons", path = "/entities/{slug}/seasons", tag = "catalog",
    security((), ("bearer" = [])),
    params(("slug" = String, Path, description = "slug сериала", example = "arcane")),
    responses(
        (status = 200, description = "Сезоны сериала", body = SeasonList),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Сериал не найден или скрыт", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<impl IntoResponse> {
    let admin = is_admin(viewer.as_ref());
    let found: Option<(Uuid, bool)> = sqlx::query_as(&format!(
        "SELECT e.id, {VISIBLE} FROM entities e WHERE e.slug = $1 AND e.kind <> 'season'"
    ))
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?;
    let (series_id, _) = found
        .filter(|(_, visible)| *visible || admin)
        .ok_or(AppError::NotFound)?;

    let seasons: Vec<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS_LEAN} FROM entities e {ENTITY_STATS_JOIN}
         WHERE e.parent_id = $1 AND ($2 OR {VISIBLE}) ORDER BY {SEASON_ORDER}"
    ))
    .bind(series_id)
    .bind(admin)
    .fetch_all(&state.db)
    .await?;

    let ids: Vec<Uuid> = seasons.iter().map(|season| season.id).collect();
    let ratings = ratings_of(&state.db, &ids).await?;
    let mine = my_ratings(&state, viewer.as_ref(), &ids).await?;
    let items = seasons
        .into_iter()
        .map(|season| {
            let meta = &season.metadata;
            SeasonSummary {
                number: number_of(meta),
                title: season.title,
                original_title: season.original_title,
                premiere_date: season.release_date,
                finale_date: meta
                    .get("finale_date")
                    .and_then(|v| v.as_str())
                    .and_then(|v| v.parse().ok()),
                episode_count: meta
                    .get("episode_count")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|v| i32::try_from(v).ok()),
                status: meta
                    .get("status")
                    .and_then(|v| serde_json::from_value(v.clone()).ok()),
                cover_url: season.cover_url,
                cover_width: season.cover_width,
                cover_height: season.cover_height,
                rating: ratings.get(&season.id).copied().unwrap_or(NO_RATING),
                my_rating: mine.get(&season.id).copied(),
                slug: season.slug,
                id: season.id,
            }
        })
        .collect();
    Ok((cache_headers(viewer.is_some()), Json(SeasonList { items })))
}

/// Карточка сезона: поля сущности, сериал для шапки и хлебных крошек и серии сезона.
///
/// Социальное (оценка, рецензия, лайк, отметка, коллекция) — существующими ручками `social` по
/// этому же slug'у: сезон для них обычная сущность.
#[utoipa::path(
    get, operation_id = "get_season", path = "/seasons/{slug}", tag = "catalog",
    security((), ("bearer" = [])),
    params(("slug" = String, Path, description = "slug сезона", example = "arcane-season-2")),
    responses(
        (status = 200, description = "Карточка сезона", body = SeasonDetail),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Не найден или скрыт", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
) -> AppResult<Json<SeasonDetail>> {
    let admin = is_admin(viewer.as_ref());
    let season = season_by_slug(&state.db, &slug, admin).await?;
    let id = season.id;
    let parent_id = season.parent_id;
    let detail = crate::entities::detail(&state.db, season, admin).await?;

    let series: Option<EntitySummary> = match parent_id {
        Some(parent_id) => {
            sqlx::query_as(&format!(
                "SELECT {ENTITY_SUMMARY_COLUMNS} FROM entities e
                 WHERE e.id = $1 AND ($2 OR {VISIBLE})"
            ))
            .bind(parent_id)
            .bind(admin)
            .fetch_optional(&state.db)
            .await?
        }
        None => None,
    };
    let (episodes, _) = episodes_page(&state.db, id, shared::pagination::MAX_PAGE_SIZE, 0).await?;
    let ids = [id];
    Ok(Json(SeasonDetail {
        entity: detail,
        series,
        rating: ratings_of(&state.db, &ids)
            .await?
            .get(&id)
            .copied()
            .unwrap_or(NO_RATING),
        my_rating: my_ratings(&state, viewer.as_ref(), &ids)
            .await?
            .get(&id)
            .copied(),
        episodes,
    }))
}

/// Серии сезона: страница `?limit=1..100&offset=` (по умолчанию 50).
///
/// Серии, которые ещё не вышли (`air_date` в будущем), тоже отдаются: карусель покажет их
/// неактивными с датой. Оценок у серий нет — они не сущности.
#[utoipa::path(
    get, operation_id = "list_season_episodes", path = "/seasons/{slug}/episodes", tag = "catalog",
    security((), ("bearer" = [])),
    params(
        ("slug" = String, Path, description = "slug сезона", example = "arcane-season-2"),
        EpisodesQuery,
    ),
    responses(
        (status = 200, description = "Серии сезона", body = Page<Episode>),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Сезон не найден или скрыт", body = ErrorBody),
    )
)]
pub async fn episodes(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path(slug): Path<String>,
    Query(query): Query<EpisodesQuery>,
) -> AppResult<impl IntoResponse> {
    let admin = is_admin(viewer.as_ref());
    let season = season_by_slug(&state.db, &slug, admin).await?;
    let (limit, offset) = page_bounds(Some(query.limit.unwrap_or(EPISODES_PAGE)), query.offset);
    let (items, total) = episodes_page(&state.db, season.id, limit, offset).await?;
    Ok((
        cache_headers(viewer.is_some()),
        Json(Page {
            items,
            total,
            limit,
            offset,
        }),
    ))
}

/// Сезон по slug'у: невидимый — 404 для всех, кроме админа (как любая сущность).
async fn season_by_slug(db: &PgPool, slug: &str, admin: bool) -> AppResult<Entity> {
    let season: Option<Entity> = sqlx::query_as(&format!(
        "SELECT {ENTITY_COLUMNS_LEAN} FROM entities e {ENTITY_STATS_JOIN}
         WHERE e.slug = $1 AND e.kind = 'season'"
    ))
    .bind(slug)
    .fetch_optional(db)
    .await?;
    season
        .filter(|season| season.visible || admin)
        .ok_or(AppError::NotFound)
}

pub(crate) async fn episodes_page(
    db: &PgPool,
    season_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<Episode>, i64)> {
    // Срез режется в SQL: тащить весь массив сезона ради страницы в 50 серий незачем.
    // Порядок — тот, в котором серии записаны (по номеру), ordinality его сохраняет.
    let row: Option<(i32, serde_json::Value)> = sqlx::query_as(&format!(
        "WITH season AS (SELECT {EPISODES_JSON} AS eps FROM entities e WHERE e.id = $1)
         SELECT jsonb_array_length(eps),
                coalesce((SELECT jsonb_agg(value ORDER BY ord)
                            FROM jsonb_array_elements(eps) WITH ORDINALITY t(value, ord)
                           WHERE ord > $3 AND ord <= $3 + $2), '[]'::jsonb)
           FROM season"
    ))
    .bind(season_id)
    .bind(limit)
    .bind(offset)
    .fetch_optional(db)
    .await?;
    let Some((total, items)) = row else {
        return Ok((Vec::new(), 0));
    };
    let items: Vec<Episode> = serde_json::from_value(items).map_err(|error| {
        tracing::error!(%season_id, %error, "broken episodes in season metadata");
        AppError::Internal("broken episodes in season metadata".into())
    })?;
    Ok((items, i64::from(total)))
}

const NO_RATING: RatingValue = RatingValue {
    value: None,
    count: 0,
};

/// Сводки оценок сущностей (`entity_ratings` — таблица каталога, пишет её `social`).
/// Среднее округляется до 0,1, как в `/social/entities/{slug}/rating`.
async fn ratings_of(db: &PgPool, ids: &[Uuid]) -> AppResult<HashMap<Uuid, RatingValue>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Uuid, Option<f64>, i64)> = sqlx::query_as(
        "SELECT entity_id, average, count FROM entity_ratings WHERE entity_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, average, count)| {
            (
                id,
                RatingValue {
                    value: average.map(|average| (average * 10.0).round() / 10.0),
                    count,
                },
            )
        })
        .collect())
}

/// Свои оценки зрителя: читаются справочником `social` (`UserRatings`), а не JOIN'ом к его
/// таблице рецензий. Гость — пусто.
async fn my_ratings(
    state: &AppState,
    viewer: Option<&AuthUser>,
    ids: &[Uuid],
) -> AppResult<HashMap<Uuid, i16>> {
    match viewer {
        Some(user) => state.user_ratings.mine(user.id, ids).await,
        None => Ok(HashMap::new()),
    }
}

/// Гостю ответ кэшируется (как `/search`), вошедшему — нет: в нём его собственная оценка.
fn cache_headers(authorized: bool) -> [(header::HeaderName, &'static str); 1] {
    let value = if authorized {
        "private, no-store"
    } else {
        "public, s-maxage=600"
    };
    [(header::CACHE_CONTROL, value)]
}

/// Номер сезона из `metadata`.
pub(crate) fn number_of(metadata: &serde_json::Value) -> Option<i32> {
    metadata
        .get("number")
        .and_then(serde_json::Value::as_i64)
        .and_then(|number| i32::try_from(number).ok())
}

/// Сезон принадлежит сериалу: slug сериала → его id. Не сериал или нет такого — 400.
pub(crate) async fn series_id_by_slug(db: &PgPool, slug: &str) -> AppResult<Uuid> {
    let found: Option<(Uuid, EntityKind)> =
        sqlx::query_as("SELECT id, kind FROM entities WHERE slug = $1")
            .bind(slug)
            .fetch_optional(db)
            .await?;
    match found {
        Some((id, EntityKind::Series)) => Ok(id),
        Some(_) => Err(AppError::BadRequest(format!(
            "entity {slug} is not a series"
        ))),
        None => Err(AppError::BadRequest(format!("series {slug} not found"))),
    }
}
