//! Структура `entities.metadata` по типам сущности. БД её не проверяет, поэтому всё, что
//! пишется через API, проходит через [`validate`]: неизвестные поля и неверные значения дают 400.
//!
//! Новый тип контента: вариант в `EntityKind` (+ миграция `ALTER TYPE entity_kind ADD VALUE`),
//! структура здесь и ветка в [`validate`].

use crate::models::EntityKind;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use shared::{AppError, AppResult};
use utoipa::ToSchema;

/// Поля фильма.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MovieMetadata {
    /// Длительность в минутах.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 155, minimum = 1, maximum = 10000)]
    pub runtime_min: Option<u32>,
    /// Страны производства, ISO 3166-1 alpha-2.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(["US", "CA"]))]
    pub countries: Option<Vec<String>>,
    /// Возрастной рейтинг.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "PG-13")]
    pub age_rating: Option<String>,
    /// Трейлер: источники по приоритету, фронтенд показывает первый доступный у зрителя.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!([
        { "provider": "youtube", "id": "n9xhJrPXop4" },
        { "provider": "rutube", "id": "0ab1fc1e47f2e9b89e9e59d9db36f2b4" }
    ]))]
    pub trailers: Option<Vec<TrailerSource>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SeriesStatus {
    /// Ещё выходит.
    Ongoing,
    /// Завершён.
    Ended,
    /// Закрыт до завершения.
    Canceled,
}

/// Поля сериала.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeriesMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 4, minimum = 1, maximum = 10000)]
    pub seasons: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 32, minimum = 1, maximum = 100000)]
    pub episodes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<SeriesStatus>,
    /// Страны производства, ISO 3166-1 alpha-2.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(["US"]))]
    pub countries: Option<Vec<String>>,
    /// Трейлер: источники по приоритету, фронтенд показывает первый доступный у зрителя.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!([
        { "provider": "youtube", "id": "n9xhJrPXop4" },
        { "provider": "rutube", "id": "0ab1fc1e47f2e9b89e9e59d9db36f2b4" }
    ]))]
    pub trailers: Option<Vec<TrailerSource>>,
}

/// Поля книги.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BookMetadata {
    /// ISBN-10 или ISBN-13, дефисы допустимы. Хранится без дефисов.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "9780441013593")]
    pub isbn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 896, minimum = 1, maximum = 100000)]
    pub pages: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Ace Books")]
    pub publisher: Option<String>,
}

/// Поля игры.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GameMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(["pc", "ps5", "xbox-series"]))]
    pub platforms: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "CD Projekt Red")]
    pub developer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "CD Projekt")]
    pub publisher: Option<String>,
    /// Трейлер: источники по приоритету, фронтенд показывает первый доступный у зрителя.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!([
        { "provider": "youtube", "id": "n9xhJrPXop4" },
        { "provider": "rutube", "id": "0ab1fc1e47f2e9b89e9e59d9db36f2b4" }
    ]))]
    pub trailers: Option<Vec<TrailerSource>>,
}

/// Видеосервис трейлера. Фронтенд сам строит адрес плеера по провайдеру и id, поэтому в БД не
/// попадает произвольный URL (и чужой домен в iframe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrailerProvider {
    /// `https://www.youtube-nocookie.com/embed/{id}`; id — 11 символов.
    Youtube,
    /// `https://rutube.ru/play/embed/{id}`; id — 32 hex-символа. Открывается там, где YouTube
    /// заблокирован.
    Rutube,
}

