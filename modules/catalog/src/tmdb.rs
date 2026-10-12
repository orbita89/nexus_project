//! Операции админки с внешними данными: «Обновить рейтинги» (TMDB popularity из ежедневного дампа
//! и рейтинги IMDb → `entity_stats`, пересчёт балла «Рекомендуемые») и «Пересобрать активные» (топ
//! вышедших по баллу + все «Скоро премьера»).
//!
//! Обе долгие, запускаются вручную (расписания нет), ход — потоком шагов ([`crate::jobs`]), как у
//! перестройки поиска. Одна за раз: общая блокировка с перестройкой ([`ReindexLock`]), занято —
//! `409` до потока. В конце — перестройка поиска (если Meilisearch включён).
//!
//! Связь с TMDB — `entity_tmdb`, `imdb_id` — `entity_stats` (пишет импорт
//! `scripts/catalog-import/merge.py`).

use crate::jobs::{self, DoneEvent, Reporter, Step, StepStatus};
use crate::models::{ActiveQuery, ActiveResult, StatsResult};
use crate::ranking;
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

/// Дамп TMDB публикуется около 08:00 UTC: сегодняшнего нет — берётся вчерашний и так далее.
const EXPORT_DAYS: u64 = 4;
/// Строк на один INSERT во временную таблицу.
const INSERT_CHUNK: usize = 50_000;
pub const DEFAULT_ACTIVE: i64 = 80_000;
const MAX_ACTIVE: i64 = 1_000_000;

/// Блокировка долгих операций каталога. Поиск выключен — работаем без неё (только БД).
pub(crate) async fn lock(state: &AppState) -> AppResult<Option<ReindexLock>> {
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
                "search index rebuilt after catalog job"
            );
            true
        }
        Err(error) => {
            tracing::warn!(%error, "search reindex after catalog job failed");
            false
        }
    }
}

async fn release(state: &AppState, lock: Option<ReindexLock>) {
    if let Some(lock) = lock {
        lock.release(state).await;
    }
}

// ---------------------------------------------------------------- рейтинги

/// «Обновить рейтинги», потоком `text/event-stream`: шаги `tmdb_export` (ежедневный дамп TMDB,
/// без ключа), `imdb_ratings` (`title.ratings.tsv.gz`), `stats` (запись в `entity_stats`, с
/// `progress`), `rank` (пересчёт балла «Рекомендуемые»), затем перестройка поиска; последним
/// `done` (`stats` — итог). Уже идёт перестройка или другая операция каталога — `409` до потока.
/// Доводится до конца, даже если клиент закрыл соединение.
#[utoipa::path(
    post, path = "/admin/tmdb/stats/stream", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Поток событий", content_type = "text/event-stream",
            body = jobs::JobEvent),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт перестройка поиска или операция каталога", body = ErrorBody),
    )
)]
pub async fn stats_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> AppResult<Response> {
    let lock = lock(&state).await?;
    let reindex = ReindexOptions::default();
    let mut steps = vec![Step::TmdbExport, Step::ImdbRatings, Step::Stats, Step::Rank];
    if lock.is_some() {
        steps.extend(reindex.steps());
    }
    tracing::info!(admin_id = %admin.id, "stats refresh started");
    Ok(jobs::stream(&steps, move |report| async move {
        let mut done = DoneEvent::default();
        match refresh_stats(&state, &report).await {
            Ok(result) => {
                tracing::info!(?result, "stats refreshed");
                done.stats = Some(result);
                done.ok = reindex_after(&state, lock, reindex, &report).await;
            }
            Err(error) => {
                tracing::warn!(%error, "stats refresh failed");
                release(&state, lock).await;
            }
        }
        done
    }))
}

/// Шаг: `run` → итог или ошибка в лог.
async fn step<T>(
    report: &Reporter,
    step: Step,
    run: impl std::future::Future<Output = Result<(T, String), String>>,
) -> Result<T, String> {
    let started = report.start(step);
    match run.await {
        Ok((value, message)) => {
            report.finish(step, StepStatus::Done, message, started);
            Ok(value)
        }
        Err(error) => {
            report.finish(step, StepStatus::Failed, error.clone(), started);
            Err(error)
        }
    }
}

