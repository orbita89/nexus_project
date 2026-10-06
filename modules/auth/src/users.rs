//! Запросы к `users`, нужные нескольким хендлерам.

use crate::models::{UserView, USER_COLUMNS};
use crate::validate;
use shared::{AppError, AppResult};
use sqlx::PgConnection;
use uuid::Uuid;

/// Пользователь вместе с хешем пароля — только для проверки пароля, наружу не отдаётся.
#[derive(sqlx::FromRow)]
pub struct UserWithHash {
    #[sqlx(flatten)]
    pub user: UserView,
    pub password_hash: Option<String>,
}

pub async fn by_id(conn: &mut PgConnection, id: Uuid) -> AppResult<Option<UserView>> {
    Ok(
        sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// `= $1::citext` — сравнение без учёта регистра; без приведения сравнивались бы как text.
pub async fn by_email(conn: &mut PgConnection, email: &str) -> AppResult<Option<UserView>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS} FROM users WHERE email = $1::citext"
    ))
    .bind(email.trim())
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn by_login_with_hash(
    conn: &mut PgConnection,
    login: &str,
) -> AppResult<Option<UserWithHash>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS}, password_hash FROM users
         WHERE email = $1::citext OR username = $1::citext"
    ))
    .bind(login.trim())
    .fetch_optional(&mut *conn)
    .await?)
}

/// Нарушение уникальности email или username → 409 с понятным сообщением.
pub fn unique_violation(error: sqlx::Error) -> AppError {
    if let Some(db_error) = error.as_database_error() {
        if db_error.is_unique_violation() {
            let message = match db_error.constraint() {
                Some("users_email_key") => "email already registered",
                Some("users_username_key") => "username already taken",
                _ => "user already exists",
            };
            return AppError::Conflict(message.to_string());
        }
    }
    AppError::Database(error)
}

/// Отмечает email подтверждённым (если ещё не был) и возвращает пользователя.
pub async fn mark_email_verified(conn: &mut PgConnection, id: Uuid) -> AppResult<UserView> {
    Ok(sqlx::query_as(&format!(
        "UPDATE users SET email_verified_at = COALESCE(email_verified_at, now())
         WHERE id = $1
         RETURNING {USER_COLUMNS}"
    ))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Пользователь, пришедший по ссылке из письма: без пароля, email сразу подтверждён.
pub async fn create_from_email(conn: &mut PgConnection, email: &str) -> AppResult<UserView> {
    create_external(conn, email, true, None, None).await
}

/// Пользователь без пароля (вход по ссылке, через OAuth-провайдера, dev login).
/// username генерируется из email, сменить его можно будет в профиле.
pub async fn create_external(
    conn: &mut PgConnection,
    email: &str,
    email_verified: bool,
    display_name: Option<&str>,
    avatar_url: Option<&str>,
) -> AppResult<UserView> {
    let display_name = display_name.map(|name| name.chars().take(64).collect::<String>());
    Ok(sqlx::query_as(&format!(
        "INSERT INTO users (email, username, display_name, avatar_url, email_verified_at)
         VALUES ($1, $2, $3, $4, CASE WHEN $5 THEN now() END)
         RETURNING {USER_COLUMNS}"
    ))
    .bind(email)
    .bind(username_from_email(email))
    .bind(display_name)
    .bind(avatar_url)
    .bind(email_verified)
    .fetch_one(&mut *conn)
    .await?)
}

/// `Neo.Anderson+tag@matrix.io` → `neo.anderson_3f9a1c`. Суффикс исключает совпадения.
fn username_from_email(email: &str) -> String {
    let local = email.split('@').next().unwrap_or_default();
    let base: String = local
        .split('+')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| validate::is_username_char(*c))
        .take(24)
        .collect::<String>()
        .to_lowercase();
    let base = if base.is_empty() {
        "user".to_string()
    } else {
        base
    };
    let suffix = &Uuid::new_v4().simple().to_string()[..6];
    format!("{base}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::username_from_email;
    use crate::validate;

    #[test]
    fn generated_username_is_valid() {
        for email in [
            "Neo.Anderson+tag@matrix.io",
            "кириллица@почта.рф",
            "a@b.co",
            &format!("{}@b.co", "x".repeat(100)),
        ] {
            let username = username_from_email(email);
            assert!(
                validate::username(&username).is_ok(),
                "{email} -> {username}"
            );
        }
        assert!(username_from_email("Neo.Anderson+tag@matrix.io").starts_with("neo.anderson_"));
    }
}
