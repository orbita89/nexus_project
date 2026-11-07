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
/// Связи раскрываются сами: у сущности — её участники (их страницы показывают сущность) и
/// связанные произведения (сиквелы, приквелы, ремейки показывают её в `related`), у человека — его
/// работы (их страницы и документы показывают человека). Собирать **до**
/// записи, если запись удаляет или меняет slug: старые адреса запоминаются, чтобы пересобрать и их.
#[derive(Debug, Default)]
pub struct Touched {
    pub(crate) entities: Vec<Uuid>,
    pub(crate) people: Vec<Uuid>,
    /// Премии, чьи страницы задеты напрямую (правка самой премии, её церемоний и категорий):
    /// у премии без номинаций работ и людей нет, а страница меняется.
    pub(crate) awards: Vec<Uuid>,
    /// Церемонии, чьи страницы задеты: их адрес нужен и тогда, когда номинаций у церемонии не
    /// осталось (их только что сняли).
    pub(crate) editions: Vec<Uuid>,
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
        let related: Vec<Uuid> = sqlx::query_scalar(
            "SELECT to_id FROM entity_relations WHERE from_id = ANY($1)
             UNION SELECT from_id FROM entity_relations WHERE to_id = ANY($1)",
        )
        .bind(entities)
        .fetch_all(db)
        .await?;
        let mut touched = Self {
            entities: merge(entities, merge(&entities_of, related)),
            people: merge(people, people_of),
            awards: Vec::new(),
            editions: Vec::new(),
            before: Vec::new(),
        };
        touched.before = touched.paths(db).await?;
        Ok(touched)
    }

    /// Та же сборка, но со страницами премий `awards` и церемоний `editions` (админка наград).
    /// Адреса «до» берутся уже с ними, поэтому удалённая церемония тоже пересобирается.
    pub async fn collect_awards(
        db: &sqlx::PgPool,
        entities: &[Uuid],
        people: &[Uuid],
        awards: &[Uuid],
        editions: &[Uuid],
    ) -> Result<Self, sqlx::Error> {
        let mut touched = Self::collect(db, entities, people).await?;
        touched.awards = merge(awards, Vec::new());
        touched.editions = merge(editions, Vec::new());
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

    /// Сущности, у которых поменялись только связи между собой: их slug'и не меняются, а
    /// участников и связанных пересобирать незачем.
    pub fn entities(ids: &[Uuid]) -> Self {
        Self {
            entities: merge(ids, Vec::new()),
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
        self.entities.is_empty()
            && self.people.is_empty()
            && self.awards.is_empty()
            && self.editions.is_empty()
    }

    /// Страницы фронтенда по slug'ам, которые сейчас в БД.
    async fn paths(&self, db: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
        // У сезона свой адрес внутри сериала (`/series/arcane/season-2`), поэтому берём и slug
        // родителя с номером сезона.
        let entities: Vec<(EntityKind, String, Option<String>, Option<i32>)> = sqlx::query_as(
            "SELECT e.kind, e.slug, p.slug, (e.metadata ->> 'number')::int
             FROM entities e LEFT JOIN entities p ON p.id = e.parent_id
             WHERE e.id = ANY($1)",
        )
        .bind(&self.entities)
        .fetch_all(db)
        .await?;
        let people: Vec<String> = sqlx::query_scalar("SELECT slug FROM people WHERE id = ANY($1)")
            .bind(&self.people)
            .fetch_all(db)
            .await?;
        // Страницы студий сущностей: в их списке фильмов меняется карточка.
        let studios: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT st.slug FROM entities e,
                    jsonb_array_elements_text(CASE jsonb_typeof(e.metadata -> 'studios')
                        WHEN 'array' THEN e.metadata -> 'studios' ELSE '[]' END) s
             JOIN studios st ON st.name = s
             WHERE e.id = ANY($1)",
        )
        .bind(&self.entities)
        .fetch_all(db)
        .await?;
        // Страницы наград: церемонии, где участвуют затронутые работы и люди, и страницы самих
        // премий из `awards`. Список раздела тоже пересобирается — на плитке премии стоят годы
        // и число церемоний.
        let editions: Vec<(String, i32)> = sqlx::query_as(
            "SELECT DISTINCT a.slug, ed.year FROM award_nominations n
               JOIN award_editions ed ON ed.id = n.edition_id
               JOIN awards a ON a.id = ed.award_id
              WHERE n.entity_id = ANY($1) OR n.person_id = ANY($2) OR a.id = ANY($3)
             UNION
            SELECT a.slug, ed.year FROM award_editions ed
               JOIN awards a ON a.id = ed.award_id
              WHERE ed.id = ANY($4)",
        )
        .bind(&self.entities)
        .bind(&self.people)
        .bind(&self.awards)
        .bind(&self.editions)
        .fetch_all(db)
        .await?;
        let award_slugs: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT a.slug FROM awards a
              WHERE a.id = ANY($3)
                 OR EXISTS (SELECT 1 FROM award_editions ed
                              JOIN award_nominations n ON n.edition_id = ed.id
                             WHERE ed.award_id = a.id
                               AND (n.entity_id = ANY($1) OR n.person_id = ANY($2)))
                 OR EXISTS (SELECT 1 FROM award_editions ed
                             WHERE ed.award_id = a.id AND ed.id = ANY($4))",
        )
        .bind(&self.entities)
        .bind(&self.people)
        .bind(&self.awards)
        .bind(&self.editions)
        .fetch_all(db)
        .await?;
        let section =
            (!award_slugs.is_empty() || !self.awards.is_empty()).then(|| AWARDS_PATH.to_owned());

        Ok(entities
            .iter()
            .flat_map(|(kind, slug, parent, number)| entity_paths(*kind, slug, parent, *number))
            .chain(people.iter().map(|slug| person_path(slug)))
            .chain(studios.iter().map(|slug| studio_path(slug)))
            .chain(award_slugs.iter().map(|slug| award_path(slug)))
            .chain(
                editions
                    .iter()
                    .map(|(slug, year)| award_edition_path(slug, *year)),
            )
            .chain(section)
            .collect())
    }
}

