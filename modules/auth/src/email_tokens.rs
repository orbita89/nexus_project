//! Одноразовые токены для ссылок из писем (таблица `email_tokens`).

use crate::crypto;
use shared::{AppError, AppResult};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, sqlx::Type)]
#[sqlx(type_name = "email_token_purpose", rename_all = "snake_case")]
pub enum Purpose {
    VerifyEmail,
    Login,
    ResetPassword,
    /// Код возврата с OAuth-провайдера на фронтенд (не письмо, но та же механика).
    #[sqlx(rename = "oauth_login")]
    OAuthLogin,
    /// Подтверждение нового адреса при смене email (письмо уходит на новый адрес).
    ChangeEmail,
}

impl Purpose {
    /// Срок жизни ссылки.
    fn ttl_minutes(self) -> i32 {
        match self {
            Self::VerifyEmail => 24 * 60,
            Self::Login => 15,
            Self::ResetPassword => 60,
            Self::OAuthLogin => 2,
            Self::ChangeEmail => 60,
        }
    }
}

/// Создаёт токен. Предыдущие неиспользованные токены того же назначения на этот адрес
/// гасятся: работает только ссылка из последнего письма.
pub async fn create(
    conn: &mut PgConnection,
    purpose: Purpose,
    email: &str,
    user_id: Option<Uuid>,
) -> AppResult<String> {
    sqlx::query(
        "UPDATE email_tokens SET used_at = now()
         WHERE purpose = $1 AND email = $2::citext AND used_at IS NULL",
    )
    .bind(purpose)
    .bind(email)
    .execute(&mut *conn)
    .await?;

    let token = crypto::generate_token();
    sqlx::query(
        "INSERT INTO email_tokens (purpose, email, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(mins => $5))",
    )
    .bind(purpose)
    .bind(email)
    .bind(user_id)
    .bind(crypto::hash_token(&token))
    .bind(purpose.ttl_minutes())
    .execute(&mut *conn)
    .await?;
    Ok(token)
}

pub struct Consumed {
    pub email: String,
    pub user_id: Option<Uuid>,
}

/// Погашает токен. Неизвестный, использованный, просроченный или чужого назначения — 400.
pub async fn consume(
    conn: &mut PgConnection,
    purpose: Purpose,
    token: &str,
) -> AppResult<Consumed> {
    let row: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "UPDATE email_tokens SET used_at = now()
         WHERE token_hash = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > now()
         RETURNING email::text, user_id",
    )
    .bind(crypto::hash_token(token.trim()))
    .bind(purpose)
    .fetch_optional(&mut *conn)
    .await?;

    row.map(|(email, user_id)| Consumed { email, user_id })
        .ok_or_else(|| AppError::BadRequest("invalid or expired token".to_string()))
}
