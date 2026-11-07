//! Админка серий сезона: список серий заменяется целиком.
//!
//! Массовая заливка важнее ручного ввода — на сезон из 22 серий руками никто не пойдёт, а TMDB
//! отдаёт сезон целиком (`GET /tv/{id}/season/{n}`). Поэтому одна ручка `PUT`, а не четыре на
//! каждую серию: админка и импорт присылают весь список, лишние строки снимаются.

use crate::jobs::Reporter;
use crate::models::EntityKind;
use crate::publish::{self, Touched};
use crate::seasons::{episodes_page, Episode};
use crate::validate::{self, MAX_TITLE};
use axum::extract::State;
use axum::Json;
use chrono::NaiveDate;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::ToSchema;
use uuid::Uuid;

/// Серий в сезоне: больше не бывает даже у ежедневных шоу с разбивкой по годам.
const MAX_EPISODES: usize = 1000;
/// Описание серии — как просил фронтенд: до 2000 символов, обрезать на бэке не надо.
const MAX_OVERVIEW: usize = 2000;
/// Длительность серии в минутах.
const MAX_RUNTIME: i32 = 10_000;
/// Размер кадра в пикселях.
const MAX_STILL_PX: i32 = 100_000;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetEpisodes {
    /// Серии сезона целиком: присланный список заменяет текущий. Пустой — снять все серии.
    pub episodes: Vec<EpisodeInput>,
}

/// Серия на входе. Ни «просмотрено», ни прогресса здесь нет и не будет: видео у нас нет.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeInput {
    /// Номер серии в сезоне, 0–10000; повторы в списке — 400.
    #[schema(example = 1)]
    pub number: i32,
    #[schema(example = "Тяжёлые времена")]
    pub title: Option<String>,
    #[schema(example = "Welcome to the Playground")]
    pub original_title: Option<String>,
    /// До 2000 символов.
    pub overview: Option<String>,
    pub air_date: Option<NaiveDate>,
    /// Целые минуты, 1–10000.
    #[schema(example = 42)]
    pub runtime_minutes: Option<i32>,
    /// Кадр из серии (16:9), `http(s)://...`. Постер сериала или сезона сюда класть не надо —
    /// пропорции другие, карточка поедет.
    pub still_url: Option<String>,
    /// Размеры кадра в пикселях: только вдвоём и только вместе с `still_url`.
    pub still_width: Option<i32>,
    pub still_height: Option<i32>,
}

