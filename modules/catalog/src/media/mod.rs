//! Постеры и трейлеры для каталога: `nexus media fill | check` (в dev — `make media`).
//!
//! - **fill** — заполняет только пустое: постер, если нет `cover_url`; трейлер YouTube и запасной
//!   Rutube, если такого источника нет в `metadata.trailers`.
//! - **check** — проверяет, живы ли текущие ссылки; точно мёртвые удаляет и подбирает замену
//!   (как fill). Ошибки сети, 5xx и 403 — «неизвестно»: такие ссылки не трогаем, иначе один сбой
//!   связи стёр бы все постеры.
//!
//! Подобранное вручную (как у «Дюны» в сидах) не перезаписывается, пока живо. Запись — с той же
//! проверкой `metadata`, что у админки, затем обновление поискового индекса.

pub mod pick;
mod providers;

pub use providers::{HttpFinder, Keys};

use crate::metadata::{self, TrailerProvider, TrailerSource};
use crate::models::EntityKind;
use crate::search;
use serde_json::Value;
use shared::AppState;
use std::fmt::{self, Write as _};
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fill,
    Check,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub mode: Mode,
    /// Только показать, что изменилось бы.
    pub dry_run: bool,
    /// Только эта сущность.
    pub slug: Option<String>,
    /// Не больше стольких сущностей за запуск.
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    /// Сеть, 5xx, 403: не знаем — ничего не делаем.
    Unknown,
}

/// Сущность, для которой ищем медиа.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: Uuid,
    pub kind: EntityKind,
    pub slug: String,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<i32>,
    pub isbn: Option<String>,
    /// Авторы книги (для поиска обложки).
    pub authors: Vec<String>,
    /// В каталоге есть одноимённое произведение того же типа (ремейк, экранизация с тем же
    /// названием): различаем только по году.
    pub has_namesake: bool,
}

impl Target {
    /// Названия для поиска: сначала оригинальное (зарубежные базы ищут по нему), затем русское.
    pub fn titles(&self) -> impl Iterator<Item = &str> {
        self.original_title
            .as_deref()
            .into_iter()
            .chain(std::iter::once(self.title.as_str()))
            .filter(|t| !t.trim().is_empty())
    }
}

/// Источники медиа. В бою — [`HttpFinder`], в тестах — подделка без сети. Найденное уже проверено:
/// возвращать только живые ссылки.
#[allow(async_fn_in_trait)]
pub trait Finder {
    async fn cover(&self, target: &Target) -> Option<String>;
    /// id видео YouTube с канала правообладателя.
    async fn youtube_trailer(&self, target: &Target) -> Option<String>;
    /// id видео Rutube — запасной источник, где YouTube заблокирован.
    async fn rutube_trailer(&self, target: &Target) -> Option<String>;
    async fn image_alive(&self, url: &str) -> Liveness;
    async fn video_alive(&self, source: &TrailerSource) -> Liveness;
}

/// Что произошло с одной сущностью.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Change {
    pub slug: String,
    /// «+ постер», «− трейлер youtube (мёртв)», ...
    pub actions: Vec<String>,
    /// Не нашлось: «постер», «трейлер youtube».
    pub missing: Vec<String>,
    /// Найдено автоматически на Rutube — стоит посмотреть глазами.
    pub review: Option<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub mode: Option<Mode>,
    pub dry_run: bool,
    pub checked: usize,
    pub changes: Vec<Change>,
    /// Предупреждения: каких ключей нет и что поэтому не искалось.
    pub notes: Vec<String>,
}

impl Report {
    pub fn updated(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| !c.actions.is_empty())
            .count()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            Some(Mode::Check) => "check",
            _ => "fill",
        };
        let dry = if self.dry_run {
            " (dry run — ничего не записано)"
        } else {
            ""
        };
        writeln!(
            f,
            "media {mode}{dry}: проверено {}, изменено {}",
            self.checked,
            self.updated()
        )?;
        for note in &self.notes {
            writeln!(f, "  ⚠ {note}")?;
        }
        for change in &self.changes {
            let mut line = String::new();
            if !change.actions.is_empty() {
                let _ = write!(line, " {}", change.actions.join(", "));
            }
            if !change.missing.is_empty() {
                let _ = write!(line, " · не найдено: {}", change.missing.join(", "));
            }
            if line.is_empty() {
                continue;
            }
            writeln!(f, "  {}:{line}", change.slug)?;
        }
        let review: Vec<_> = self
            .changes
            .iter()
            .filter_map(|c| c.review.as_ref())
            .collect();
        if !review.is_empty() {
            writeln!(f, "Проверить глазами (Rutube, сторонние каналы):")?;
            for url in review {
                writeln!(f, "  {url}")?;
            }
        }
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    kind: EntityKind,
    slug: String,
    title: String,
    original_title: Option<String>,
    year: Option<i32>,
    cover_url: Option<String>,
    metadata: Value,
    authors: Vec<String>,
    has_namesake: bool,
}

/// Медиа по всем сущностям (или по `slug`) с настоящими источниками.
pub async fn run(state: &AppState, options: Options) -> Result<Report, BoxError> {
    let keys = Keys::from_env();
    let mut notes = Vec::new();
    if keys.tmdb.is_none() {
        notes.push(
            "TMDB_API_KEY не задан: постеры и YouTube-трейлеры фильмов и сериалов не ищутся".into(),
        );
    }
    if !keys.has_igdb() {
        notes.push(
            "TWITCH_CLIENT_ID / TWITCH_CLIENT_SECRET не заданы: обложки и YouTube-трейлеры игр (IGDB) не ищутся"
                .into(),
        );
    }
    let mut report = run_with(state, &HttpFinder::new(keys), options).await?;
    report.notes = notes;
    Ok(report)
}

