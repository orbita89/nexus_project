//! Типы БД и DTO каталога.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Видна на сайте (SQL, алиас `e`): активна и — если пришла из TMDB (`external_ids.tmdb`) — уже вышла
/// или выходит в ближайшие `catalog_ranking.soon_days` дней («Скоро премьера»); включённая вручную
/// (`active_manual`) — видна всегда. Невышедшие и без даты скрываются сами, без заданий: статус
/// считается по сегодняшней дате.
macro_rules! visible_sql {
    () => {
        "(e.active AND (e.active_manual \
         OR NOT e.external_ids ? 'tmdb' \
         OR coalesce(e.release_date <= current_date + (SELECT vr.soon_days FROM catalog_ranking vr), \
                     false)))"
    };
}

/// Статус выхода (SQL, алиас `e`): `released`, `soon` (в окне «Скоро премьера»), `announced`.
macro_rules! release_status_sql {
    () => {
        "(CASE WHEN e.release_date <= current_date THEN 'released' \
         WHEN e.release_date <= current_date + (SELECT sr.soon_days FROM catalog_ranking sr) \
         THEN 'soon' ELSE 'announced' END)"
    };
}

pub const VISIBLE: &str = visible_sql!();

/// Статус выхода по сегодняшней дате.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    /// Уже вышла (или не фильм из TMDB без даты — см. `visible`).
    #[default]
    Released,
    /// Выходит в ближайшие `soon_days` дней: «Скоро премьера».
    Soon,
    /// Позже или без даты.
    Announced,
}

impl TryFrom<String> for ReleaseStatus {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "released" => Ok(Self::Released),
            "soon" => Ok(Self::Soon),
            "announced" => Ok(Self::Announced),
            other => Err(format!("unknown release status {other:?}")),
        }
    }
}

/// Тип сущности (Postgres enum `entity_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "entity_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Movie,
    Series,
    Book,
    Game,
}

impl EntityKind {
    pub const ALL: [EntityKind; 4] = [Self::Movie, Self::Series, Self::Book, Self::Game];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
            Self::Book => "book",
            Self::Game => "game",
        }
    }

    /// `movie` → [`EntityKind::Movie`]; неизвестный тип — `None`.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Раздел в адресе страницы на фронтенде: `/films/dune-2021`
    /// (`KINDS` в nexus_web `src/lib/catalog/kinds.ts`).
    pub fn url_segment(self) -> &'static str {
        match self {
            Self::Movie => "films",
            Self::Series => "series",
            Self::Book => "books",
            Self::Game => "games",
        }
    }
}

pub use shared::pagination::{page_bounds, Page, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};

/// Сущность в списке.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct EntitySummary {
    pub id: Uuid,
    pub kind: EntityKind,
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    #[schema(example = "Dune")]
    pub original_title: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
    /// По сегодняшней дате: `soon` — бейдж «Скоро премьера».
    #[serde(default)]
    #[sqlx(try_from = "String")]
    pub release_status: ReleaseStatus,
    /// Рейтинг IMDb (0–10) и число голосов; нет — `null`.
    #[serde(default)]
    #[schema(example = 8.0)]
    pub imdb_rating: Option<f64>,
    #[serde(default)]
    #[schema(example = 912_000)]
    pub imdb_votes: Option<i32>,
}

pub const ENTITY_SUMMARY_COLUMNS: &str = concat!(
    "e.id, e.kind, e.slug, e.title, e.original_title, e.release_date, e.cover_url, ",
    release_status_sql!(),
    " AS release_status, \
     (SELECT es.imdb_rating FROM entity_stats es WHERE es.entity_id = e.id) AS imdb_rating, \
     (SELECT es.imdb_votes FROM entity_stats es WHERE es.entity_id = e.id) AS imdb_votes"
);

