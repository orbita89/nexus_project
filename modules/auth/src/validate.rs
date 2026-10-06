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
    }
}
