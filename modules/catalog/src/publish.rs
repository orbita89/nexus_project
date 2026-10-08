//! Публикация правки каталога после коммита в БД: поисковый индекс и статика фронтенда (ISR).
//!
//! Шаги идут по порядку: [`Step::Search`] — документы в Meilisearch (с ожиданием применения),
//! затем [`Step::Isr`] — пересборка страниц затронутых сущностей и людей (`/films/dune-2021`,
//! `/people/denis-villeneuve`), старых и новых адресов. Ошибка шага запись не отменяет и следующий
//! шаг не останавливает: индекс догонит перестройка, статику — следующая правка.
//!
//! Обычные админские эндпоинты вызывают [`run`] молча, `.../stream` — с потоком ([`crate::jobs`]).

use crate::entities::detail_by_id;
use crate::jobs::{self, DoneEvent, Reporter, Step, StepStatus};
use crate::models::EntityKind;
use crate::search;
use axum::response::Response;
use shared::AppState;
use std::time::Instant;
use uuid::Uuid;

/// Что затронула запись в каталоге: какие документы переотправить и какие страницы пересобрать.
///
/// Связи раскрываются сами: у сущности — её участники (их страницы показывают сущность),
/// у человека — его работы (их страницы и документы показывают человека). Собирать **до**
/// записи, если запись удаляет или меняет slug: старые адреса запоминаются, чтобы пересобрать и их.
#[derive(Debug, Default)]
pub struct Touched {
    pub(crate) entities: Vec<Uuid>,
    pub(crate) people: Vec<Uuid>,
    /// Адреса на момент сбора.
    before: Vec<String>,
}

impl Touched {
    pub async fn collect(
        db: &sqlx::PgPool,
        entities: &[Uuid],
        people: &[Uuid],
    ) -> Result<Self, sqlx::Error> {
        let people_of: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT person_id FROM entity_credits WHERE entity_id = ANY($1)",
        )
        .bind(entities)
        .fetch_all(db)
        .await?;
        let entities_of: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT entity_id FROM entity_credits WHERE person_id = ANY($1)",
        )
        .bind(people)
        .fetch_all(db)
        .await?;
        let mut touched = Self {
            entities: merge(entities, entities_of),
            people: merge(people, people_of),
            before: Vec::new(),
        };
        touched.before = touched.paths(db).await?;
        Ok(touched)
    }

    /// Новая сущность (до неё ничего не было ни в индексе, ни в статике).
    pub fn entity(id: Uuid) -> Self {
        Self {
            entities: vec![id],
            ..Self::default()
        }
    }

    /// Новый человек.
    pub fn person(id: Uuid) -> Self {
        Self {
            people: vec![id],
            ..Self::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.people.is_empty()
    }

    /// Страницы фронтенда по slug'ам, которые сейчас в БД.
    async fn paths(&self, db: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
        let entities: Vec<(EntityKind, String)> =
            sqlx::query_as("SELECT kind, slug FROM entities WHERE id = ANY($1)")
                .bind(&self.entities)
                .fetch_all(db)
                .await?;
        let people: Vec<String> = sqlx::query_scalar("SELECT slug FROM people WHERE id = ANY($1)")
            .bind(&self.people)
            .fetch_all(db)
            .await?;
        Ok(entities
            .iter()
            .map(|(kind, slug)| entity_path(*kind, slug))
            .chain(people.iter().map(|slug| person_path(slug)))
            .collect())
    }
}

/// Страница сущности на фронтенде: `/films/dune-2021`.
pub fn entity_path(kind: EntityKind, slug: &str) -> String {
    format!("/{}/{slug}", kind.url_segment())
}

/// Страница человека на фронтенде.
pub fn person_path(slug: &str) -> String {
    format!("/people/{slug}")
}