/// Сущность целиком (строка `entities`).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Entity {
    pub id: Uuid,
    pub kind: EntityKind,
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    #[schema(example = "Dune")]
    pub original_title: Option<String>,
    pub description: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_url: Option<String>,
    /// Поля, специфичные для `kind`.
    #[schema(value_type = crate::metadata::Metadata)]
    pub metadata: Value,
    /// Включена. Скрытая хранится в БД, но её нет в списках, поиске, похожих и фильмографиях, а
    /// карточка — 404 для всех, кроме админа.
    pub active: bool,
    /// Видна на сайте: `active` и (для фильмов из TMDB) уже вышла или «Скоро премьера», либо
    /// включена вручную.
    pub visible: bool,
    #[sqlx(try_from = "String")]
    pub release_status: ReleaseStatus,
    /// Статистика (`entity_stats`): IMDb, TMDB и балл «Рекомендуемые»; нет — `null`.
    pub imdb_rating: Option<f64>,
    pub imdb_votes: Option<i32>,
    /// TMDB popularity: «смотрят сейчас».
    pub tmdb_popularity: Option<f64>,
    pub tmdb_votes: Option<i32>,
    /// Байесовский рейтинг IMDb (сортировка `top_rated`).
    pub quality: Option<f64>,
    /// Балл «Рекомендуемые» (сортировка по умолчанию); у невышедших и малоизвестных — `null`.
    pub rank_score: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const ENTITY_COLUMNS: &str = concat!(
    "e.id, e.kind, e.slug, e.title, e.original_title, e.description, \
     e.release_date, e.cover_url, e.metadata, e.active, ",
    visible_sql!(),
    " AS visible, ",
    release_status_sql!(),
    " AS release_status, es.imdb_rating, es.imdb_votes, es.tmdb_popularity, es.tmdb_votes, \
     es.quality, es.rank_score, e.created_at, e.updated_at"
);

/// К `ENTITY_COLUMNS`: `FROM entities e {ENTITY_STATS_JOIN}`.
pub const ENTITY_STATS_JOIN: &str = "LEFT JOIN entity_stats es ON es.entity_id = e.id";

/// Карточка: сущность, теги, участники в порядке титров и связанные произведения.
#[derive(Debug, Serialize, ToSchema)]
pub struct EntityDetail {
    #[serde(flatten)]
    pub entity: Entity,
    /// Студии из `metadata.studios` в том же порядке, со ссылкой на страницу (`/studios/{slug}`).
    /// Названия, для которых студии ещё нет (появится при синхронизации), пропускаются.
    pub studios: Vec<crate::studios::StudioRef>,
    pub tags: Vec<Tag>,
    pub credits: Vec<EntityCredit>,
    pub related: Vec<RelatedWork>,
}

/// Связанное произведение в общей ленте карусели: сиквелы, ремейки и приквелы идут одним
/// списком хронологически (по `release_date`, без даты — в конце), у каждой карточки — тип связи.
#[derive(Debug, Serialize, ToSchema)]
pub struct RelatedWork {
    #[serde(flatten)]
    pub entity: EntitySummary,
    pub relation: RelationKind,
}

/// Вид связи в админке и в `related` карточки. `prequel` — то же, что `sequel` в обратную сторону.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RelationKind {
    /// `to_id` — сиквел этой сущности.
    Sequel,
    /// `to_id` — приквел этой сущности.
    Prequel,
    /// `to_id` — ремейк этой сущности (или её оригинал: показывается в обе стороны).
    Remake,
}

/// Связать сущность с другой.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutRelation {
    pub kind: RelationKind,
    pub to_id: Uuid,
    /// Порядок при одинаковой дате выхода, по умолчанию 0.
    pub position: Option<i32>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RelationQuery {
    /// Вид связи.
    #[param(inline)]
    pub kind: RelationKind,
}

/// Вид тега: `genre` — жанр («Драма», «Космическая опера»), `tag` — остальные метки
/// («Экранизация», «Открытый мир»).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema,
)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum TagKind {
    Genre,
    #[default]
    Tag,
}

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Tag {
    pub id: Uuid,
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
    pub kind: TagKind,
}

/// Колонки [`Tag`] для `FROM tags t`. Порядок в карточке: жанры первыми, затем по названию.
pub const TAG_COLUMNS: &str = "t.id, t.slug, t.name, t.kind";
pub const TAG_ORDER: &str = "t.kind = 'genre' DESC, t.name";

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct TagWithCount {
    pub id: Uuid,
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
    pub kind: TagKind,
    /// Сколько активных сущностей с этим тегом.
    #[schema(example = 12)]
    pub entities_count: i64,
}