pub async fn run_with<F: Finder>(
    state: &AppState,
    finder: &F,
    options: Options,
) -> Result<Report, BoxError> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT e.id, e.kind, e.slug, e.title, e.original_title,
                EXTRACT(YEAR FROM e.release_date)::int AS year, e.cover_url, e.metadata,
                COALESCE(ARRAY(
                    SELECT p.full_name FROM entity_credits c JOIN people p ON p.id = c.person_id
                    WHERE c.entity_id = e.id AND c.role = 'author' ORDER BY c.position
                ), '{}') AS authors,
                EXISTS(
                    SELECT 1 FROM entities o
                    WHERE o.kind = e.kind AND o.id <> e.id
                      AND (lower(o.title) = lower(e.title)
                           OR lower(o.original_title) = lower(e.original_title))
                ) AS has_namesake
         FROM entities e
         WHERE ($1::text IS NULL OR e.slug = $1)
         ORDER BY e.created_at, e.id
         LIMIT $2",
    )
    .bind(&options.slug)
    .bind(options.limit.unwrap_or(i64::MAX))
    .fetch_all(&state.db)
    .await?;

    let mut report = Report {
        mode: Some(options.mode),
        dry_run: options.dry_run,
        ..Report::default()
    };
    let mut touched = Vec::new();
    for row in rows {
        report.checked += 1;
        let (change, update) = plan(finder, &row, options.mode).await;
        if let Some((cover_url, metadata)) = update {
            if !options.dry_run {
                let metadata = metadata::validate(row.kind, metadata)
                    .map_err(|e| format!("{}: {e}", row.slug))?;
                sqlx::query("UPDATE entities SET cover_url = $2, metadata = $3 WHERE id = $1")
                    .bind(row.id)
                    .bind(&cover_url)
                    .bind(&metadata)
                    .execute(&state.db)
                    .await?;
                touched.push(row.id);
            }
        }
        report.changes.push(change);
    }
    search::sync(state, &touched).await;
    Ok(report)
}

/// Решение по одной сущности: что поменять (новые `cover_url` и `metadata`) и что написать в отчёт.
async fn plan<F: Finder>(
    finder: &F,
    row: &Row,
    mode: Mode,
) -> (Change, Option<(Option<String>, Value)>) {
    let target = Target {
        id: row.id,
        kind: row.kind,
        slug: row.slug.clone(),
        title: row.title.clone(),
        original_title: row.original_title.clone(),
        year: row.year,
        isbn: row
            .metadata
            .get("isbn")
            .and_then(Value::as_str)
            .map(str::to_string),
        authors: row.authors.clone(),
        has_namesake: row.has_namesake,
    };
    let mut change = Change {
        slug: row.slug.clone(),
        ..Change::default()
    };
    let mut cover = row.cover_url.clone();
    let has_trailers = row.kind != EntityKind::Book;
    let mut trailers: Vec<TrailerSource> = row
        .metadata
        .get("trailers")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .unwrap_or_default();
    let original_trailers = trailers.clone();

    if mode == Mode::Check {
        if let Some(url) = &cover {
            if finder.image_alive(url).await == Liveness::Dead {
                change.actions.push("− постер (мёртв)".into());
                cover = None;
            }
        }
        let mut alive = Vec::with_capacity(trailers.len());
        for source in trailers {
            if finder.video_alive(&source).await == Liveness::Dead {
                change.actions.push(format!(
                    "− трейлер {} (мёртв)",
                    provider_name(source.provider)
                ));
            } else {
                alive.push(source);
            }
        }
        trailers = alive;
    }

    if cover.is_none() {
        match finder.cover(&target).await {
            Some(url) => {
                change.actions.push("+ постер".into());
                cover = Some(url);
            }
            None => change.missing.push("постер".into()),
        }
    }

    if has_trailers {
        let has = |list: &[TrailerSource], p| list.iter().any(|s| s.provider == p);
        if !has(&trailers, TrailerProvider::Youtube) {
            match finder.youtube_trailer(&target).await {
                // YouTube — с правообладателя, поэтому первым.
                Some(id) => {
                    change.actions.push("+ трейлер youtube".into());
                    trailers.insert(
                        0,
                        TrailerSource {
                            provider: TrailerProvider::Youtube,
                            id,
                        },
                    );
                }
                None => change.missing.push("трейлер youtube".into()),
            }
        }
        if !has(&trailers, TrailerProvider::Rutube) {
            match finder.rutube_trailer(&target).await {
                Some(id) => {
                    change.actions.push("+ трейлер rutube".into());
                    change.review = Some(format!("{}: https://rutube.ru/video/{id}/", row.slug));
                    trailers.push(TrailerSource {
                        provider: TrailerProvider::Rutube,
                        id,
                    });
                }
                None => change.missing.push("трейлер rutube".into()),
            }
        }
    }

    if cover == row.cover_url && trailers == original_trailers {
        return (change, None);
    }
    let mut metadata = row.metadata.clone();
    if let Some(object) = metadata.as_object_mut() {
        // Поле прежнего формата (один id YouTube) — убираем.
        object.remove("trailer");
        if trailers.is_empty() {
            object.remove("trailers");
        } else {
            object.insert(
                "trailers".into(),
                serde_json::to_value(&trailers).unwrap_or(Value::Null),
            );
        }
    }
    (change, Some((cover, metadata)))
}

fn provider_name(provider: TrailerProvider) -> &'static str {
    match provider {
        TrailerProvider::Youtube => "youtube",
        TrailerProvider::Rutube => "rutube",
    }
}
