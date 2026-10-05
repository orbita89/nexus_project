//! Сессии: выдача пары токенов и работа с `refresh_tokens`.

use crate::models::{TokenResponse, UserView};
use sha2::{Digest, Sha256};
use shared::auth::ACCESS_TOKEN_TTL_SECS;
use shared::{AppResult, AppState};
use sqlx::PgConnection;
use uuid::Uuid;

pub const REFRESH_TOKEN_TTL_DAYS: i64 = 30;

/// Откуда пришёл запрос — для списка активных сессий.
#[derive(Default)]
pub struct ClientInfo {
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

/// 244 случайных бита из ОС (две UUID v4) — подобрать перебором нереально.
fn generate_refresh_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// В БД кладём SHA-256 от токена: утечка таблицы не даёт войти. Медленный хеш (как для паролей)
/// не нужен: у токена высокая энтропия, перебор бессмыслен.
pub fn hash_refresh_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Создаёт сессию и выдаёт пару токенов.
pub async fn issue(
    state: &AppState,
    conn: &mut PgConnection,
    user: UserView,
    client: &ClientInfo,
) -> AppResult<TokenResponse> {
    let refresh_token = generate_refresh_token();
    sqlx::query(
        "INSERT INTO refresh_tokens (user_id, token_hash, expires_at, user_agent, ip)
         VALUES ($1, $2, now() + make_interval(days => $3), $4, $5::inet)",
    )
    .bind(user.id)
    .bind(hash_refresh_token(&refresh_token))
    .bind(REFRESH_TOKEN_TTL_DAYS as i32)
    .bind(&client.user_agent)
    .bind(&client.ip)
    .execute(&mut *conn)
    .await?;

    Ok(TokenResponse {
        access_token: state.jwt.issue(user.id, user.role)?,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token,
        user,
    })
}