/// Заменить серии сезона целиком. Номера уникальны внутри сезона; строки, которых нет в
/// присланном списке, удаляются.
#[utoipa::path(
    put, operation_id = "set_season_episodes", path = "/admin/seasons/{id}/episodes",
    tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сезона (сущность `kind = season`)")),
    request_body = SetEpisodes,
    responses(
        (status = 200, description = "Серии сезона", body = Vec<Episode>),
        (status = 400, description = "Неверные поля или повтор номера", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Сезон не найден", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<SetEpisodes>,
) -> AppResult<Json<Vec<Episode>>> {
    if req.episodes.len() > MAX_EPISODES {
        return Err(AppError::BadRequest(format!(
            "no more than {MAX_EPISODES} episodes per season"
        )));
    }
    let rows = req
        .episodes
        .into_iter()
        .map(check)
        .collect::<AppResult<Vec<_>>>()?;
    let mut numbers: Vec<i32> = rows.iter().map(|row| row.number).collect();
    numbers.sort_unstable();
    let unique = numbers.len();
    numbers.dedup();
    if numbers.len() != unique {
        return Err(AppError::BadRequest(
            "episode numbers must be unique within a season".into(),
        ));
    }

    let mut tx = state.db.begin().await?;
    // FOR UPDATE: параллельное удаление сезона не проскочит между проверкой и вставкой.
    let kind: Option<EntityKind> =
        sqlx::query_scalar("SELECT kind FROM entities WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if kind.ok_or(AppError::NotFound)? != EntityKind::Season {
        return Err(AppError::BadRequest("entity is not a season".into()));
    }
    sqlx::query("DELETE FROM episodes WHERE season_id = $1 AND NOT (number = ANY($2))")
        .bind(id)
        .bind(&numbers)
        .execute(&mut *tx)
        .await?;
    for row in &rows {
        sqlx::query(
            "INSERT INTO episodes (season_id, number, title, original_title, overview, air_date,
                                   runtime_minutes, still_url, still_width, still_height)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (season_id, number) DO UPDATE SET
                 title = EXCLUDED.title, original_title = EXCLUDED.original_title,
                 overview = EXCLUDED.overview, air_date = EXCLUDED.air_date,
                 runtime_minutes = EXCLUDED.runtime_minutes, still_url = EXCLUDED.still_url,
                 still_width = EXCLUDED.still_width, still_height = EXCLUDED.still_height",
        )
        .bind(id)
        .bind(row.number)
        .bind(&row.title)
        .bind(&row.original_title)
        .bind(&row.overview)
        .bind(row.air_date)
        .bind(row.runtime_minutes)
        .bind(&row.still_url)
        .bind(row.still_width)
        .bind(row.still_height)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    // Страницы сезона и сериала: число серий и карусель на них меняются.
    publish::run(&state, Touched::entities(&[id]), &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, season_id = %id, episodes = rows.len(), "episodes set");
    let (items, _) = episodes_page(&state.db, id, MAX_EPISODES as i64, 0).await?;
    Ok(Json(items))
}

/// Проверенная серия: те же ограничения, что у остальных полей каталога.
struct Row {
    number: i32,
    title: Option<String>,
    original_title: Option<String>,
    overview: Option<String>,
    air_date: Option<NaiveDate>,
    runtime_minutes: Option<i32>,
    still_url: Option<String>,
    still_width: Option<i32>,
    still_height: Option<i32>,
}

fn check(input: EpisodeInput) -> AppResult<Row> {
    if !(0..=10_000).contains(&input.number) {
        return Err(AppError::BadRequest("number must be 0-10000".into()));
    }
    if input
        .runtime_minutes
        .is_some_and(|value| !(1..=MAX_RUNTIME).contains(&value))
    {
        return Err(AppError::BadRequest(format!(
            "runtime_minutes must be 1-{MAX_RUNTIME}"
        )));
    }
    let still_url = validate::url("still_url", input.still_url)?;
    let (still_width, still_height) =
        still_size(still_url.is_some(), input.still_width, input.still_height)?;
    Ok(Row {
        number: input.number,
        title: validate::optional("title", input.title, MAX_TITLE)?,
        original_title: validate::optional("original_title", input.original_title, MAX_TITLE)?,
        overview: validate::optional("overview", input.overview, MAX_OVERVIEW)?,
        air_date: input.air_date,
        runtime_minutes: input.runtime_minutes,
        still_url,
        still_width,
        still_height,
    })
}

/// Размеры кадра: как `cover_size` у обложки — только вдвоём и только вместе с картинкой.
fn still_size(
    has_still: bool,
    width: Option<i32>,
    height: Option<i32>,
) -> AppResult<(Option<i32>, Option<i32>)> {
    let (width, height) = match (width, height) {
        (None, None) => return Ok((None, None)),
        (Some(width), Some(height)) => (width, height),
        _ => {
            return Err(AppError::BadRequest(
                "still_width and still_height must be given together".into(),
            ))
        }
    };
    if !has_still {
        return Err(AppError::BadRequest(
            "still_width and still_height are only accepted with still_url".into(),
        ));
    }
    for (field, value) in [("still_width", width), ("still_height", height)] {
        if !(1..=MAX_STILL_PX).contains(&value) {
            return Err(AppError::BadRequest(format!(
                "{field} must be 1-{MAX_STILL_PX}"
            )));
        }
    }
    Ok((Some(width), Some(height)))
}
