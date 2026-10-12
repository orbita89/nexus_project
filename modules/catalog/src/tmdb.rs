//! Операции админки с данными TMDB: обновить `popularity` из ежедневного дампа и пересобрать
//! активные сущности (топ по `vote_count` и `popularity`).
//!
//! Обе долгие (минуты), запускаются вручную, ход — потоком шагов ([`crate::jobs`]), как у
//! перестройки поиска. Одна за раз: общая блокировка с перестройкой ([`ReindexLock`]), занято —
//! `409` до потока. В конце — перестройка поиска (если Meilisearch включён): поиск сортирует по
//! `popularity` и содержит только активные.
//!
//! Связь с TMDB — таблица `entity_tmdb` (пишет импорт `scripts/catalog-import/merge.py`).

use crate::jobs::{self, DoneEvent, Reporter, Step, StepStatus};
use crate::models::{ActiveQuery, ActiveResult, PopularityResult};
use crate::search::{reindex_locked, ReindexError, ReindexLock, ReindexOptions};
use axum::extract::State;
use axum::response::Response;
use chrono::{Days, Utc};
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::Query;
use shared::{AdminUser, AppError, AppResult, AppState};
use sqlx::PgPool;
use std::io::BufRead;
use std::time::Duration;
use uuid::Uuid;

/// Дамп публикуется около 08:00 UTC: сегодняшнего нет — берётся вчерашний и так далее.
const EXPORT_DAYS: u64 = 4;
/// Сущностей на один `UPDATE`: короткие блокировки строк и ход в потоке.
const CHUNK: i64 = 20_000;
pub const DEFAULT_BY_VOTES: i64 = 40_000;
pub const DEFAULT_BY_POPULARITY: i64 = 40_000;
const MAX_ACTIVE: i64 = 1_000_000;

/// Блокировка долгих операций каталога. Поиск выключен — работаем без неё (только БД).
async fn lock(state: &AppState) -> AppResult<Option<ReindexLock>> {
    match ReindexLock::acquire(state).await {
        Ok(lock) => Ok(Some(lock)),
        Err(ReindexError::Disabled) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Перестройка поиска в конце операции; `false` — не удалась.
async fn reindex_after(
    state: &AppState,
    lock: Option<ReindexLock>,
    options: ReindexOptions,
    report: &Reporter,
) -> bool {
    let Some(lock) = lock else { return true };
    let result = reindex_locked(state, &lock, options, report).await;
    lock.release(state).await;
    match result {
        Ok(result) => {
            tracing::info!(
                indexed = result.indexed,
                "search index rebuilt after tmdb job"
            );
            true
        }
        Err(error) => {
            tracing::warn!(%error, "search reindex after tmdb job failed");
            false
        }
    }
}

// ---------------------------------------------------------------- popularity

/// Обновить `metadata.popularity` фильмов из ежедневного дампа TMDB (без ключа API), потоком
/// `text/event-stream`: `plan`, `step` по шагам `tmdb_export`, `popularity` (с `progress`), затем
/// шаги перестройки поиска, последним `done` (`popularity` — итог).
///
/// Меняется только ключ `popularity` (остальная `metadata` — как есть), только у фильмов со
/// связью в `entity_tmdb`. Уже идёт перестройка или другая такая операция — `409` до потока.
/// Доводится до конца, даже если клиент закрыл соединение.
#[utoipa::path(
    post, path = "/admin/tmdb/popularity/stream", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Поток событий", content_type = "text/event-stream",
            body = jobs::JobEvent),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт перестройка поиска или операция TMDB", body = ErrorBody),
    )
)]
pub async fn popularity_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<Response> {
    let lock = lock(&state).await?;
    let reindex = ReindexOptions::default();
    let mut steps = vec![Step::TmdbExport, Step::Popularity];
    if lock.is_some() {
        steps.extend(reindex.steps());
    }
    tracing::info!(admin_id = %admin.id, "tmdb popularity refresh started");
    Ok(jobs::stream(&steps, move |report| async move {
        let mut done = DoneEvent::default();
        match refresh_popularity(&state, &report).await {
            Ok(result) => {
                tracing::info!(updated = result.updated, "tmdb popularity refreshed");
                done.popularity = Some(result);
                done.ok = reindex_after(&state, lock, reindex, &report).await;
            }
            Err(error) => {
                tracing::warn!(%error, "tmdb popularity refresh failed");
                if let Some(lock) = lock {
                    lock.release(&state).await;
                }
            }
        }
        done
    }))
}