/// Человек в титрах сущности.
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonRef {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub photo_url: Option<String>,
}

/// Участие человека в сущности (строка `entity_credits` + человек).
#[derive(Debug, Serialize, ToSchema)]
pub struct EntityCredit {
    /// id записи участия: по нему участие удаляется.
    pub id: Uuid,
    #[schema(example = "director")]
    pub role: String,
    #[schema(example = "Пол Атрейдес")]
    pub character_name: Option<String>,
    pub position: i32,
    pub person: PersonRef,
}

#[derive(sqlx::FromRow)]
pub struct EntityCreditRow {
    pub id: Uuid,
    pub role: String,
    pub character_name: Option<String>,
    pub position: i32,
    pub person_id: Uuid,
    pub person_slug: String,
    pub person_full_name: String,
    pub person_photo_url: Option<String>,
}

impl From<EntityCreditRow> for EntityCredit {
    fn from(row: EntityCreditRow) -> Self {
        Self {
            id: row.id,
            role: row.role,
            character_name: row.character_name,
            position: row.position,
            person: PersonRef {
                id: row.person_id,
                slug: row.person_slug,
                full_name: row.person_full_name,
                photo_url: row.person_photo_url,
            },
        }
    }
}

/// Человек в списке.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct PersonSummary {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
}

/// Человек целиком (строка `people`).
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Person {
    pub id: Uuid,
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    /// Имя латиницей (английское из TMDB), если отличается от `full_name`: по нему тоже ищут.
    #[schema(example = "Denis Villeneuve")]
    pub original_name: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
    pub bio: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const PERSON_COLUMNS: &str =
    "p.id, p.slug, p.full_name, p.original_name, p.birth_date, p.photo_url, p.bio, p.created_at, \
     p.updated_at";

/// Человек и его фильмография (новые работы сверху).
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonDetail {
    #[serde(flatten)]
    pub person: Person,
    pub credits: Vec<PersonCredit>,
}

/// Участие человека в сущности, со стороны человека.
#[derive(Debug, Serialize, ToSchema)]
pub struct PersonCredit {
    pub id: Uuid,
    #[schema(example = "director")]
    pub role: String,
    pub character_name: Option<String>,
    pub entity: EntitySummary,
}

#[derive(sqlx::FromRow)]
pub struct PersonCreditRow {
    pub id: Uuid,
    pub role: String,
    pub character_name: Option<String>,
    #[sqlx(flatten)]
    pub entity: EntitySummary,
}

impl From<PersonCreditRow> for PersonCredit {
    fn from(row: PersonCreditRow) -> Self {
        Self {
            id: row.id,
            role: row.role,
            character_name: row.character_name,
            entity: row.entity,
        }
    }
}

/// Сортировка поиска (без неё — по релевантности); у кого значения нет — в конце.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// Балл «Рекомендуемые» (`rank_score`).
    Recommended,
    /// TMDB popularity: смотрят сейчас.
    Trending,
    /// Байесовский рейтинг IMDb (`quality`).
    TopRated,
    /// Вышедшие, новые сверху.
    New,
    /// Ещё не вышедшие, ближайшие сверху.
    Soon,
    /// По названию.
    Title,
    /// Больше оценок Nexus.
    Popular,
    /// Средняя оценка Nexus (при равной — больше оценок).
    Rating,
}

/// Сущность в админском списке: как в публичном, плюс флаг активности.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct AdminEntitySummary {
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub entity: EntitySummary,
    pub active: bool,
    /// `active` переключали в админке: «пересобрать активные» её не трогает.
    pub active_manual: bool,
    /// Видна на сайте (активна и вышла / «скоро» / вручную / не из TMDB).
    pub visible: bool,
}

/// Итог «обновить рейтинги».
#[derive(Debug, Serialize, ToSchema)]
pub struct StatsResult {
    /// Файл дампа TMDB.
    #[schema(example = "movie_ids_10_11_2026.json.gz")]
    pub tmdb_export: String,
    /// У скольких сущностей изменилась TMDB popularity.
    pub tmdb_updated: u64,
    /// У скольких — рейтинг IMDb.
    pub imdb_updated: u64,
    /// У скольких есть балл «Рекомендуемые».
    pub ranked: i64,
}

