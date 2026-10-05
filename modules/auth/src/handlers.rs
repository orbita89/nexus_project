//! Регистрация, вход, обновление токенов, выход, профиль.

use crate::models::{
    LoginRequest, RefreshRequest, RegisterRequest, TokenResponse, UserView, USER_COLUMNS,
};
use crate::password;
use crate::session::{self, ClientInfo};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use chrono::{DateTime, Utc};
use shared::{AppError, AppResult, AppState, AuthUser};
use std::net::IpAddr;
use uuid::Uuid;

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> AppResult<(StatusCode, Json<TokenResponse>)> {
    let email = req.email.trim().to_string();
    let username = req.username.trim().to_string();
    let display_name = req
        .display_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    validate_registration(&email, &username, &req.password, display_name.as_deref())?;

    let password_hash = password::hash(req.password).await?;

    let mut tx = state.db.begin().await?;
    let user: UserView = sqlx::query_as(&format!(
        "INSERT INTO users (email, username, password_hash, display_name)
         VALUES ($1, $2, $3, $4)
         RETURNING {USER_COLUMNS}"
    ))
    .bind(&email)
    .bind(&username)
    .bind(&password_hash)
    .bind(&display_name)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_unique_violation)?;

    let tokens = session::issue(&state, &mut tx, user, &client_info(&headers)).await?;
    tx.commit().await?;

    tracing::info!(user_id = %tokens.user.id, "user registered");
    Ok((StatusCode::CREATED, Json(tokens)))
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> AppResult<Json<TokenResponse>> {
    // `= $1::citext` — сравнение без учёта регистра; без приведения сравнивались бы как text.
    let found: Option<(UserView, String)> = sqlx::query_as::<_, UserWithHash>(&format!(
        "SELECT {USER_COLUMNS}, password_hash FROM users
         WHERE email = $1::citext OR username = $1::citext"
    ))
    .bind(req.login.trim())
    .fetch_optional(&state.db)
    .await?
    .map(|row| (row.user, row.password_hash));

    let (user, stored_hash) = match found {
        Some((user, hash)) => (Some(user), Some(hash)),
        None => (None, None),
    };

    // Неверный логин, неверный пароль и заблокированный аккаунт выглядят одинаково (401),
    // чтобы не подсказывать, какие логины существуют.
    let password_ok = password::verify(req.password, stored_hash).await?;
    let user = match user {
        Some(user) if password_ok && user.is_active => user,
        _ => return Err(AppError::Unauthorized),
    };

    let mut conn = state.db.acquire().await?;
    let tokens = session::issue(&state, &mut conn, user, &client_info(&headers)).await?;
    Ok(Json(tokens))
}

/// Ротация: refresh-токен одноразовый. Повторное использование уже отозванного токена значит,
/// что его украли (или клиент сломан), — тогда отзываем все сессии пользователя.
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RefreshRequest>,
) -> AppResult<Json<TokenResponse>> {
    let mut tx = state.db.begin().await?;

    let token: Option<StoredRefreshToken> = sqlx::query_as(
        "SELECT id, user_id, expires_at, revoked_at FROM refresh_tokens
         WHERE token_hash = $1
         FOR UPDATE",
    )
    .bind(session::hash_refresh_token(&req.refresh_token))
    .fetch_optional(&mut *tx)
    .await?;

    let Some(StoredRefreshToken {
        id: token_id,
        user_id,
        expires_at,
        revoked_at,
    }) = token
    else {
        return Err(AppError::Unauthorized);
    };

    if revoked_at.is_some() {
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = now()
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::warn!(%user_id, "revoked refresh token reused, all sessions revoked");
        return Err(AppError::Unauthorized);
    }

    if expires_at <= Utc::now() {
        return Err(AppError::Unauthorized);
    }

    let user: Option<UserView> =
        sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(user) = user.filter(|user| user.is_active) else {
        return Err(AppError::Unauthorized);
    };

    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    let tokens = session::issue(&state, &mut tx, user, &client_info(&headers)).await?;
    tx.commit().await?;

    Ok(Json(tokens))
}

/// Отзывает сессию. Access-токен при этом доживает свои ≤15 минут.
/// Всегда 204: неизвестный или уже отозванный токен — не ошибка для клиента.
pub async fn logout(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> AppResult<StatusCode> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(session::hash_refresh_token(&req.refresh_token))
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<UserView>> {
    let user: Option<UserView> =
        sqlx::query_as(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
            .bind(auth.id)
            .fetch_optional(&state.db)
            .await?;
    user.filter(|user| user.is_active)
        .map(Json)
        .ok_or(AppError::Unauthorized)
}

#[derive(sqlx::FromRow)]
struct StoredRefreshToken {
    id: Uuid,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct UserWithHash {
    #[sqlx(flatten)]
    user: UserView,
    password_hash: String,
}

fn validate_registration(
    email: &str,
    username: &str,
    password: &str,
    display_name: Option<&str>,
) -> AppResult<()> {
    let bad = |msg: &str| Err(AppError::BadRequest(msg.to_string()));

    let email_ok = email.len() <= 254
        && !email.contains(char::is_whitespace)
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
    if !email_ok {
        return bad("invalid email");
    }

    let username_ok = (3..=32).contains(&username.chars().count())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !username_ok {
        return bad("username must be 3-32 characters: latin letters, digits, '_', '-', '.'");
    }

    if !(8..=128).contains(&password.chars().count()) {
        return bad("password must be 8-128 characters");
    }

    if display_name.is_some_and(|name| name.chars().count() > 64) {
        return bad("display_name must be at most 64 characters");
    }

    Ok(())
}

fn map_unique_violation(error: sqlx::Error) -> AppError {
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

/// IP берём из `X-Real-IP`, который выставляет nginx. Мусор в заголовке игнорируем.
fn client_info(headers: &HeaderMap) -> ClientInfo {
    let text = |name| {
        headers
            .get(name)
            .and_then(|value: &header::HeaderValue| value.to_str().ok())
            .map(str::to_string)
    };
    ClientInfo {
        user_agent: text(header::USER_AGENT.as_str()),
        ip: text("x-real-ip")
            .and_then(|ip| ip.parse::<IpAddr>().ok())
            .map(|ip| ip.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_registration;

    #[test]
    fn accepts_valid_registration() {
        assert!(validate_registration("a@b.co", "neo_1", "password123", Some("Neo")).is_ok());
    }

    #[test]
    fn rejects_invalid_fields() {
        assert!(validate_registration("no-at-sign", "neo", "password123", None).is_err());
        assert!(validate_registration("a@localhost", "neo", "password123", None).is_err());
        assert!(validate_registration("a@b.co", "ne", "password123", None).is_err());
        assert!(validate_registration("a@b.co", "нео", "password123", None).is_err());
        assert!(validate_registration("a@b.co", "neo", "short", None).is_err());
    }
}
