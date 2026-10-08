//! Публикация правки каталога после коммита в БД: поисковый индекс, кэш карточек людей,
//! статика фронтенда (ISR).
//!
//! Шаги идут по порядку: [`Step::Search`] — документы в Meilisearch (с ожиданием применения),
//! затем сброс карточек людей в кэше, затем [`Step::Isr`] — пересборка страниц затронутых
//! сущностей и людей (`/films/dune-2021`, `/people/denis-villeneuve`), старых и новых адресов.
//! Ошибка шага запись не отменяет и следующие шаги не останавливает: индекс догонит
//! перестройка, статику — следующая правка или полная ревалидация.
//!
//! Обычные админские эндпоинты вызывают [`run`] молча, `.../stream` транслирует шаги в SSE.

use crate::entities::detail_by_id;
use crate::models::{EntityDetail, EntityKind};
use crate::search;
use axum::http::HeaderName;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use shared::AppState;
use std::convert::Infallible;
use std::time::Instant;
use tokio::sync::mpsc;
use utoipa::ToSchema;
use uuid::Uuid;

/// Шаг публикации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Запись в PostgreSQL (выполняется до публикации, в потоке приходит первым).
    Db,
    /// Документы в Meilisearch.
    Search,
    /// Пересборка статических страниц фронтенда.
    Isr,
}

impl Step {
    pub const ALL: [Step; 3] = [Self::Db, Self::Search, Self::Isr];

    pub fn title(self) -> &'static str {
        match self {
            Self::Db => "Сохранение в БД",
            Self::Search => "Обновление поиска (Meilisearch)",
            Self::Isr => "Пересборка статики (ISR)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Done,
    /// Не настроено (Meilisearch или ISR выключены).
    Skipped,
    Failed,
}

/// Состояние шага. На каждый шаг приходит `running`, затем итог.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StepEvent {
    pub step: Step,
    pub status: StepStatus,
    /// Строка для лога в админке.
    #[schema(example = "Meilisearch обновлён")]
    pub message: String,
    /// Сколько шёл шаг; только у итога.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl StepEvent {
    fn running(step: Step) -> Self {
        Self {
            step,
            status: StepStatus::Running,
            message: step.title().to_string(),
            duration_ms: None,
        }
    }

    pub(crate) fn finished(
        step: Step,
        status: StepStatus,
        message: impl Into<String>,
        started: Instant,
    ) -> Self {
        Self {
            step,
            status,
            message: message.into(),
            duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        }
    }
}

/// Шаг в событии `plan`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlannedStep {
    pub step: Step,
    #[schema(example = "Обновление поиска (Meilisearch)")]
    pub title: &'static str,
}

/// Событие потока публикации (`text/event-stream`): `event:` — значение `type`, `data:` — весь
/// этот JSON.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublishEvent {
    /// Первое: все шаги по порядку, чтобы сразу показать их ожидающими.
    Plan { steps: Vec<PlannedStep> },
    /// Шаг начался или закончился.
    Step(StepEvent),
    /// Последнее. `ok` — ни один шаг не упал; `entity` — карточка после записи (`null`, если
    /// сущность успели удалить).
    Done {
        ok: bool,
        entity: Option<Box<EntityDetail>>,
    },
}

impl PublishEvent {
    fn plan() -> Self {
        Self::Plan {
            steps: Step::ALL
                .into_iter()
                .map(|step| PlannedStep {
                    step,
                    title: step.title(),
                })
                .collect(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::Step(_) => "step",
            Self::Done { .. } => "done",
        }
    }

    fn to_sse(&self) -> Event {
        Event::default()
            .event(self.name())
            .json_data(self)
            .expect("publish event serializes to JSON")
    }
}

/// Ответ `text/event-stream` с публикацией правки сущности `id`. Запись в БД уже прошла: `db` —
/// её итог. Публикация идёт в отдельной задаче и доводится до конца, даже если клиент ушёл.
pub fn stream_entity(state: AppState, touched: Touched, db: StepEvent, id: Uuid) -> Response {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Ошибка отправки — клиент закрыл соединение; публикация всё равно доводится до конца.
        let send = |event: PublishEvent| {
            let _ = tx.send(event);
        };
        send(PublishEvent::plan());
        send(PublishEvent::Step(db));
        let ok = run(&state, touched, |event| send(PublishEvent::Step(event))).await;
        let entity = match detail_by_id(&state.db, id).await {
            Ok(entity) => Some(Box::new(entity)),
            Err(error) => {
                tracing::debug!(%error, entity_id = %id, "no card for the done event");
                None
            }
        };
        send(PublishEvent::Done { ok, entity });
    });

    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        let event = rx.recv().await?;
        Some((Ok::<_, Infallible>(event.to_sse()), rx))
    });
    (
        // nginx иначе копит поток в буфере и отдаёт одним куском в конце.
        [(HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(events).keep_alive(KeepAlive::default()),
    )
        .into_response()
}

/// Что затронула запись в каталоге: какие документы переотправить, какие карточки сбросить и
/// какие страницы пересобрать.
///
/// Связи раскрываются сами: у сущности — её участники (их карточки показывают сущность),
/// у человека — его работы (их карточки показывают человека). Собирать **до** записи, если
/// запись удаляет или меняет slug: старые адреса запоминаются, чтобы сбросить и их.
#[derive(Debug, Default)]
pub struct Touched {
    pub(crate) entities: Vec<Uuid>,
    pub(crate) people: Vec<Uuid>,
    /// Slug'и на момент сбора.
    before: Slugs,
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
            before: Slugs::default(),
        };
        touched.before = Slugs::load(db, &touched).await?;
        Ok(touched)
    }

    /// Новая сущность (до неё ничего не было ни в кэше, ни в статике).
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
}