/// Итог «пересобрать активные».
#[derive(Debug, Serialize, ToSchema)]
pub struct ActiveResult {
    pub activated: i64,
    pub deactivated: i64,
    /// Из них «Скоро премьера» (включаются всегда).
    pub soon: i64,
    /// Активных сущностей после (всех, не только из TMDB).
    pub active: i64,
}

/// Сколько вышедших фильмов из TMDB держать активными.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ActiveQuery {
    /// Топ по баллу «Рекомендуемые» (без балла — по числу оценок TMDB), 0–1000000, по умолчанию
    /// 80000. Плюс все «Скоро премьера».
    #[param(example = 80_000)]
    pub count: Option<i64>,
}

/// Настройки балла «Рекомендуемые» и окна «Скоро премьера» (`catalog_ranking`).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RankingSettings {
    /// Вес рейтинга IMDb, 0–10.
    #[schema(example = 0.5)]
    pub weight_quality: f64,
    /// Вес TMDB popularity, 0–10.
    #[schema(example = 0.3)]
    pub weight_popularity: f64,
    /// Вес свежести, 0–10.
    #[schema(example = 0.2)]
    pub weight_recency: f64,
    /// «Доверительные» голоса байесовского рейтинга: меньше голосов — ближе к среднему.
    #[schema(example = 5000)]
    pub trust_votes: i32,
    /// Через сколько лет свежесть падает вдвое, 0.1–100.
    #[schema(example = 4.0)]
    pub half_life_years: f64,
    /// Меньше голосов IMDb — без балла.
    #[schema(example = 1000)]
    pub min_votes: i32,
    /// Окно «Скоро премьера», дней (0–365).
    #[schema(example = 30)]
    pub soon_days: i32,
    /// Когда сохранены; в запросе не передаётся.
    #[serde(default, skip_deserializing)]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RankPreviewQuery {
    /// 1–200, по умолчанию 50.
    pub limit: Option<i64>,
}

/// Строка предпросмотра: фильм и составляющие балла.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct RankPreviewRow {
    pub slug: String,
    pub title: String,
    pub release_date: Option<NaiveDate>,
    pub active: bool,
    pub imdb_rating: Option<f64>,
    pub imdb_votes: Option<i32>,
    pub tmdb_popularity: Option<f64>,
    /// Байесовский рейтинг.
    pub quality: f64,
    /// Вклад рейтинга, популярности и свежести; `score` — их сумма.
    pub q_part: f64,
    pub p_part: f64,
    pub r_part: f64,
    pub score: f64,
}

/// Порядок списка сущностей.
#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntitySort {
    /// По умолчанию: балл «Рекомендуемые» (рейтинг IMDb + популярность + свежесть, настраивается
    /// в админке); без балла — в конце, новее выше.
    #[default]
    Recommended,
    /// Смотрят сейчас: TMDB popularity, только вышедшие с баллом.
    Trending,
    /// Высокий рейтинг: байесовский рейтинг IMDb.
    TopRated,
    /// Новинки: только вышедшие, новые сверху.
    New,
    /// «Скоро премьера»: ещё не вышедшие (в окне `soon_days`), ближайшие сверху.
    Soon,
    /// Больше оценок сверху.
    Popular,
    /// Выше средняя оценка сверху; без оценок — в конце.
    Rating,
    /// По названию.
    Title,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListEntitiesQuery {
    /// По умолчанию `recommended`.
    pub sort: Option<EntitySort>,
    /// Тип сущности.
    pub kind: Option<EntityKind>,
    /// Slug тега.
    #[param(example = "sci-fi")]
    pub tag: Option<String>,
    /// Год выхода.
    #[param(example = 2021)]
    pub year: Option<i32>,
    /// Slug студии (`GET /studios/{slug}`): её фильмы. Неизвестный slug — пустой список.
    #[param(example = "mosfilm")]
    pub studio: Option<String>,
    /// Не поддерживается: поиск по названию — только `GET /search` (Meilisearch), здесь — 400
    /// `use_search`.
    #[param(ignore)]
    pub q: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

/// Админский список: фильтры публичного плюс `active`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AdminListEntitiesQuery {
    /// По умолчанию `recommended`.
    pub sort: Option<EntitySort>,
    pub kind: Option<EntityKind>,
    /// Slug тега.
    pub tag: Option<String>,
    pub year: Option<i32>,
    /// Slug студии.
    pub studio: Option<String>,
    /// Подстрока в названии или оригинальном названии, без учёта регистра.
    pub q: Option<String>,
    /// `true` — только активные, `false` — только скрытые; не передан — все.
    pub active: Option<bool>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SimilarQuery {
    /// 1–24, по умолчанию 12. Вне диапазона приводится к границам.
    pub limit: Option<i64>,
}

/// Подсказки студий для фильтра поиска.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StudiosQuery {
    /// Начало или часть названия, опечатки допустимы. Пустой — самые частые.
    #[param(example = "warn")]
    pub q: Option<String>,
    /// Только студии сущностей этого типа.
    pub kind: Option<EntityKind>,
    /// 1–50, по умолчанию 10. Вне диапазона приводится к границам.
    pub limit: Option<i64>,
}

