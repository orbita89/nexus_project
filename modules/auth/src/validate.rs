//! Проверка полей. Ошибка — 400 с понятным сообщением.

use shared::{AppError, AppResult};

fn bad(message: &str) -> AppResult<()> {
    Err(AppError::BadRequest(message.to_string()))
}

pub fn email(email: &str) -> AppResult<()> {
    let ok = email.len() <= 254
        && !email.contains(char::is_whitespace)
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
    if ok {
        Ok(())
    } else {
        bad("invalid email")
    }
}

pub fn username(username: &str) -> AppResult<()> {
    let ok = (3..=32).contains(&username.chars().count()) && username.chars().all(is_username_char);
    if ok {
        Ok(())
    } else {
        bad("username must be 3-32 characters: latin letters, digits, '_', '-', '.'")
    }
}

pub fn is_username_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

pub fn password(password: &str) -> AppResult<()> {
    if (8..=128).contains(&password.chars().count()) {
        Ok(())
    } else {
        bad("password must be 8-128 characters")
    }
}

/// Аватар — ссылка на картинку: только `https://`, до 500 символов.
pub fn avatar_url(url: Option<&str>) -> AppResult<()> {
    match url {
        Some(url)
            if url.chars().count() > 500
                || !url.starts_with("https://")
                || url.len() <= "https://".len()
                || url.contains(char::is_whitespace) =>
        {
            bad("avatar_url must be an https:// link up to 500 characters")
        }
        _ => Ok(()),
    }
}

pub fn display_name(name: Option<&str>) -> AppResult<()> {
    if name.is_some_and(|name| name.chars().count() > 64) {
        bad("display_name must be at most 64 characters")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_values() {
        assert!(email("a@b.co").is_ok());
        assert!(username("neo_1.x-y").is_ok());
        assert!(password("password123").is_ok());
        assert!(display_name(Some("Neo")).is_ok());
        assert!(avatar_url(Some("https://example.com/a.png")).is_ok());
        assert!(avatar_url(None).is_ok());
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(email("no-at-sign").is_err());
        assert!(email("a@localhost").is_err());
        assert!(email("a b@c.de").is_err());
        assert!(username("ne").is_err());
        assert!(username("нео").is_err());
        assert!(password("short").is_err());
        assert!(display_name(Some(&"x".repeat(65))).is_err());
        assert!(avatar_url(Some("http://example.com/a.png")).is_err());
        assert!(avatar_url(Some("https://")).is_err());
        assert!(avatar_url(Some("javascript:alert(1)")).is_err());
        assert!(avatar_url(Some(&format!("https://{}", "x".repeat(500)))).is_err());
    }
}