async fn refresh_stats(state: &AppState, report: &Reporter) -> Result<StatsResult, String> {
    let (export, popularity) = step(report, Step::TmdbExport, async {
        let (name, rows) = download_export(&state.config.tmdb_exports_url).await?;
        let message = format!("{name}: {} фильмов с popularity", rows.len());
        Ok(((name, rows), message))
    })
    .await?;
    let ratings = step(report, Step::ImdbRatings, async {
        let rows = download_imdb(&state.config.imdb_datasets_url).await?;
        let message = format!("title.ratings.tsv.gz: {} записей", rows.len());
        Ok((rows, message))
    })
    .await?;
    let (tmdb_updated, imdb_updated) = step(report, Step::Stats, async {
        let counts = write_stats(&state.db, &popularity, &ratings, report)
            .await
            .map_err(|e| e.to_string())?;
        let message = format!(
            "TMDB popularity изменилась у {}, рейтинг IMDb — у {}",
            counts.0, counts.1
        );
        Ok((counts, message))
    })
    .await?;
    let ranked = step(report, Step::Rank, async {
        let ranked = ranking::recompute(&state.db)
            .await
            .map_err(|e| e.to_string())?;
        Ok((ranked, format!("Балл «Рекомендуемые» у {ranked} фильмов")))
    })
    .await?;
    Ok(StatsResult {
        tmdb_export: export,
        tmdb_updated,
        imdb_updated,
        ranked,
    })
}

/// Ошибка с причинами: у reqwest верхний уровень — только «error sending request».
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

async fn get_bytes(url: &str) -> Result<Option<Vec<u8>>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {}", chain(&e)))?;
    let status = response.status().as_u16();
    if status == 403 || status == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("{url}: {}", chain(&e)))?;
    Ok(Some(body.to_vec()))
}

/// Ежедневный дамп `movie_ids_MM_DD_YYYY.json.gz`: (TMDB id, popularity до тысячных) всех
/// фильмов, кроме `video`.
pub(crate) async fn download_export(base: &str) -> Result<(String, Vec<(i32, f64)>), String> {
    let today = Utc::now().date_naive();
    for back in 0..EXPORT_DAYS {
        let day = today - Days::new(back);
        let name = format!("movie_ids_{}.json.gz", day.format("%m_%d_%Y"));
        let Some(gz) = get_bytes(&format!("{base}/{name}")).await? else {
            continue; // ещё не опубликован
        };
        let rows = tokio::task::spawn_blocking(move || parse_export(&gz))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{name}: {e}"))?;
        return Ok((name, rows));
    }
    Err(format!("нет дампа TMDB за последние {EXPORT_DAYS} дня"))
}

fn parse_export(gz: &[u8]) -> Result<Vec<(i32, f64)>, String> {
    // В дампе бывают `"video": null` и прочие пропуски: всё, кроме id, необязательно.
    #[derive(Deserialize)]
    struct Row {
        id: i64,
        popularity: Option<f64>,
        video: Option<bool>,
    }
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(gz));
    let (mut rows, mut bad) = (Vec::new(), 0usize);
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        // Битая строка — пропустить, а не терять весь дамп.
        let Ok(row) = serde_json::from_str::<Row>(&line) else {
            bad += 1;
            continue;
        };
        let (Ok(id), Some(popularity)) = (i32::try_from(row.id), row.popularity) else {
            continue;
        };
        if row.video != Some(true)
            && popularity.is_finite()
            && (0.0..=1_000_000.0).contains(&popularity)
        {
            rows.push((id, (popularity * 1000.0).round() / 1000.0));
        }
    }
    // Битых больше, чем годных, — формат сменился: лучше ошибка, чем пустая popularity.
    if bad > rows.len() {
        return Err(format!("{bad} unreadable lines, {} usable", rows.len()));
    }
    if bad > 0 {
        tracing::warn!(bad, "tmdb export: unreadable lines skipped");
    }
    Ok(rows)
}

/// IMDb `title.ratings.tsv.gz`: `tconst`, `averageRating`, `numVotes` (все типы записей; лишние
/// не совпадут ни с одним `imdb_id`).
pub(crate) async fn download_imdb(base: &str) -> Result<Vec<(String, f64, i32)>, String> {
    let url = format!("{base}/title.ratings.tsv.gz");
    let gz = get_bytes(&url)
        .await?
        .ok_or_else(|| format!("{url}: не найден"))?;
    tokio::task::spawn_blocking(move || parse_imdb(&gz))
        .await
        .map_err(|e| e.to_string())?
}