/// Поиск людей.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PeopleSearchQuery {
    /// Имя или его начало, русское или латиницей, опечатки допустимы; до 300 символов. Пустой —
    /// самые известные.
    #[param(example = "брэд")]
    pub q: Option<String>,
    /// Только с этой основной ролью: `actor`, `director`, `writer`, `producer`, ...
    #[param(example = "director")]
    pub known_for: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Человек в выдаче поиска людей.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PersonHit {
    pub id: Uuid,
    #[schema(example = "brad-pitt")]
    pub slug: String,
    #[schema(example = "Брэд Питт")]
    pub full_name: String,
    #[schema(example = "Brad Pitt")]
    pub original_name: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
    /// Основная роль: где вклад в известность больше всего.
    #[schema(example = "actor")]
    pub known_for: Option<String>,
}

/// Студия в подсказках: название (значение для `studio`) и сколько сущностей в индексе.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StudioHit {
    #[schema(example = "Warner Bros. Pictures")]
    pub name: String,
    /// Slug страницы студии (`GET /studios/{slug}`); `null` — страницы у студии ещё нет.
    #[schema(example = "warner-bros-pictures")]
    pub slug: Option<String>,
    pub count: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListPeopleQuery {
    /// Подстрока в имени, без учёта регистра.
    #[param(example = "кинг")]
    pub q: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    /// Поисковый запрос: название, оригинальное название, студии. Опечатки допустимы. Пустой —
    /// все, кто подходит под фильтры.
    #[param(example = "дюан")]
    pub q: Option<String>,
    /// Не передан — по релевантности.
    pub sort: Option<SearchSort>,
    pub kind: Option<EntityKind>,
    /// Slug тега.
    pub tag: Option<String>,
    /// Ровно этот год выхода. Вместе с `year_from` / `year_to` — 400.
    #[param(example = 2001)]
    pub year: Option<i32>,
    /// Год выхода от (включительно).
    #[param(example = 2000)]
    pub year_from: Option<i32>,
    /// Год выхода до (включительно).
    #[param(example = 2010)]
    pub year_to: Option<i32>,
    /// Студия — точное название (без учёта регистра); подсказки — `GET /search/studios`.
    #[param(example = "Warner Bros. Pictures")]
    pub studio: Option<String>,
    /// Slug студии (`GET /studios/{slug}`): её фильмы, без запроса названия. Неизвестный slug —
    /// пустая выдача.
    #[param(example = "warner-bros-pictures")]
    pub studio_slug: Option<String>,
    /// Slug человека: сущности с его участием (в любой роли).
    #[param(example = "christopher-nolan")]
    pub person: Option<String>,
    /// Роль человека из `person` в этой сущности: `actor`, `director`, `producer`, `writer`, ...
    /// Без `person` — 400.
    #[param(example = "director")]
    pub role: Option<String>,
    /// 1–100, по умолчанию 20.
    pub limit: Option<i64>,
    /// С 0.
    pub offset: Option<i64>,
}