fn merge(a: &[Uuid], b: Vec<Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = a.iter().copied().chain(b).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Пути для лога: несколько — списком, много (переименован тег) — числом.
fn list(paths: &[String]) -> String {
    if paths.len() <= 5 {
        paths.join(", ")
    } else {
        format!("{} страниц", paths.len())
    }
}

/// Публикует запись: шаги [`Step::Search`] и [`Step::Isr`]. Вызывать после коммита.
/// `true` — ни один шаг не упал (пропущенные не считаются).
pub async fn run(state: &AppState, touched: Touched, report: &Reporter) -> bool {
    if touched.is_empty() {
        return true;
    }
    let mut ok = true;

    let started = report.start(Step::Search);
    if !state.search.is_enabled() {
        report.finish(
            Step::Search,
            StepStatus::Skipped,
            "Meilisearch выключен",
            started,
        );
    } else {
        match search::update_index(state, &touched).await {
            Ok(()) => report.finish(
                Step::Search,
                StepStatus::Done,
                "Meilisearch обновлён",
                started,
            ),
            Err(error) => {
                tracing::warn!(
                    %error,
                    entities = touched.entities.len(),
                    "search index sync failed, run reindex"
                );
                ok = false;
                report.finish(
                    Step::Search,
                    StepStatus::Failed,
                    format!("Meilisearch не обновлён: {error}"),
                    started,
                );
            }
        }
    }

    let started = report.start(Step::Isr);
    // Старые адреса (смена slug, удаление) и новые.
    let mut paths = touched.before.clone();
    match touched.paths(&state.db).await {
        Ok(after) => paths.extend(after),
        Err(error) => tracing::warn!(%error, "page paths lookup after write failed"),
    }
    paths.sort_unstable();
    paths.dedup();
    if !state.isr.is_enabled() {
        report.finish(
            Step::Isr,
            StepStatus::Skipped,
            "ISR не настроен (ISR_URL, ISR_SECRET)",
            started,
        );
    } else {
        match state.isr.revalidate(&paths).await {
            Ok(()) => report.finish(
                Step::Isr,
                StepStatus::Done,
                format!("Статический HTML/JSON пересобран: {}", list(&paths)),
                started,
            ),
            Err(error) => {
                tracing::warn!(%error, paths = paths.len(), "isr revalidation failed");
                ok = false;
                report.finish(
                    Step::Isr,
                    StepStatus::Failed,
                    format!("Статика не пересобрана: {error}"),
                    started,
                );
            }
        }
    }
    ok
}

/// Поток публикации правки сущности `id`. Запись в БД шла с `db_started` до сих пор.
pub fn stream_entity(state: AppState, touched: Touched, db_started: Instant, id: Uuid) -> Response {
    jobs::stream(
        &[Step::Db, Step::Search, Step::Isr],
        move |report| async move {
            report.finish(Step::Db, StepStatus::Done, "БД обновлена", db_started);
            let ok = run(&state, touched, &report).await;
            let entity = match detail_by_id(&state.db, id).await {
                Ok(entity) => Some(Box::new(entity)),
                Err(error) => {
                    tracing::debug!(%error, entity_id = %id, "no card for the done event");
                    None
                }
            };
            DoneEvent {
                ok,
                entity,
                ..DoneEvent::default()
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_frontend_routes() {
        assert_eq!(
            entity_path(EntityKind::Movie, "dune-2021"),
            "/films/dune-2021"
        );
        assert_eq!(
            entity_path(EntityKind::Book, "dune-novel"),
            "/books/dune-novel"
        );
        assert_eq!(entity_path(EntityKind::Series, "x"), "/series/x");
        assert_eq!(
            entity_path(EntityKind::Game, "witcher-3"),
            "/games/witcher-3"
        );
        assert_eq!(person_path("denis-villeneuve"), "/people/denis-villeneuve");
    }

    #[test]
    fn long_path_lists_are_counted() {
        let paths: Vec<String> = (0..6).map(|i| format!("/films/{i}")).collect();
        assert_eq!(list(&paths[..2]), "/films/0, /films/1");
        assert_eq!(list(&paths), "6 страниц");
    }
}
