//! Проверка входных данных админских эндпоинтов и перевод ошибок БД в ответы API.

use shared::AppError;

pub const MAX_SLUG: usize = 100;
pub const MAX_TITLE: usize = 300;
pub const MAX_LONG_TEXT: usize = 20_000;
pub const MAX_URL: usize = 2048;

fn bad(message: String) -> AppError {
    AppError::BadRequest(message)
}

/// `dune-2021`: латиница в нижнем регистре, цифры, одиночные дефисы между ними.
pub fn slug(field: &str, value: &str) -> Result<(), AppError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_SLUG
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--");
    if ok {
        Ok(())
    } else {
        Err(bad(format!(
            "{field} must be 1-{MAX_SLUG} chars of a-z, 0-9 and single hyphens"
        )))
    }
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

/// Необязательный URL картинки: только `http(s)://`.
pub fn url(field: &str, value: Option<String>) -> Result<Option<String>, AppError> {
    let value = optional(field, value, MAX_URL)?;
    if let Some(url) = &value {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"));
        if rest.is_none_or(|rest| rest.is_empty() || rest.contains(char::is_whitespace)) {
            return Err(bad(format!("{field} must be an http(s) URL")));
        }
    }
    Ok(value)
}

/// Наибольший размер файла обложки в пикселях.
pub const MAX_COVER_PX: i32 = 100_000;

/// Размеры обложки в пикселях: только вдвоём и только вместе с самой картинкой. Размеры от
/// прежнего файла дали бы карточке чужое `aspect-ratio` — это хуже, чем их отсутствие.
pub fn cover_size(
    has_cover: bool,
    width: Option<i32>,
    height: Option<i32>,
) -> Result<(Option<i32>, Option<i32>), AppError> {
    let (width, height) = match (width, height) {
        (None, None) => return Ok((None, None)),
        (Some(width), Some(height)) => (width, height),
        _ => {
            return Err(bad(
                "cover_width and cover_height must be given together".into()
            ))
        }
    };
    if !has_cover {
        return Err(bad(
            "cover_width and cover_height are only accepted with cover_url".into(),
        ));
    }
    for (field, value) in [("cover_width", width), ("cover_height", height)] {
        if !(1..=MAX_COVER_PX).contains(&value) {
            return Err(bad(format!("{field} must be 1-{MAX_COVER_PX}")));
        }
    }
    Ok((Some(width), Some(height)))
}

/// Роль в титрах: `actor`, `voice_actor`, ...
pub fn role(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    let ok = (1..=32).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        && !value.starts_with('_');
    if ok {
        Ok(value.to_string())
    } else {
        Err(bad("role must be 1-32 chars of a-z and _".into()))
    }
}

/// Коды стран ISO 3166-1 alpha-2: все назначенные плюс несколько исторических и
/// исключительно зарезервированных, которые приходят из TMDB и Wikidata (`SU` — СССР, `CS` —
/// Сербия и Черногория, `YU` — Югославия, `DD` — ГДР, `AN` — Нидерландские Антилы, `XK` —
/// Косово). Одной строкой через пробел: список статический, перебор 255 значений дешевле карты.
const COUNTRY_CODES: &str = "\
AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ \
BA BB BD BE BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ \
CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ \
DE DJ DK DM DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR \
GA GB GD GE GF GG GH GI GL GM GN GP GQ GR GS GT GU GW GY \
HK HM HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP \
KE KG KH KI KM KN KP KR KW KY KZ LA LB LC LI LK LR LS LT LU LV LY \
MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ \
NA NC NE NF NG NI NL NO NP NR NU NZ OM \
PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW \
SA SB SC SD SE SG SH SI SJ SK SL SM SN SO SR SS ST SV SX SY SZ \
TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ \
VA VC VE VG VI VN VU WF WS YE YT ZA ZM ZW \
AN CS DD SU XK";

/// Код страны ISO 3166-1 alpha-2 (`US`, `gb` → `GB`). Неизвестный — 400, чтобы мусор в адресе не
/// выдавал пустой результат молча.
pub fn country(field: &str, value: &str) -> Result<String, AppError> {
    let code = value.trim().to_ascii_uppercase();
    let known = code.len() == 2 && COUNTRY_CODES.split(' ').any(|known| known == code.as_str());
    if known {
        Ok(code)
    } else {
        Err(bad(format!(
            "{field} must be an ISO 3166-1 alpha-2 code (got {value:?})"
        )))
    }
}

/// Нарушение UNIQUE → 409 с понятным текстом, остальное — как есть (500, детали в лог).
pub fn conflict(message: &'static str) -> impl Fn(sqlx::Error) -> AppError {
    move |error| match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => AppError::Conflict(message.into()),
        _ => AppError::Database(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for ok in ["dune-2021", "a", "the-witcher-3"] {
            assert!(slug("slug", ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Dune",
            "дюна",
            "-dune",
            "dune-",
            "du--ne",
            "dune 2021",
            "dune_2021",
        ] {
            assert!(slug("slug", bad).is_err(), "{bad}");
        }
        assert!(slug("slug", &"a".repeat(MAX_SLUG + 1)).is_err());
    }

    #[test]
    fn strings() {
        assert_eq!(required("t", "  Дюна ", 10).unwrap(), "Дюна");
        assert!(required("t", "   ", 10).is_err());
        assert!(required("t", "абвгд", 4).is_err());
        assert_eq!(optional("t", Some("  ".into()), 10).unwrap(), None);
    }

    #[test]
    fn urls() {
        assert!(url("u", Some("https://img.example/x.jpg".into())).is_ok());
        assert_eq!(url("u", Some(" ".into())).unwrap(), None);
        for bad in ["ftp://x", "javascript:alert(1)", "https://", "https://a b"] {
            assert!(url("u", Some(bad.into())).is_err(), "{bad}");
        }
    }

    #[test]
    fn roles() {
        assert_eq!(role(" voice_actor ").unwrap(), "voice_actor");
        assert!(role("Actor").is_err());
        assert!(role("").is_err());
        assert!(role("_x").is_err());
    }
}
