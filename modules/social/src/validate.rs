//! Проверка входных данных и перевод ошибок БД в ответы API.

use shared::AppError;

pub const MAX_REVIEW_BODY: usize = 10_000;
pub const MAX_COLLECTION_TITLE: usize = 200;
pub const MAX_COLLECTION_DESCRIPTION: usize = 2_000;
pub const MAX_NOTE: usize = 1_000;
pub const MAX_COLLECTION_ITEMS: i64 = 500;
pub const MAX_THREAD_TITLE: usize = 200;
pub const MAX_THREAD_BODY: usize = 20_000;
pub const MAX_POST_BODY: usize = 10_000;
pub const MAX_THREAD_ENTITIES: usize = 10;
pub const MAX_INTERESTS: i64 = 500;
pub const MAX_COMMENT_BODY: usize = 2_000;
pub const MAX_CHAT_BODY: usize = 2_000;
pub const MAX_ROOM_SLUG: usize = 80;
pub const MAX_ROOM_TITLE: usize = 100;
pub const MAX_ROOM_DESCRIPTION: usize = 500;

fn bad(message: String) -> AppError {
    AppError::BadRequest(message)
}

/// Обязательная строка: обрезает пробелы, не пустая, не длиннее `max`.
pub fn required(field: &str, value: &str, max: usize) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(bad(format!("{field} must not be empty")));
    }
    if value.chars().count() > max {
        return Err(bad(format!("{field} is longer than {max} chars")));
    }
    Ok(value.to_string())
}

/// slug своей комнаты чата: строчные латинские буквы, цифры и дефисы между ними, 1–80 символов.
pub fn room_slug(value: &str) -> Result<String, AppError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_ROOM_SLUG
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if !ok {
        return Err(bad(format!(
            "slug must be 1-{MAX_ROOM_SLUG} chars: a-z, 0-9 and single dashes between them"
        )));
    }
    Ok(value.to_string())
}

/// Необязательная строка: пустая после обрезки — `None`.
pub fn optional(
    field: &str,
    value: Option<String>,
    max: usize,
) -> Result<Option<String>, AppError> {
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => required(field, value, max).map(Some),
    }
}

pub fn rating(value: Option<i16>) -> Result<Option<i16>, AppError> {
    match value {
        Some(rating) if !(1..=10).contains(&rating) => {
            Err(bad("rating must be between 1 and 10".into()))
        }
        _ => Ok(value),
    }
}

/// Нарушение внешнего ключа: сущность или пользователь удалены между проверкой и записью → 404.
pub fn missing_reference(error: sqlx::Error) -> AppError {
    match &error {
        sqlx::Error::Database(db) if db.is_foreign_key_violation() => AppError::NotFound,
        _ => AppError::Database(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        assert_eq!(required("t", "  Дюна ", 10).unwrap(), "Дюна");
        assert!(required("t", "   ", 10).is_err());
        assert!(required("t", "абвгд", 4).is_err());
        assert_eq!(optional("t", Some("  ".into()), 10).unwrap(), None);
        assert_eq!(optional("t", None, 10).unwrap(), None);
    }

    #[test]
    fn ratings() {
        assert_eq!(rating(None).unwrap(), None);
        assert_eq!(rating(Some(1)).unwrap(), Some(1));
        assert_eq!(rating(Some(10)).unwrap(), Some(10));
        assert!(rating(Some(0)).is_err());
        assert!(rating(Some(11)).is_err());
        assert!(rating(Some(-3)).is_err());
    }
}
