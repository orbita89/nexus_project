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
            to_value(m)
        }
        EntityKind::Series => {
            let mut m: SeriesMetadata = parse(kind, metadata)?;
            range("seasons", m.seasons, 1, 10_000)?;
            range("episodes", m.episodes, 1, 100_000)?;
            m.countries = countries(m.countries)?;
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