async fn refresh_popularity(
    state: &AppState,
    report: &Reporter,
) -> Result<PopularityResult, String> {
    let started = report.start(Step::TmdbExport);
    let (export, rows) = match download_export(&state.config.tmdb_exports_url).await {
        Ok(found) => found,
        Err(error) => {
            report.finish(Step::TmdbExport, StepStatus::Failed, error.clone(), started);
            return Err(error);
        }
    };
    report.finish(
        Step::TmdbExport,
        StepStatus::Done,
        format!("{export}: {} фильмов с popularity", rows.len()),
        started,
    );

    let started = report.start(Step::Popularity);
    match write_popularity(&state.db, &rows, report).await {
        Ok(updated) => {
            report.finish(
                Step::Popularity,
                StepStatus::Done,
                format!("popularity обновлена у {updated} фильмов"),
                started,
            );
            Ok(PopularityResult {
                export,
                films: rows.len() as u64,
                updated,
            })
        }
        Err(error) => {
            let error = error.to_string();
            report.finish(Step::Popularity, StepStatus::Failed, error.clone(), started);
            Err(error)
        }
    }
}

/// Ежедневный дамп `movie_ids_MM_DD_YYYY.json.gz`: (TMDB id, popularity до тысячных) всех
/// фильмов, кроме `video`.
pub(crate) async fn download_export(base: &str) -> Result<(String, Vec<(i32, f64)>), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let today = Utc::now().date_naive();
    for back in 0..EXPORT_DAYS {
        let day = today - Days::new(back);
        let name = format!("movie_ids_{}.json.gz", day.format("%m_%d_%Y"));
        let response = client
            .get(format!("{}/{name}", base.trim_end_matches('/')))
            .send()
            .await
            .map_err(|e| format!("{name}: {e}"))?;
        let status = response.status().as_u16();
        if status == 403 || status == 404 {
            continue; // ещё не опубликован
        }
        if !response.status().is_success() {
            return Err(format!("{name}: HTTP {status}"));
        }
        let gz = response.bytes().await.map_err(|e| format!("{name}: {e}"))?;
        let rows = tokio::task::spawn_blocking(move || parse_export(&gz))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{name}: {e}"))?;
        return Ok((name, rows));
    }
    Err(format!("нет дампа TMDB за последние {EXPORT_DAYS} дня"))
}

fn parse_export(gz: &[u8]) -> Result<Vec<(i32, f64)>, String> {
    #[derive(Deserialize)]
    struct Row {
        id: i64,
        popularity: Option<f64>,
        #[serde(default)]
        video: bool,
    }
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(gz));
    let mut rows = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let row: Row = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        let (Ok(id), Some(popularity)) = (i32::try_from(row.id), row.popularity) else {
            continue;
        };
        if !row.video && popularity.is_finite() && (0.0..=1_000_000.0).contains(&popularity) {
            rows.push((id, (popularity * 1000.0).round() / 1000.0));
        }
    }
    Ok(rows)
}