/// Страница сущности на фронтенде: `/films/dune-2021`.
pub fn entity_path(kind: EntityKind, slug: &str) -> String {
    format!("/{}/{slug}", kind.url_segment())
}

/// Страница сезона на фронтенде: `/series/arcane/season-2`. Своего раздела у сезона нет — он
/// живёт внутри сериала, поэтому одного его slug'а (`arcane-season-2`) для адреса мало.
pub fn season_path(series_slug: &str, number: i32) -> String {
    format!("/series/{series_slug}/season-{number}")
}

/// Страницы, которые задевает правка сущности. У сезона их две: его собственная и страница
/// сериала — в её шапке стоит переключатель сезонов. Сезон без родителя (битые данные) своей
/// страницы не имеет.
fn entity_paths(
    kind: EntityKind,
    slug: &str,
    parent: &Option<String>,
    number: Option<i32>,
) -> Vec<String> {
    if kind != EntityKind::Season {
        return vec![entity_path(kind, slug)];
    }
    match (parent, number) {
        (Some(parent), Some(number)) => vec![
            season_path(parent, number),
            entity_path(EntityKind::Series, parent),
        ],
        (Some(parent), None) => vec![entity_path(EntityKind::Series, parent)],
        _ => Vec::new(),
    }
}

/// Страница человека на фронтенде.
pub fn person_path(slug: &str) -> String {
    format!("/people/{slug}")
}

/// Страница студии на фронтенде.
pub fn studio_path(slug: &str) -> String {
    format!("/studios/{slug}")
}

/// Список премий на фронтенде.
pub const AWARDS_PATH: &str = "/awards";

/// Страница премии на фронтенде.
pub fn award_path(slug: &str) -> String {
    format!("/awards/{slug}")
}

/// Страница церемонии на фронтенде.
pub fn award_edition_path(slug: &str, year: i32) -> String {
    format!("/awards/{slug}/{year}")
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
    // Новые названия студий в `metadata` — страницы студий (до ссылок в карточках и ISR).
    if !touched.entities.is_empty() {
        if let Err(error) = crate::studios::sync(&state.db, Some(&touched.entities), &[]).await {
            tracing::warn!(%error, "studios sync after write failed");
        }
    }

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
        assert_eq!(season_path("arcane", 2), "/series/arcane/season-2");
        assert_eq!(
            entity_paths(
                EntityKind::Season,
                "arcane-season-2",
                &Some("arcane".into()),
                Some(2)
            ),
            ["/series/arcane/season-2", "/series/arcane"]
        );
        assert_eq!(
            entity_paths(EntityKind::Movie, "dune-2021", &None, None),
            ["/films/dune-2021"]
        );
        assert!(entity_paths(EntityKind::Season, "orphan", &None, Some(1)).is_empty());
        assert_eq!(person_path("denis-villeneuve"), "/people/denis-villeneuve");
        assert_eq!(award_path("oscar"), "/awards/oscar");
        assert_eq!(award_edition_path("oscar", 2024), "/awards/oscar/2024");
    }

    #[test]
    fn long_path_lists_are_counted() {
        let paths: Vec<String> = (0..6).map(|i| format!("/films/{i}")).collect();
        assert_eq!(list(&paths[..2]), "/films/0, /films/1");
        assert_eq!(list(&paths), "6 страниц");
    }
}
