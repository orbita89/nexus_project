//! Сессии: выдача пары токенов и работа с `refresh_tokens`.
//! Одна строка — одна выдача refresh-токена. При обновлении старая строка отзывается,
//! новая создаётся, поэтому id сессии меняется вместе с токеном.

use crate::crypto;
use crate::models::{TokenResponse, UserView};
use crate::rate_limit::client_ip;
use axum::http::{header, HeaderMap};
use shared::auth::ACCESS_TOKEN_TTL_SECS;
use shared::{AppResult, AppState};
use sqlx::PgConnection;
use uuid::Uuid;

pub const REFRESH_TOKEN_TTL_DAYS: i32 = 30;
/// Сколько секунд после замены повтор старого refresh-токена не считается кражей.
pub const REFRESH_REUSE_GRACE_SECS: i64 = 30;

/// Откуда пришёл запрос — для списка активных сессий.
pub struct ClientInfo {
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

impl ClientInfo {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(|ua| ua.chars().take(512).collect()),
            ip: client_ip(headers),
        }
    }
}

/// Создаёт сессию и выдаёт пару токенов.
pub async fn issue(
    state: &AppState,
    conn: &mut PgConnection,
    user: UserView,
    client: &ClientInfo,
) -> AppResult<TokenResponse> {
    let refresh_token = crypto::generate_token();
    let session_id: Uuid = sqlx::query_scalar(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at, user_agent, ip)
         VALUES ($1, $2, now() + make_interval(days => $3), $4, $5::inet)
         RETURNING id",
    )
    .bind(user.id)
    .bind(crypto::hash_token(&refresh_token))
    .bind(REFRESH_TOKEN_TTL_DAYS)
    .bind(&client.user_agent)
    .bind(&client.ip)
    .fetch_one(&mut *conn)
    .await?;

    Ok(TokenResponse {
        access_token: state.jwt.issue(user.id, user.role, session_id)?,
        token_type: "Bearer".to_string(),
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token,
        user,
    })
}

/// Отзывает все активные сессии пользователя, кроме `except` (обычно текущей).
pub async fn revoke_all(
    conn: &mut PgConnection,
    user_id: Uuid,
    except: Option<Uuid>,
) -> AppResult<u64> {
    let result = sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE user_id = $1 AND revoked_at IS NULL AND id IS DISTINCT FROM $2",
    )
    .bind(user_id)
    .bind(except)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}