/// Временная таблица с дампом и `UPDATE` пачками по [`CHUNK`] связей `entity_tmdb` (keyset по
/// `entity_id`). Меняются только строки, где popularity другая.
async fn write_popularity(
    db: &PgPool,
    rows: &[(i32, f64)],
    report: &Reporter,
) -> Result<u64, sqlx::Error> {
    let mut conn = db.acquire().await?;
    sqlx::query(
        "CREATE TEMP TABLE IF NOT EXISTS tmdb_popularity (
             tmdb_id integer PRIMARY KEY, popularity float8 NOT NULL)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("TRUNCATE tmdb_popularity")
        .execute(&mut *conn)
        .await?;
    for chunk in rows.chunks(50_000) {
        let ids: Vec<i32> = chunk.iter().map(|row| row.0).collect();
        let values: Vec<f64> = chunk.iter().map(|row| row.1).collect();
        sqlx::query(
            "INSERT INTO tmdb_popularity SELECT * FROM unnest($1::int[], $2::float8[])
             ON CONFLICT DO NOTHING",
        )
        .bind(ids)
        .bind(values)
        .execute(&mut *conn)
        .await?;
    }

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM entity_tmdb")
        .fetch_one(&mut *conn)
        .await?;
    let total = u64::try_from(total).unwrap_or(0);
    report.progress(Step::Popularity, 0, total);
    let (mut after, mut done, mut updated): (Option<Uuid>, u64, u64) = (None, 0, 0);
    loop {
        let upto: Option<Uuid> = sqlx::query_scalar(
            "SELECT entity_id FROM entity_tmdb WHERE $1::uuid IS NULL OR entity_id > $1
             ORDER BY entity_id OFFSET $2 LIMIT 1",
        )
        .bind(after)
        .bind(CHUNK - 1)
        .fetch_optional(&mut *conn)
        .await?;
        let result = sqlx::query(
            "UPDATE entities e
             SET metadata = jsonb_set(e.metadata, '{popularity}', to_jsonb(n.popularity))
             FROM entity_tmdb t JOIN tmdb_popularity n ON n.tmdb_id = t.tmdb_id
             WHERE e.id = t.entity_id AND e.kind = 'movie'
               AND ($1::uuid IS NULL OR t.entity_id > $1)
               AND ($2::uuid IS NULL OR t.entity_id <= $2)
               AND e.metadata->'popularity' IS DISTINCT FROM to_jsonb(n.popularity)",
        )
        .bind(after)
        .bind(upto)
        .execute(&mut *conn)
        .await?;
        updated += result.rows_affected();
        done = (done + CHUNK as u64).min(total);
        report.progress(
            Step::Popularity,
            if upto.is_some() { done } else { total },
            total,
        );
        match upto {
            Some(id) => after = Some(id),
            None => break,
        }
    }
    sqlx::query("DROP TABLE tmdb_popularity")
        .execute(&mut *conn)
        .await?;
    Ok(updated)
}

// ---------------------------------------------------------------- активные