// ------------------------------------------------------------------ admin

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEntity {
    pub kind: EntityKind,
    /// Латиница в нижнем регистре, цифры и дефисы.
    #[schema(example = "dune-2021")]
    pub slug: String,
    #[schema(example = "Дюна")]
    pub title: String,
    #[schema(example = "Dune")]
    pub original_title: Option<String>,
    pub description: Option<String>,
    pub release_date: Option<NaiveDate>,
    /// `http(s)://...`
    pub cover_url: Option<String>,
    /// Поля для `kind`. По умолчанию `{}`.
    #[schema(value_type = Option<crate::metadata::Metadata>)]
    pub metadata: Option<Value>,
    /// Slug'и существующих тегов.
    #[serde(default)]
    #[schema(example = json!(["sci-fi"]))]
    pub tags: Vec<String>,
    /// Видна на сайте. По умолчанию `true`.
    pub active: Option<bool>,
}

/// Изменение сущности: переданные поля заменяются, `null` очищает необязательное поле.
/// `kind` не меняется. `metadata` заменяется целиком.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateEntity {
    pub slug: Option<String>,
    pub title: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub original_title: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<NaiveDate>)]
    pub release_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub cover_url: Option<Option<String>>,
    #[schema(value_type = Option<crate::metadata::Metadata>)]
    pub metadata: Option<Value>,
    /// Включить (`true`) или скрыть с сайта (`false`). Помечает `active_manual`: «пересобрать
    /// активные» эту сущность больше не трогает.
    pub active: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTags {
    /// Slug'и тегов: заменяют текущий набор целиком.
    #[schema(example = json!(["sci-fi", "space-opera"]))]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddCredit {
    pub person_id: Uuid,
    /// `actor`, `director`, `writer`, `author`, `composer`, `developer`, ...
    /// (латиница в нижнем регистре и `_`).
    #[schema(example = "actor")]
    pub role: String,
    #[schema(example = "Пол Атрейдес")]
    pub character_name: Option<String>,
    /// Порядок в титрах, по умолчанию 0.
    pub position: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePerson {
    #[schema(example = "denis-villeneuve")]
    pub slug: String,
    #[schema(example = "Дени Вильнёв")]
    pub full_name: String,
    /// Имя латиницей, если отличается от `full_name`.
    #[schema(example = "Denis Villeneuve")]
    pub original_name: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub photo_url: Option<String>,
    pub bio: Option<String>,
}

/// Изменение человека: переданные поля заменяются, `null` очищает необязательное поле.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePerson {
    pub slug: Option<String>,
    pub full_name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub original_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<NaiveDate>)]
    pub birth_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub photo_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub bio: Option<Option<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTag {
    #[schema(example = "sci-fi")]
    pub slug: String,
    #[schema(example = "Научная фантастика")]
    pub name: String,
    /// По умолчанию `tag`.
    #[serde(default)]
    pub kind: TagKind,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTag {
    pub slug: Option<String>,
    pub name: Option<String>,
    pub kind: Option<TagKind>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTagsQuery {
    /// Только жанры или только остальные теги; не передан — все.
    #[param(inline)]
    pub kind: Option<TagKind>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReindexResult {
    /// Сколько сущностей в новом индексе.
    #[schema(example = 100_000)]
    pub indexed: usize,
    /// Сколько людей в новом индексе `people`.
    #[schema(example = 880_000)]
    pub people_indexed: usize,
    /// Сколько документов было в индексе до перестройки.
    #[schema(example = 99_870)]
    pub previous_count: u64,
    /// Где сохранена предыдущая версия (откат — swap обратно).
    #[schema(example = "entities_v1759912345123")]
    pub previous_version: String,
    /// Удалённые устаревшие версии.
    #[schema(example = json!(["entities_v1759800000000"]))]
    pub deleted: Vec<String>,
}

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReindexQuery {
    /// Переключить поиск, даже если новый индекс меньше 80% текущего (сущности удалены намеренно).
    #[serde(default)]
    pub force: bool,
}

/// Отличает отсутствующее поле (`None`) от `null` (`Some(None)`). Вместе с `#[serde(default)]`.
pub(crate) fn nullable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