/// Адреса затронутого на один момент времени.
#[derive(Debug, Default)]
struct Slugs {
    entities: Vec<(EntityKind, String)>,
    people: Vec<String>,
}

impl Slugs {
    async fn load(db: &sqlx::PgPool, touched: &Touched) -> Result<Self, sqlx::Error> {
        Ok(Self {
            entities: sqlx::query_as("SELECT kind, slug FROM entities WHERE id = ANY($1)")
                .bind(&touched.entities)
                .fetch_all(db)
                .await?,
            people: sqlx::query_scalar("SELECT slug FROM people WHERE id = ANY($1)")
                .bind(&touched.people)
                .fetch_all(db)
                .await?,
        })
    }

    /// Ключи карточек в кэше (там только люди).
    fn cache_keys(&self) -> impl Iterator<Item = String> + '_ {
        self.people
            .iter()
            .map(|slug| crate::cards::CardKind::Person.key(slug))
    }

    /// Страницы фронтенда.
    fn paths(&self) -> impl Iterator<Item = String> + '_ {
        let entities = self
            .entities
            .iter()
            .map(|(kind, slug)| format!("/{}/{slug}", kind.url_segment()));
        let people = self.people.iter().map(|slug| format!("/people/{slug}"));
        entities.chain(people)
    }
}

fn merge(a: &[Uuid], b: Vec<Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = a.iter().copied().chain(b).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn sorted(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut items: Vec<String> = items.collect();
    items.sort_unstable();
    items.dedup();
    items
}

/// Пути для лога: несколько — списком, много (переименован тег) — числом.
fn list(paths: &[String]) -> String {
    if paths.len() <= 5 {
        paths.join(", ")
    } else {
        format!("{} страниц", paths.len())
    }
}

/// Публикует запись: шаги [`Step::Search`] и [`Step::Isr`], каждый сообщает о себе в `report`.
/// Вызывать после коммита. `true` — ни один шаг не упал (пропущенные не считаются).
pub async fn run(state: &AppState, touched: Touched, mut report: impl FnMut(StepEvent)) -> bool {
    if touched.is_empty() {
        return true;
    }
    let mut ok = true;

    report(StepEvent::running(Step::Search));
    let started = Instant::now();
    let event = if !state.search.is_enabled() {
        StepEvent::finished(
            Step::Search,
            StepStatus::Skipped,
            "Meilisearch выключен",
            started,
        )
    } else {
        match search::update_index(state, &touched).await {
            Ok(()) => StepEvent::finished(
                Step::Search,
                StepStatus::Done,
                "Meilisearch обновлён",
                started,
            ),
            Err(error) => {
                tracing::warn!(
                    %error,
                    entities = touched.entities.len(),
                    people = touched.people.len(),
                    "search index sync failed, run reindex"
                );
                ok = false;
                StepEvent::finished(
                    Step::Search,
                    StepStatus::Failed,
                    format!("Meilisearch не обновлён: {error}"),
                    started,
                )
            }
        }
    };

    // Адреса после записи: новые slug'и. Кэш сбрасывается только после применения индекса,
    // иначе промах, начатый до правки, положит в кэш старую карточку.
    let after = Slugs::load(&state.db, &touched)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "slugs lookup after write failed");
            Slugs::default()
        });
    let keys = sorted(touched.before.cache_keys().chain(after.cache_keys()));
    state.cache.invalidate(&keys).await;
    report(event);

    report(StepEvent::running(Step::Isr));
    let started = Instant::now();
    let paths = sorted(touched.before.paths().chain(after.paths()));
    let event = if !state.isr.is_enabled() {
        StepEvent::finished(
            Step::Isr,
            StepStatus::Skipped,
            "ISR не настроен (ISR_URL, ISR_SECRET)",
            started,
        )
    } else {
        match state.isr.revalidate(&paths).await {
            Ok(()) => StepEvent::finished(
                Step::Isr,
                StepStatus::Done,
                format!("Статический HTML/JSON пересобран: {}", list(&paths)),
                started,
            ),
            Err(error) => {
                tracing::warn!(%error, paths = paths.len(), "isr revalidation failed");
                ok = false;
                StepEvent::finished(
                    Step::Isr,
                    StepStatus::Failed,
                    format!("Статика не пересобрана: {error}"),
                    started,
                )
            }
        }
    };
    report(event);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_frontend_routes() {
        let slugs = Slugs {
            entities: vec![
                (EntityKind::Movie, "dune-2021".into()),
                (EntityKind::Book, "dune-novel".into()),
            ],
            people: vec!["denis-villeneuve".into()],
        };
        assert_eq!(
            slugs.paths().collect::<Vec<_>>(),
            [
                "/films/dune-2021",
                "/books/dune-novel",
                "/people/denis-villeneuve"
            ]
        );
        assert_eq!(
            slugs.cache_keys().collect::<Vec<_>>(),
            ["catalog:person:denis-villeneuve"]
        );
    }
}
