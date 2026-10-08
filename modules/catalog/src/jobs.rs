//! Долгие операции админки с логом шагов: публикация правки ([`crate::publish`]) и перестройка
//! поиска ([`crate::search::reindex`]).
//!
//! События общие, чтобы админка рисовала один лог в стиле GitHub Actions: `plan` (все шаги
//! операции), `step` (`running`, затем итог), `progress` (счётчик длинного шага), `done`.
//! Поток — `text/event-stream`: `event:` — тип, `data:` — JSON события с тем же `type`.

use crate::models::{EntityDetail, ReindexResult};
use axum::http::HeaderName;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::convert::Infallible;
use std::future::Future;
use std::time::Instant;
use tokio::sync::mpsc;
use utoipa::ToSchema;

/// Шаг операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Публикация: запись в PostgreSQL.
    Db,
    /// Публикация: документы в Meilisearch.
    Search,
    /// Перестройка: теневой индекс `entities_v<ms>`.
    CreateIndex,
    /// Перестройка: все сущности из БД пачками в теневой индекс (с `progress`).
    Fill,
    /// Перестройка: новый индекс не меньше 80% текущего.
    Check,
    /// Перестройка: поиск переключается на новый индекс.
    Swap,
    /// Перестройка: удаление версий старше предыдущей.
    Rotate,
    /// Пересборка статических страниц фронтенда.
    Isr,
}

impl Step {
    pub fn title(self) -> &'static str {
        match self {
            Self::Db => "Сохранение в БД",
            Self::Search => "Обновление поиска (Meilisearch)",
            Self::CreateIndex => "Создание теневого индекса",
            Self::Fill => "Индексация сущностей",
            Self::Check => "Проверка размера нового индекса",
            Self::Swap => "Переключение поиска на новый индекс",
            Self::Rotate => "Удаление устаревших индексов",
            Self::Isr => "Пересборка статики (ISR)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Done,
    /// Не настроено (Meilisearch или ISR выключены) или не нужно.
    Skipped,
    Failed,
}

/// Состояние шага. На каждый шаг приходит `running`, затем итог (у `db` — сразу итог).
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

/// Шаг в событии `plan`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlannedStep {
    pub step: Step,
    #[schema(example = "Индексация сущностей")]
    pub title: &'static str,
}

/// Счётчик длинного шага: «Проиндексировано 500/100000».
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProgressEvent {
    pub step: Step,
    #[schema(example = 500)]
    pub done: u64,
    #[schema(example = 100_000)]
    pub total: u64,
}

/// Итог операции.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct DoneEvent {
    /// Ни один шаг не упал (пропущенные не считаются).
    pub ok: bool,
    /// Публикация: карточка после записи (`null`, если сущность успели удалить).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<Box<EntityDetail>>,
    /// Перестройка: что получилось (нет, если она прервалась).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reindex: Option<ReindexResult>,
}

/// Событие потока.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    /// Первое: все шаги по порядку, чтобы сразу показать их ожидающими.
    Plan { steps: Vec<PlannedStep> },
    /// Шаг начался или закончился.
    Step(StepEvent),
    /// Продвинулся длинный шаг.
    Progress(ProgressEvent),
    /// Последнее.
    Done(DoneEvent),
}

impl JobEvent {
    fn name(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::Step(_) => "step",
            Self::Progress(_) => "progress",
            Self::Done(_) => "done",
        }
    }

    fn to_sse(&self) -> Event {
        Event::default()
            .event(self.name())
            .json_data(self)
            .expect("job event serializes to JSON")
    }
}

/// Куда операция сообщает о шагах. [`Reporter::silent`] — никуда (обычные эндпоинты, расписание).
#[derive(Clone, Default)]
pub struct Reporter {
    tx: Option<mpsc::UnboundedSender<JobEvent>>,
}

impl Reporter {
    pub fn silent() -> Self {
        Self::default()
    }

    fn send(&self, event: JobEvent) {
        // Ошибка отправки — клиент закрыл соединение; операция всё равно доводится до конца.
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }

    /// Шаг начался: `running`. Возвращает время начала для [`Reporter::finish`].
    pub fn start(&self, step: Step) -> Instant {
        self.send(JobEvent::Step(StepEvent {
            step,
            status: StepStatus::Running,
            message: step.title().to_string(),
            duration_ms: None,
        }));
        Instant::now()
    }

    /// Итог шага.
    pub fn finish(
        &self,
        step: Step,
        status: StepStatus,
        message: impl Into<String>,
        started: Instant,
    ) {
        self.send(JobEvent::Step(StepEvent {
            step,
            status,
            message: message.into(),
            duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        }));
    }

    pub fn progress(&self, step: Step, done: u64, total: u64) {
        self.send(JobEvent::Progress(ProgressEvent { step, done, total }));
    }
}

/// Ответ `text/event-stream`: `plan` из `steps`, события `job`, затем его итог в `done`.
/// `job` идёт в отдельной задаче и доводится до конца, даже если клиент ушёл.
pub fn stream<F, Fut>(steps: &[Step], job: F) -> Response
where
    F: FnOnce(Reporter) -> Fut + Send + 'static,
    Fut: Future<Output = DoneEvent> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let reporter = Reporter { tx: Some(tx) };
    reporter.send(JobEvent::Plan {
        steps: steps
            .iter()
            .map(|&step| PlannedStep {
                step,
                title: step.title(),
            })
            .collect(),
    });
    tokio::spawn(async move {
        let done = job(reporter.clone()).await;
        reporter.send(JobEvent::Done(done));
        // Последний отправитель уходит — поток закрывается.
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