/// Пересобрать активные: из сущностей со связью TMDB, кроме переключённых вручную
/// (`active_manual`), активны топ `by_votes` по `metadata.vote_count` и ещё `by_popularity` самых
/// популярных (`metadata.popularity`) из остальных, без повторов; остальные скрыты. Потоком: шаг
/// `activate`, затем перестройка поиска (без проверки размера — состав меняется намеренно) и
/// пересборка статики, последним `done` (`active` — итог).
///
/// Включённые и скрытые в админке (`PATCH active`), сиды и сущности без TMDB не трогаются.
#[utoipa::path(
    post, path = "/admin/tmdb/active/stream", tag = "catalog-admin",
    security(("bearer" = [])),
    params(ActiveQuery),
    responses(
        (status = 200, description = "Поток событий", content_type = "text/event-stream",
            body = jobs::JobEvent),
        (status = 400, description = "Неверные числа", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт перестройка поиска или операция TMDB", body = ErrorBody),
    )
)]
pub async fn active_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Query(query): Query<ActiveQuery>,
) -> AppResult<Response> {
    let by_votes = query.by_votes.unwrap_or(DEFAULT_BY_VOTES);
    let by_popularity = query.by_popularity.unwrap_or(DEFAULT_BY_POPULARITY);
    for (name, value) in [("by_votes", by_votes), ("by_popularity", by_popularity)] {
        if !(0..=MAX_ACTIVE).contains(&value) {
            return Err(AppError::BadRequest(format!(
                "{name} must be between 0 and {MAX_ACTIVE}"
            )));
        }
    }
    let lock = lock(&state).await?;
    let reindex = ReindexOptions {
        force: true,
        revalidate: true,
    };
    let mut steps = vec![Step::Activate];
    if lock.is_some() {
        steps.extend(reindex.steps());
    }
    tracing::info!(admin_id = %admin.id, by_votes, by_popularity, "active rebuild started");
    Ok(jobs::stream(&steps, move |report| async move {
        let mut done = DoneEvent::default();
        let started = report.start(Step::Activate);
        match rebuild_active(&state.db, by_votes, by_popularity).await {
            Ok(result) => {
                report.finish(
                    Step::Activate,
                    StepStatus::Done,
                    format!(
                        "Включено {}, скрыто {}, активных теперь {}",
                        result.activated, result.deactivated, result.active
                    ),
                    started,
                );
                tracing::info!(?result, "active rebuilt");
                done.active = Some(result);
                done.ok = reindex_after(&state, lock, reindex, &report).await;
            }
            Err(error) => {
                tracing::warn!(%error, "active rebuild failed");
                report.finish(
                    Step::Activate,
                    StepStatus::Failed,
                    error.to_string(),
                    started,
                );
                if let Some(lock) = lock {
                    lock.release(&state).await;
                }
            }
        }
        done
    }))
}

/// Один `UPDATE`: счётчики людей и тегов сдвигает операторный триггер
/// `entities_active_changed` одним запросом на весь набор.
pub(crate) async fn rebuild_active(
    db: &PgPool,
    by_votes: i64,
    by_popularity: i64,
) -> Result<ActiveResult, sqlx::Error> {
    let mut tx = db.begin().await?;
    let (activated, deactivated): (i64, i64) = sqlx::query_as(
        "WITH cand AS (
             SELECT e.id, (e.metadata->>'vote_count')::int AS votes,
                    (e.metadata->>'popularity')::float8 AS pop
             FROM entities e JOIN entity_tmdb t ON t.entity_id = e.id
             WHERE NOT e.active_manual
         ),
         by_votes AS (
             SELECT id FROM cand ORDER BY votes DESC NULLS LAST, pop DESC NULLS LAST, id LIMIT $1
         ),
         by_pop AS (
             SELECT c.id FROM cand c
             WHERE NOT EXISTS (SELECT 1 FROM by_votes v WHERE v.id = c.id)
             ORDER BY c.pop DESC NULLS LAST, c.votes DESC NULLS LAST, c.id LIMIT $2
         ),
         want AS (SELECT id FROM by_votes UNION ALL SELECT id FROM by_pop),
         changed AS (
             UPDATE entities e SET active = (w.id IS NOT NULL)
             FROM cand c LEFT JOIN want w ON w.id = c.id
             WHERE e.id = c.id AND e.active <> (w.id IS NOT NULL)
             RETURNING e.active
         )
         SELECT count(*) FILTER (WHERE active), count(*) FILTER (WHERE NOT active) FROM changed",
    )
    .bind(by_votes)
    .bind(by_popularity)
    .fetch_one(&mut *tx)
    .await?;
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM entities WHERE active")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ActiveResult {
        activated,
        deactivated,
        active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn export_lines_without_video_rounded() {
        let lines = concat!(
            r#"{"adult":false,"id":550,"original_title":"Fight Club","popularity":61.40456,"video":false}"#,
            "\n",
            r#"{"adult":false,"id":7,"original_title":"Clip","popularity":3.0,"video":true}"#,
            "\n",
            r#"{"adult":false,"id":8,"original_title":"No popularity","video":false}"#,
            "\n\n"
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(lines.as_bytes()).unwrap();
        let rows = parse_export(&gz.finish().unwrap()).unwrap();
        assert_eq!(rows, [(550, 61.405)]);
    }
}
