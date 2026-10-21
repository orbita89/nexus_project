//! Читаемый адрес особой коллекции: `/collections/russian-top-2021`.
//!
//! Формат `^[a-z0-9]+(-[a-z0-9]+)*$`, 3–100 символов; не UUID (иначе спутается с
//! `/collections/{id}`) и не служебное слово ([`RESERVED`]). Транслит — по правилам каталога
//! («Русский топ 2021» → `russkiy-top-2021`), буквы других алфавитов отбрасываются.

use shared::AppError;
use uuid::Uuid;

pub const MIN_SLUG: usize = 3;
pub const MAX_SLUG: usize = 100;

/// Служебные адреса раздела `/collections/...` и фронтенда.
pub const RESERVED: &[&str] = &[
    "featured", "new", "by-slug", "admin", "create", "edit", "search",
];

/// Проверить slug от admin: 400 `validation` с причиной.
pub fn validate(slug: &str) -> Result<String, AppError> {
    let bad = |why: &str| Err(AppError::BadRequest(format!("slug: {why}")));
    let well_formed = slug.split('-').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    });
    if !well_formed {
        return bad("only a-z, 0-9 and single dashes between them");
    }
    if !(MIN_SLUG..=MAX_SLUG).contains(&slug.len()) {
        return bad(&format!("must be {MIN_SLUG}-{MAX_SLUG} chars"));
    }
    if Uuid::parse_str(slug).is_ok() {
        return bad("must not look like a UUID");
    }
    if RESERVED.contains(&slug) {
        return bad("reserved word");
    }
    Ok(slug.to_string())
}

/// Основа slug из названия: транслит, всё прочее — дефис. Короткая или пустая — дополняется
/// словом `collection`; длинная обрезается по границе слова (место под суффикс `-N`).
pub fn from_title(title: &str) -> String {
    let mut out = String::new();
    for c in title.to_lowercase().chars() {
        match translit(c) {
            Some(s) => out.push_str(s),
            None if c.is_ascii_alphanumeric() => out.push(c),
            None => out.push('-'),
        }
    }
    let mut slug = out
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.len() > MAX_SLUG - 4 {
        slug.truncate(MAX_SLUG - 4);
        if let Some(cut) = slug.rfind('-') {
            slug.truncate(cut);
        }
    }
    if slug.len() < MIN_SLUG || RESERVED.contains(&slug.as_str()) || Uuid::parse_str(&slug).is_ok()
    {
        slug = if slug.is_empty() {
            "collection".into()
        } else {
            format!("{slug}-collection")
        };
    }
    slug
}

fn translit(c: char) -> Option<&'static str> {
    Some(match c {
        'а' => "a",
        'б' => "b",
        'в' => "v",
        'г' => "g",
        'д' => "d",
        'е' | 'ё' => "e",
        'ж' => "zh",
        'з' => "z",
        'и' => "i",
        'й' => "y",
        'к' => "k",
        'л' => "l",
        'м' => "m",
        'н' => "n",
        'о' => "o",
        'п' => "p",
        'р' => "r",
        'с' => "s",
        'т' => "t",
        'у' => "u",
        'ф' => "f",
        'х' => "kh",
        'ц' => "ts",
        'ч' => "ch",
        'ш' => "sh",
        'щ' => "shch",
        'ъ' | 'ь' => "",
        'ы' => "y",
        'э' => "e",
        'ю' => "yu",
        'я' => "ya",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(from_title("Русский топ 2021"), "russkiy-top-2021");
        assert_eq!(from_title("  Top-10: «Дюна» & Co!  "), "top-10-dyuna-co");
        assert_eq!(from_title("Ёлки"), "elki");
        assert_eq!(from_title("??"), "collection");
        assert_eq!(from_title("Ab"), "ab-collection");
        assert_eq!(from_title("New"), "new-collection");
        let long = from_title(&"слово ".repeat(40));
        assert!(long.len() <= MAX_SLUG - 4 && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn validation() {
        assert_eq!(validate("russian-top-2021").unwrap(), "russian-top-2021");
        for bad in [
            "ab",
            "Top",
            "a--b",
            "-ab",
            "ab-",
            "a b",
            "топ",
            "featured",
            "by-slug",
            "0f8fad5b-d9cb-469f-a165-70867728950e",
        ] {
            assert!(validate(bad).is_err(), "{bad}");
        }
        assert!(validate(&"a".repeat(101)).is_err());
    }
}