/// Источник трейлера. На входе можно передать и просто ссылку на видео — провайдер и id
/// определятся сами; в `id` тоже допустима ссылка.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "TrailerInput")]
pub struct TrailerSource {
    pub provider: TrailerProvider,
    #[schema(example = "n9xhJrPXop4")]
    pub id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TrailerInput {
    Link(String),
    Source(RawTrailerSource),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrailerSource {
    provider: String,
    id: String,
}

impl TryFrom<TrailerInput> for TrailerSource {
    type Error = String;

    fn try_from(input: TrailerInput) -> Result<Self, String> {
        match input {
            TrailerInput::Link(link) => parse_video_link(link.trim()).ok_or_else(|| {
                format!("metadata.trailers: {link:?} is not a YouTube or Rutube video link")
            }),
            TrailerInput::Source(raw) => {
                let provider = match raw.provider.trim() {
                    "youtube" => TrailerProvider::Youtube,
                    "rutube" => TrailerProvider::Rutube,
                    other => {
                        return Err(format!(
                            "metadata.trailers: unknown provider {other:?} (youtube, rutube)"
                        ))
                    }
                };
                let id = raw.id.trim();
                if provider.is_id(id) {
                    return Ok(Self {
                        provider,
                        id: id.to_string(),
                    });
                }
                parse_video_link(id)
                    .filter(|source| source.provider == provider)
                    .ok_or_else(|| {
                        format!("metadata.trailers: {id:?} is not a {provider:?} video id or link")
                    })
            }
        }
    }
}

impl TrailerProvider {
    fn is_id(self, id: &str) -> bool {
        match self {
            Self::Youtube => {
                id.len() == 11
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }
            Self::Rutube => {
                id.len() == 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
        }
    }
}

/// Поля, специфичные для типа (`kind`). Все поля необязательны, неизвестные запрещены.
/// Только для документации: разбирается по `kind` функцией [`validate`].
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum Metadata {
    Movie(MovieMetadata),
    Series(SeriesMetadata),
    Book(BookMetadata),
    Game(GameMetadata),
}

const MAX_TEXT: usize = 200;
const MAX_LIST: usize = 30;
const MAX_TRAILERS: usize = 5;

/// Проверяет `metadata` для `kind` и возвращает нормализованное значение для записи в БД.
pub fn validate(kind: EntityKind, metadata: Value) -> AppResult<Value> {
    if !metadata.is_object() {
        return Err(bad("metadata must be an object"));
    }
    match kind {
        EntityKind::Movie => {
            let mut m: MovieMetadata = parse(kind, metadata)?;
            range("runtime_min", m.runtime_min, 1, 10_000)?;
            m.countries = countries(m.countries)?;
            m.age_rating = text("age_rating", m.age_rating, 16)?;
            m.trailers = trailers(m.trailers)?;
            to_value(m)
        }
        EntityKind::Series => {
            let mut m: SeriesMetadata = parse(kind, metadata)?;
            range("seasons", m.seasons, 1, 10_000)?;
            range("episodes", m.episodes, 1, 100_000)?;
            m.countries = countries(m.countries)?;
            m.trailers = trailers(m.trailers)?;
            to_value(m)
        }
        EntityKind::Book => {
            let mut m: BookMetadata = parse(kind, metadata)?;
            m.isbn = m.isbn.map(|isbn| isbn_normalize(&isbn)).transpose()?;
            range("pages", m.pages, 1, 100_000)?;
            m.publisher = text("publisher", m.publisher, MAX_TEXT)?;
            to_value(m)
        }
        EntityKind::Game => {
            let mut m: GameMetadata = parse(kind, metadata)?;
            m.platforms = list("platforms", m.platforms, |p| {
                text("platforms", Some(p), 50).map(|p| p.unwrap_or_default())
            })?;
            m.developer = text("developer", m.developer, MAX_TEXT)?;
            m.publisher = text("publisher", m.publisher, MAX_TEXT)?;
            m.trailers = trailers(m.trailers)?;
            to_value(m)
        }
    }
}

fn bad(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}

fn parse<T: DeserializeOwned>(kind: EntityKind, metadata: Value) -> AppResult<T> {
    serde_json::from_value(metadata)
        .map_err(|e| bad(format!("invalid metadata for {}: {e}", kind.as_str())))
}

fn to_value<T: Serialize>(metadata: T) -> AppResult<Value> {
    serde_json::to_value(metadata).map_err(|e| AppError::Internal(e.to_string()))
}

fn range(field: &str, value: Option<u32>, min: u32, max: u32) -> AppResult<()> {
    match value {
        Some(v) if !(min..=max).contains(&v) => Err(bad(format!(
            "metadata.{field} must be between {min} and {max}"
        ))),
        _ => Ok(()),
    }
}

/// Обрезает пробелы; пустая строка — то же, что отсутствие поля.
fn text(field: &str, value: Option<String>, max: usize) -> AppResult<Option<String>> {
    let Some(value) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if value.chars().count() > max {
        return Err(bad(format!("metadata.{field} is longer than {max} chars")));
    }
    Ok(Some(value))
}

/// Список без дублей, не длиннее [`MAX_LIST`]. Пустой список — то же, что отсутствие поля.
fn list(
    field: &str,
    values: Option<Vec<String>>,
    item: impl Fn(String) -> AppResult<String>,
) -> AppResult<Option<Vec<String>>> {
    let Some(values) = values else {
        return Ok(None);
    };
    if values.len() > MAX_LIST {
        return Err(bad(format!(
            "metadata.{field} has more than {MAX_LIST} items"
        )));
    }
    let mut result: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let value = item(value)?;
        if value.is_empty() {
            return Err(bad(format!("metadata.{field} has an empty item")));
        }
        if !result.contains(&value) {
            result.push(value);
        }
    }
    Ok((!result.is_empty()).then_some(result))
}

fn countries(values: Option<Vec<String>>) -> AppResult<Option<Vec<String>>> {
    list("countries", values, |code| {
        let code = code.trim().to_ascii_uppercase();
        if code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase()) {
            Ok(code)
        } else {
            Err(bad(format!(
                "metadata.countries: {code:?} is not an ISO 3166-1 alpha-2 code"
            )))
        }
    })
}

/// Источники без повторов, порядок — приоритет. Пустой список — то же, что отсутствие поля.
fn trailers(values: Option<Vec<TrailerSource>>) -> AppResult<Option<Vec<TrailerSource>>> {
    let Some(values) = values else {
        return Ok(None);
    };
    if values.len() > MAX_TRAILERS {
        return Err(bad(format!(
            "metadata.trailers has more than {MAX_TRAILERS} items"
        )));
    }
    let mut result: Vec<TrailerSource> = Vec::with_capacity(values.len());
    for value in values {
        if !result.contains(&value) {
            result.push(value);
        }
    }
    Ok((!result.is_empty()).then_some(result))
}

/// Провайдер и id из ссылки: `youtube.com/watch?v=…`, `youtu.be/…`, `/embed/…`, `/shorts/…`,
/// `rutube.ru/video/…/`, `rutube.ru/play/embed/…`. Другие сайты — `None`.
fn parse_video_link(link: &str) -> Option<TrailerSource> {
    let rest = link
        .strip_prefix("https://")
        .or_else(|| link.strip_prefix("http://"))
        .unwrap_or(link);
    let rest = rest
        .strip_prefix("www.")
        .or_else(|| rest.strip_prefix("m."))
        .unwrap_or(rest);
    let first_segment = |path: &str| path.split(['?', '&', '/', '#']).next().map(str::to_string);

    let (provider, id) = if let Some(path) = rest.strip_prefix("youtu.be/") {
        (TrailerProvider::Youtube, first_segment(path)?)
    } else if let Some(path) = rest
        .strip_prefix("youtube.com/")
        .or_else(|| rest.strip_prefix("youtube-nocookie.com/"))
    {
        let id = if let Some(query) = path.strip_prefix("watch?") {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("v="))
                .and_then(|v| v.split('#').next())
                .map(str::to_string)?
        } else {
            ["embed/", "shorts/", "v/"]
                .iter()
                .find_map(|prefix| path.strip_prefix(prefix))
                .and_then(first_segment)?
        };
        (TrailerProvider::Youtube, id)
    } else {
        // Не YouTube — только Rutube, другие сайты отбрасываем.
        let path = rest.strip_prefix("rutube.ru/")?;
        let id = ["video/", "play/embed/", "shorts/"]
            .iter()
            .find_map(|prefix| path.strip_prefix(prefix))
            .and_then(first_segment)?;
        (TrailerProvider::Rutube, id)
    };
    provider
        .is_id(&id)
        .then_some(TrailerSource { provider, id })
}

