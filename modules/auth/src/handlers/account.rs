//! Регистрация по паролю, вход, обновление токенов, выход, профиль.

use crate::email_tokens::{self, Purpose};
use crate::models::{
    LoginRequest, RefreshRequest, RegisterRequest, TokenResponse, UserView, USER_COLUMNS,
};
use crate::rate_limit::RateLimits;
use crate::session::{self, ClientInfo};
use crate::{crypto, emails, users, validate};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use shared::error::ErrorBody;
use shared::extract::JsonBody;
use shared::{AppError, AppResult, AppState, AuthUser};
use std::sync::Arc;
use uuid::Uuid;

/// Регистрация по email и паролю.
///
/// Аккаунт создаётся неподтверждённым, на почту уходит ссылка. Войти по паролю можно только
/// после подтверждения (`/email/verify`), а оно сразу выдаёт токены.
#[utoipa::path(
    post, path = "/register", tag = "auth",
    request_body = RegisterRequest,
    responses(
        (status = 201, description = "Аккаунт создан, отправлено письмо для подтверждения", body = UserView),
        (status = 400, description = "Невалидные поля", body = ErrorBody),
        (status = 409, description = "Email или username заняты", body = ErrorBody),
        (status = 429, description = "Слишком много запросов", body = ErrorBody),
    )
)]
pub async fn register(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<RegisterRequest>,
) -> AppResult<(StatusCode, Json<UserView>)> {
    limits.check_ip(&headers)?;
    let email = req.email.trim().to_string();
    let username = req.username.trim().to_string();
    let display_name = req
        .display_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    validate::email(&email)?;
    validate::username(&username)?;
    validate::password(&req.password)?;
    validate::display_name(display_name.as_deref())?;
    limits.check_email(&email)?;

    let password_hash = crypto::hash_password(req.password).await?;

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
    let token = email_tokens::create(&mut tx, Purpose::VerifyEmail, &email, Some(user.id)).await?;
    tx.commit().await?;

    state
        .mailer
        .send_or_log(emails::verify_email(
            &state.config.app_base_url,
            &email,
            &token,
        ))
        .await;
    tracing::info!(user_id = %user.id, "user registered");
    Ok((StatusCode::CREATED, Json(user)))
}

/// Вход по email (или username) и паролю.
///
/// Неверный логин, неверный пароль и заблокированный аккаунт выглядят одинаково (401),
/// чтобы не подсказывать, какие логины существуют.
#[utoipa::path(
    post, path = "/login", tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Пара токенов", body = TokenResponse),
        (status = 401, description = "Неверный логин или пароль, либо аккаунт заблокирован", body = ErrorBody),
        (status = 403, description = "Email не подтверждён", body = ErrorBody),
        (status = 429, description = "Слишком много попыток", body = ErrorBody),
    )
)]
pub async fn login(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<LoginRequest>,
) -> AppResult<Json<TokenResponse>> {
    limits.check_ip(&headers)?;
    limits.check_login(&req.login)?;

    let mut conn = state.db.acquire().await?;
    let (user, stored_hash) = match users::by_login_with_hash(&mut conn, &req.login).await? {
        Some(found) => (Some(found.user), found.password_hash),
        None => (None, None),
    };

    let password_ok = crypto::verify_password(req.password, stored_hash).await?;
    let user = match user {
        Some(user) if password_ok && user.is_active => user,
        _ => return Err(AppError::Unauthorized),
    };
    // Проверяем после пароля: иначе по ответу можно было бы узнать, что аккаунт существует.
    if user.email_verified_at.is_none() {
        return Err(AppError::EmailNotVerified);
    }

    let tokens =
        session::issue(&state, &mut conn, user, &ClientInfo::from_headers(&headers)).await?;
    Ok(Json(tokens))
}

/// Новая пара токенов по refresh-токену.
///
/// Refresh-токен одноразовый: старый отзывается, выдаётся новый. Повторное использование уже
/// заменённого токена значит, что его украли (или клиент сломан), — тогда отзываются все сессии.
#[utoipa::path(
    post, path = "/refresh", tag = "auth",
    request_body = RefreshRequest,
    responses(
        (status = 200, description = "Новая пара токенов", body = TokenResponse),
        (status = 401, description = "Токен недействителен, истёк или отозван", body = ErrorBody),
    )
)]
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<RefreshRequest>,
) -> AppResult<Json<TokenResponse>> {
    let mut tx = state.db.begin().await?;

    let token: Option<StoredRefreshToken> = sqlx::query_as(
        "SELECT id, user_id, expires_at, revoked_at, replaced_by FROM refresh_tokens
         WHERE token_hash = $1
         FOR UPDATE",
    )
    .bind(crypto::hash_token(&req.refresh_token))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(token) = token else {
        return Err(AppError::Unauthorized);
    };

    if token.revoked_at.is_some() {
        // Заменённый при обновлении токен пришёл снова — его украли (или клиент сломан).
        // Отозванный выходом — просто недействителен.
        if token.replaced_by.is_some() {
            session::revoke_all(&mut tx, token.user_id, None).await?;
            tx.commit().await?;
            tracing::warn!(user_id = %token.user_id, "rotated refresh token reused, all sessions revoked");
        }
        return Err(AppError::Unauthorized);
    }
    if token.expires_at <= Utc::now() {
        return Err(AppError::Unauthorized);
    }

    let Some(user) = users::by_id(&mut tx, token.user_id)
        .await?
        .filter(|user| user.is_active)
    else {
        return Err(AppError::Unauthorized);
    };

    let tokens = session::issue(&state, &mut tx, user, &ClientInfo::from_headers(&headers)).await?;
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now(),
             replaced_by = (SELECT id FROM refresh_tokens WHERE token_hash = $2)
         WHERE id = $1",
    )
    .bind(token.id)
    .bind(crypto::hash_token(&tokens.refresh_token))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(tokens))
}

/// Выход: отзывает refresh-токен. Access-токен доживает свои ≤15 минут.
/// Всегда 204: неизвестный или уже отозванный токен — не ошибка для клиента.
#[utoipa::path(
    post, path = "/logout", tag = "auth",
    request_body = RefreshRequest,
    responses((status = 204, description = "Сессия завершена"))
)]
pub async fn logout(
    State(state): State<AppState>,
    JsonBody(req): JsonBody<RefreshRequest>,
) -> AppResult<StatusCode> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(crypto::hash_token(&req.refresh_token))
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Профиль текущего пользователя.
#[utoipa::path(
    get, path = "/me", tag = "auth",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Профиль", body = UserView),
        (status = 401, description = "Нет токена или он недействителен", body = ErrorBody),
    )
)]
pub async fn me(State(state): State<AppState>, auth: AuthUser) -> AppResult<Json<UserView>> {
    let mut conn = state.db.acquire().await?;
    users::by_id(&mut conn, auth.id)
        .await?
        .filter(|user| user.is_active)
        .map(Json)
        .ok_or(AppError::Unauthorized)
}

#[derive(sqlx::FromRow)]
struct StoredRefreshToken {
    id: Uuid,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    replaced_by: Option<Uuid>,
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
