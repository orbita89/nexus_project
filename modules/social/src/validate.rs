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
pub const MAX_REPORT_REASON: usize = 500;

/// Длина эмодзи в байтах: «👩🏽‍🚀» — 15, флаги с тегами («🏴󠁧󠁢󠁳󠁣󠁴󠁿») — 28.
pub const MAX_EMOJI_BYTES: usize = 64;
/// Разных эмодзи у одного произведения: дальше строка реакций превращается в ленту.
pub const MAX_ENTITY_EMOJI: i64 = 30;

fn bad(message: String) -> AppError {
    AppError::BadRequest(message)
}

/// Одно эмодзи — ровно одна графемная последовательность: база плюс модификаторы тона,
/// селекторы начертания и ZWJ-склейка («👩‍👩‍👧», «👍🏽»), клавиши («1️⃣») и флаги («🇷🇺», «🏴󠁧󠁢󠁥󠁮󠁧󠁿»).
///
/// Своя проверка, а не библиотека: нужно отличить эмодзи от любого другого текста, иначе поле
/// станет свалкой, — а для этого достаточно разобрать последовательность по диапазонам Unicode.
/// Буквы, цифры (кроме клавиш), пробелы и разметка не проходят.
pub fn emoji(value: &str) -> Result<String, AppError> {
    let bad_emoji = || {
        AppError::BadRequest("emoji must be a single emoji sequence".into()).with_code("bad_emoji")
    };
    if value.is_empty() || value.len() > MAX_EMOJI_BYTES {
        return Err(bad_emoji());
    }
    // Последовательность — куски, склеенные ZWJ (U+200D); каждый должен быть эмодзи.
    let mut parts = value.split('\u{200D}');
    let ok = parts.all(emoji_part) && !value.ends_with('\u{200D}');
    if !ok {
        return Err(bad_emoji());
    }
    Ok(value.to_string())
}

/// Кусок последовательности между ZWJ: база и то, что к ней цепляется.
fn emoji_part(part: &str) -> bool {
    let mut chars = part.chars().peekable();
    let Some(first) = chars.next() else {
        return false;
    };
    // Клавиша: цифра или решётка, селектор начертания и U+20E3 — только так цифра считается
    // эмодзи, сама по себе «1» реакцией быть не должна.
    if first.is_ascii_digit() || first == '#' || first == '*' {
        let rest: String = chars.collect();
        return rest == "\u{FE0F}\u{20E3}" || rest == "\u{20E3}";
    }
    // Флаг страны — пара региональных индикаторов, больше ничего в куске быть не может.
    if is_regional(first) {
        return chars.next().is_some_and(is_regional) && chars.next().is_none();
    }
    if !is_emoji_base(first) {
        return false;
    }
    // Хвост: тон кожи, селекторы начертания и теги флагов («🏴󠁧󠁢󠁳󠁣󠁴󠁿» — чёрный флаг и теги до U+E007F).
    chars.all(|ch| {
        matches!(ch, '\u{1F3FB}'..='\u{1F3FF}' | '\u{FE0E}' | '\u{FE0F}' | '\u{E0020}'..='\u{E007F}')
    })
}

fn is_regional(ch: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch)
}

/// База эмодзи: блоки пиктограмм и отдельные символы старых блоков, которым нужен U+FE0F
/// («❤️», «☀️», «✌️»). Список диапазонов, а не свойство Unicode: тянуть ради одной проверки
/// таблицу свойств незачем, а новые эмодзи приходят в те же блоки.
fn is_emoji_base(ch: char) -> bool {
    matches!(ch,
        '\u{00A9}' | '\u{00AE}'                     // © ®
        | '\u{203C}' | '\u{2049}'                   // ‼ ⁉
        | '\u{2122}' | '\u{2139}'                   // ™ ℹ
        | '\u{2194}'..='\u{21AA}'                   // стрелки
        | '\u{231A}'..='\u{231B}' | '\u{2328}'
        | '\u{23CF}' | '\u{23E9}'..='\u{23FA}'
        | '\u{24C2}' | '\u{25AA}'..='\u{25AB}'
        | '\u{25B6}' | '\u{25C0}' | '\u{25FB}'..='\u{25FE}'
        | '\u{2600}'..='\u{27BF}'                   // погода, знаки, ✅ ✌ ❤
        | '\u{2934}'..='\u{2935}' | '\u{2B00}'..='\u{2BFF}'
        | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}'
        | '\u{1F000}'..='\u{1FAFF}'                 // основные блоки пиктограмм (🀄, 🃏, 😀, 🫠)
    )
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
    fn emojis() {
        for value in [
            "🔥",
            "❤️",
            "👍🏽",
            "👩‍👩‍👧",
            "👩🏽‍🚀",
            "🇷🇺",
            "1️⃣",
            "#️⃣",
            "🏴󠁧󠁢󠁳󠁣󠁴󠁿",
            "🤯",
            "😴",
        ] {
            assert!(emoji(value).is_ok(), "{value} должен быть эмодзи");
        }
        for value in [
            "",
            " ",
            "a",
            "1",
            "фильм",
            "🔥🔥",
            "🔥 ",
            "<b>",
            "🔥\u{200D}",
            "🇷🇺🇷🇺",
            ":fire:",
            "\u{200D}",
            "🔥a",
        ] {
            assert!(emoji(value).is_err(), "{value:?} не эмодзи");
        }
        assert!(emoji(&"🔥".repeat(20)).is_err());
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