/// Убирает дефисы и пробелы, проверяет длину и контрольную цифру ISBN-10/13.
fn isbn_normalize(isbn: &str) -> AppResult<String> {
    let isbn: String = isbn
        .chars()
        .filter(|c| *c != '-' && *c != ' ')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let invalid = || bad("metadata.isbn is not a valid ISBN-10 or ISBN-13");
    let digit = |c: char| c.to_digit(10).ok_or_else(invalid);
    let valid = match isbn.len() {
        10 => {
            let mut sum = 0;
            for (i, c) in isbn.chars().enumerate() {
                let value = if i == 9 && c == 'X' { 10 } else { digit(c)? };
                sum += value * (10 - i as u32);
            }
            sum % 11 == 0
        }
        13 => {
            let mut sum = 0;
            for (i, c) in isbn.chars().enumerate() {
                sum += digit(c)? * if i % 2 == 0 { 1 } else { 3 };
            }
            sum % 10 == 0
        }
        _ => false,
    };
    if valid {
        Ok(isbn)
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn movie_is_normalized() {
        let value = validate(
            EntityKind::Movie,
            json!({ "runtime_min": 155, "countries": ["us", "US", "ca"], "age_rating": " PG-13 " }),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({ "runtime_min": 155, "countries": ["US", "CA"], "age_rating": "PG-13" })
        );
    }

    #[test]
    fn empty_object_is_valid_for_every_kind() {
        for kind in EntityKind::ALL {
            assert_eq!(validate(kind, json!({})).unwrap(), json!({}));
        }
    }

    #[test]
    fn fields_of_another_kind_are_rejected() {
        let error = validate(EntityKind::Movie, json!({ "isbn": "9780441013593" })).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn wrong_types_and_ranges_are_rejected() {
        assert!(validate(EntityKind::Movie, json!({ "runtime_min": "long" })).is_err());
        assert!(validate(EntityKind::Movie, json!({ "runtime_min": 0 })).is_err());
        assert!(validate(EntityKind::Movie, json!({ "countries": ["USA"] })).is_err());
        assert!(validate(EntityKind::Series, json!({ "status": "paused" })).is_err());
        assert!(validate(EntityKind::Game, json!({ "platforms": [""] })).is_err());
        assert!(validate(EntityKind::Book, json!([])).is_err());
    }

    #[test]
    fn trailers_accept_links_and_objects_and_keep_order() {
        let yt = "n9xhJrPXop4";
        let rt = "0ab1fc1e47f2e9b89e9e59d9db36f2b4";
        let expected = json!({ "trailers": [
            { "provider": "youtube", "id": yt },
            { "provider": "rutube", "id": rt }
        ] });
        for input in [
            json!([
                format!("https://www.youtube.com/watch?feature=share&v={yt}&t=30"),
                format!("https://rutube.ru/video/{rt}/?r=wd")
            ]),
            json!([{ "provider": "youtube", "id": yt }, { "provider": "rutube", "id": rt }]),
            json!([{ "provider": "youtube", "id": format!("https://youtu.be/{yt}?si=x") },
                   format!("https://rutube.ru/play/embed/{rt}"),
                   format!("https://www.youtube-nocookie.com/embed/{yt}")]),
        ] {
            for kind in [EntityKind::Movie, EntityKind::Series, EntityKind::Game] {
                let value = validate(kind, json!({ "trailers": input })).unwrap();
                assert_eq!(value, expected, "{kind:?} {input}");
            }
        }
        assert_eq!(
            validate(EntityKind::Movie, json!({ "trailers": [] })).unwrap(),
            json!({})
        );
    }

    #[test]
    fn trailers_reject_other_sites_wrong_ids_and_books() {
        let bad = |kind, trailers: Value| validate(kind, json!({ "trailers": trailers })).is_err();
        assert!(bad(
            EntityKind::Movie,
            json!(["https://vimeo.com/12345678"])
        ));
        assert!(bad(
            EntityKind::Movie,
            json!(["https://evil.example/watch?v=n9xhJrPXop4"])
        ));
        assert!(bad(
            EntityKind::Movie,
            json!([{ "provider": "vimeo", "id": "1" }])
        ));
        assert!(bad(
            EntityKind::Movie,
            json!([{ "provider": "rutube", "id": "n9xhJrPXop4" }])
        ));
        assert!(bad(
            EntityKind::Movie,
            json!([{ "provider": "youtube", "id": "n9xhJrPXop4", "extra": 1 }])
        ));
        assert!(bad(
            EntityKind::Movie,
            json!(vec!["https://youtu.be/n9xhJrPXop4"; 6])
        ));
        assert!(bad(EntityKind::Movie, json!("n9xhJrPXop4")));
        assert!(bad(
            EntityKind::Book,
            json!(["https://youtu.be/n9xhJrPXop4"])
        ));
    }

    #[test]
    fn isbn_checksum() {
        let ok = |isbn: &str| validate(EntityKind::Book, json!({ "isbn": isbn }));
        assert_eq!(
            ok("978-0-441-01359-3").unwrap(),
            json!({ "isbn": "9780441013593" })
        );
        assert_eq!(
            ok("0-441-01359-7").unwrap(),
            json!({ "isbn": "0441013597" })
        );
        assert_eq!(ok("080442957x").unwrap(), json!({ "isbn": "080442957X" }));
        assert!(ok("9780441013594").is_err());
        assert!(ok("12345").is_err());
    }
}