fn parse_imdb(gz: &[u8]) -> Result<Vec<(String, f64, i32)>, String> {
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(gz));
    let mut rows = Vec::new();
    for line in reader.lines().skip(1) {
        let line = line.map_err(|e| e.to_string())?;
        let mut cols = line.split('\t');
        let (Some(id), Some(rating), Some(votes)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        let (Ok(rating), Ok(votes)) = (rating.parse::<f64>(), votes.trim().parse::<i32>()) else {
            continue;
        };
        if id.starts_with("tt") && (0.0..=10.0).contains(&rating) && votes >= 0 {
            rows.push((id.to_string(), rating, votes));
        }
    }
    Ok(rows)
}

/// Дампы во временные таблицы и два `UPDATE entity_stats` (только где значение другое). Строки
/// статистики заводятся для всех связей TMDB. → (TMDB изменено, IMDb изменено).
async fn write_stats(
    db: &PgPool,
    popularity: &[(i32, f64)],
    ratings: &[(String, f64, i32)],
    report: &Reporter,
) -> Result<(u64, u64), sqlx::Error> {
    let mut conn = db.acquire().await?;
    report.progress(Step::Stats, 0, 3);
    sqlx::query(
        "INSERT INTO entity_stats (entity_id) SELECT entity_id FROM entity_tmdb
         ON CONFLICT DO NOTHING",
    )
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "CREATE TEMP TABLE IF NOT EXISTS tmdb_popularity (
             tmdb_id integer PRIMARY KEY, popularity float8 NOT NULL)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("TRUNCATE tmdb_popularity")
        .execute(&mut *conn)
        .await?;
    for chunk in popularity.chunks(INSERT_CHUNK) {
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
    let tmdb = sqlx::query(
        "UPDATE entity_stats s SET tmdb_popularity = n.popularity
         FROM entity_tmdb t JOIN tmdb_popularity n ON n.tmdb_id = t.tmdb_id
         WHERE s.entity_id = t.entity_id AND s.tmdb_popularity IS DISTINCT FROM n.popularity",
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    report.progress(Step::Stats, 1, 3);

    sqlx::query(
        "CREATE TEMP TABLE IF NOT EXISTS imdb_ratings (
             imdb_id text PRIMARY KEY, rating float8 NOT NULL, votes integer NOT NULL)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("TRUNCATE imdb_ratings")
        .execute(&mut *conn)
        .await?;
    for chunk in ratings.chunks(INSERT_CHUNK) {
        let ids: Vec<&str> = chunk.iter().map(|row| row.0.as_str()).collect();
        let values: Vec<f64> = chunk.iter().map(|row| row.1).collect();
        let votes: Vec<i32> = chunk.iter().map(|row| row.2).collect();
        sqlx::query(
            "INSERT INTO imdb_ratings
             SELECT * FROM unnest($1::text[], $2::float8[], $3::int[]) ON CONFLICT DO NOTHING",
        )
        .bind(ids)
        .bind(values)
        .bind(votes)
        .execute(&mut *conn)
        .await?;
    }
    report.progress(Step::Stats, 2, 3);
    let imdb = sqlx::query(
        "UPDATE entity_stats s SET imdb_rating = r.rating, imdb_votes = r.votes
         FROM imdb_ratings r
         WHERE s.imdb_id = r.imdb_id
           AND (s.imdb_rating, s.imdb_votes) IS DISTINCT FROM (r.rating, r.votes)",
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query("DROP TABLE tmdb_popularity, imdb_ratings")
        .execute(&mut *conn)
        .await?;
    report.progress(Step::Stats, 3, 3);
    Ok((tmdb, imdb))
}

// ---------------------------------------------------------------- активные

/// «Пересобрать активные»: из сущностей со связью TMDB, кроме переключённых вручную
/// (`active_manual`), активны топ `count` **вышедших** по баллу «Рекомендуемые» (без балла — по
/// числу оценок TMDB и popularity) и все «Скоро премьера» (выход в ближайшие `soon_days` дней);
/// остальные — скрыты, в том числе невышедшие позже и без даты. Потоком: шаг `activate`, затем
/// перестройка поиска без проверки размера и ISR `{"all": true}`; последним `done` (`active`).
///
/// Сперва «Обновить рейтинги»: топ берётся по текущему баллу. Включённые и скрытые в админке, сиды и
/// сущности без TMDB не трогаются.
#[utoipa::path(
    post, path = "/admin/tmdb/active/stream", tag = "catalog-admin",
    security(("bearer" = [])),
    params(ActiveQuery),
    responses(
        (status = 200, description = "Поток событий", content_type = "text/event-stream",
            body = jobs::JobEvent),
        (status = 400, description = "Неверное число", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Уже идёт перестройка поиска или операция каталога", body = ErrorBody),
    )
)]
pub async fn active_stream(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Query(query): Query<ActiveQuery>,
) -> AppResult<Response> {
    let count = query.count.unwrap_or(DEFAULT_ACTIVE);
    if !(0..=MAX_ACTIVE).contains(&count) {
        return Err(AppError::BadRequest(format!(
            "count must be between 0 and {MAX_ACTIVE}"
        )));
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
    tracing::info!(admin_id = %admin.id, count, "active rebuild started");
    Ok(jobs::stream(&steps, move |report| async move {
        let mut done = DoneEvent::default();
        let result = step(&report, Step::Activate, async {
            let result = rebuild_active(&state.db, count)
                .await
                .map_err(|e| e.to_string())?;
            let message = format!(
                "Включено {}, скрыто {}, «скоро премьера» {}, активных теперь {}",
                result.activated, result.deactivated, result.soon, result.active
            );
            Ok((result, message))
        })
        .await;
        match result {
            Ok(result) => {
                tracing::info!(?result, "active rebuilt");
                done.active = Some(result);
                done.ok = reindex_after(&state, lock, reindex, &report).await;
            }
            Err(error) => {
                tracing::warn!(%error, "active rebuild failed");
                release(&state, lock).await;
            }
        }
        done
    }))
}

/// Один `UPDATE` разницы; счётчики людей и тегов сдвигает операторный триггер
/// `entities_active_changed`.
pub(crate) async fn rebuild_active(db: &PgPool, count: i64) -> Result<ActiveResult, sqlx::Error> {
    let mut tx = db.begin().await?;
    let (activated, deactivated, soon): (i64, i64, i64) = sqlx::query_as(
        "WITH cand AS (
             SELECT e.id, e.release_date, st.rank_score, st.tmdb_votes, st.tmdb_popularity
             FROM entities e
             JOIN entity_tmdb t ON t.entity_id = e.id
             LEFT JOIN entity_stats st ON st.entity_id = e.id
             WHERE NOT e.active_manual
         ),
         top AS (
             SELECT id FROM cand WHERE release_date <= current_date
             ORDER BY rank_score DESC NULLS LAST, tmdb_votes DESC NULLS LAST,
                      tmdb_popularity DESC NULLS LAST, id
             LIMIT $1
         ),
         soon AS (
             SELECT id FROM cand
             WHERE release_date > current_date
               AND release_date <= current_date + (SELECT soon_days FROM catalog_ranking)
         ),
         want AS (SELECT id FROM top UNION SELECT id FROM soon),
         changed AS (
             UPDATE entities e SET active = (w.id IS NOT NULL)
             FROM cand c LEFT JOIN want w ON w.id = c.id
             WHERE e.id = c.id AND e.active <> (w.id IS NOT NULL)
             RETURNING e.active
         )
         SELECT count(*) FILTER (WHERE active), count(*) FILTER (WHERE NOT active),
                (SELECT count(*) FROM soon)
         FROM changed",
    )
    .bind(count)
    .fetch_one(&mut *tx)
    .await?;
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM entities WHERE active")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ActiveResult {
        activated,
        deactivated,
        soon,
        active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gz(text: &str) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(text.as_bytes()).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn export_lines_without_video_rounded() {
        let lines = concat!(
            r#"{"adult":false,"id":550,"original_title":"Fight Club","popularity":61.40456,"video":false}"#,
            "\n",
            r#"{"adult":false,"id":7,"original_title":"Clip","popularity":3.0,"video":true}"#,
            "\n",
            r#"{"adult":false,"id":8,"original_title":"No popularity","video":false}"#,
            "\n",
            r#"{"adult":false,"id":9,"original_title":"Null video","popularity":1.5,"video":null}"#,
            "\n",
            "not json\n\n"
        );
        assert_eq!(parse_export(&gz(lines)).unwrap(), [(550, 61.405), (9, 1.5)]);
        assert!(parse_export(&gz("garbage\nmore garbage\n")).is_err());
    }

    #[test]
    fn imdb_ratings_tsv() {
        let tsv =
            "tconst\taverageRating\tnumVotes\ntt0111161\t9.3\t3100000\nbad\nnm1\t5\t1\ntt1\tx\t2\n";
        assert_eq!(
            parse_imdb(&gz(tsv)).unwrap(),
            [("tt0111161".to_string(), 9.3, 3_100_000)]
        );
    }
}
