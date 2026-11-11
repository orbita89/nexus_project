//! Админка серий сезона: список серий заменяется целиком.
//!
//! Массовая заливка важнее ручного ввода — на сезон из 22 серий руками никто не пойдёт, а TMDB
//! отдаёт сезон целиком (`GET /tv/{id}/season/{n}`). Поэтому одна ручка `PUT`, а не четыре на
//! каждую серию: админка и импорт присылают весь список, лишние строки снимаются.

use crate::jobs::Reporter;
use crate::models::EntityKind;
use crate::publish::{self, Touched};
use crate::seasons::{Episode, EPISODES_JSON};
use crate::validate::{self, MAX_TITLE};
use axum::extract::State;
use axum::Json;
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppError, AppResult, AppState};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

// Ограничения общие с `metadata.episodes`: набор серий приходит и сюда, и в `metadata`
// при `PATCH /admin/entities`, правила должны быть одни.
use crate::metadata::{MAX_EPISODES, MAX_OVERVIEW, MAX_RUNTIME};

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
    // Набор хранится отсортированным по номеру: порядок задаёт запись, чтение его не трогает
    // (срез страницы режется по позиции в массиве).
    let mut rows = rows;
    rows.sort_by_key(|row| row.number);
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
    // FOR UPDATE: параллельное удаление сезона не проскочит между проверкой и записью, а
    // одновременные PUT'ы не затрут набор друг друга (он пишется целиком).
    let found: Option<(EntityKind, serde_json::Value)> = sqlx::query_as(&format!(
        "SELECT kind, {EPISODES_JSON} FROM entities e WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (kind, current) = found.ok_or(AppError::NotFound)?;
    if kind != EntityKind::Season {
        return Err(AppError::BadRequest("entity is not a season".into()));
    }
    // id и created_at серии переживают перезапись: по ним фронтенд держит кэш, а импорт
    // гоняет один и тот же сезон много раз.
    let previous: Vec<Episode> = serde_json::from_value(current).unwrap_or_default();
    let known: HashMap<i32, &Episode> = previous.iter().map(|row| (row.number, row)).collect();
    let now = Utc::now();
    let items: Vec<Episode> = rows
        .iter()
        .map(|row| {
            let old = known.get(&row.number);
            Episode {
                id: old.map_or_else(Uuid::new_v4, |old| old.id),
                number: row.number,
                title: row.title.clone(),
                original_title: row.original_title.clone(),
                overview: row.overview.clone(),
                air_date: row.air_date,
                runtime_minutes: row.runtime_minutes,
                still_url: row.still_url.clone(),
                still_width: row.still_width,
                still_height: row.still_height,
                created_at: old.map_or(now, |old| old.created_at),
                // Не изменилась — не трогаем `updated_at`: иначе каждый прогон импорта
                // дёргает ISR и ETag всей карусели.
                updated_at: match old {
                    Some(old) if same(old, row) => old.updated_at,
                    _ => now,
                },
            }
        })
        .collect();
    sqlx::query(
        "UPDATE entities SET metadata = jsonb_set(metadata, '{episodes}', $2) WHERE id = $1",
    )
    .bind(id)
    .bind(
        serde_json::to_value(&items)
            .map_err(|error| AppError::Internal(format!("episodes: {error}")))?,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // Страницы сезона и сериала: число серий и карусель на них меняются.
    publish::run(&state, Touched::entities(&[id]), &Reporter::silent()).await;
    tracing::info!(admin_id = %admin.id, season_id = %id, episodes = items.len(), "episodes set");
    Ok(Json(items))
}

/// Совпадают ли показываемые поля серии: только ради `updated_at`.
fn same(old: &Episode, new: &Row) -> bool {
    old.title == new.title
        && old.original_title == new.original_title
        && old.overview == new.overview
        && old.air_date == new.air_date
        && old.runtime_minutes == new.runtime_minutes
        && old.still_url == new.still_url
        && old.still_width == new.still_width
        && old.still_height == new.still_height
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
/// Правило одно на оба пути записи, поэтому живёт в `metadata`.
use crate::metadata::still_size;
