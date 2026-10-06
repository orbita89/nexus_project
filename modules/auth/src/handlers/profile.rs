//! Свой профиль: username, отображаемое имя, аватар, смена email.

use crate::email_tokens::{self, Purpose};
use crate::models::EmailTokenRequest;
use crate::models::{ChangeEmailRequest, UpdateProfileRequest, UserView, USER_COLUMNS};
use crate::rate_limit::RateLimits;
use crate::{crypto, emails, session, users, validate};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use shared::error::ErrorBody;
use shared::extract::JsonBody;
use shared::{AppError, AppResult, AppState, AuthUser};
use std::sync::Arc;

/// Как часто можно менять username: ссылки на профиль идут по нему.
pub const USERNAME_CHANGE_DAYS: i64 = 30;

/// Изменить свой профиль: username (раз в 30 дней), отображаемое имя, аватар.
#[utoipa::path(
    patch, path = "/me", tag = "profile",
    security(("bearer" = [])),
    request_body = UpdateProfileRequest,
    responses(
        (status = 200, description = "Профиль изменён", body = UserView),
        (status = 400, description = "Невалидные поля или username менялся меньше 30 дней назад", body = ErrorBody),
        (status = 401, description = "Нет токена или аккаунт заблокирован", body = ErrorBody),
        (status = 409, description = "Username занят", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    JsonBody(req): JsonBody<UpdateProfileRequest>,
) -> AppResult<Json<UserView>> {
    let display_name = req.display_name.map(non_empty);
    let avatar_url = req.avatar_url.map(non_empty);
    if let Some(name) = &display_name {
        validate::display_name(name.as_deref())?;
    }
    if let Some(url) = &avatar_url {
        validate::avatar_url(url.as_deref())?;
    }

    let mut tx = state.db.begin().await?;
    let current: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT username::text, username_changed_at FROM users
         WHERE id = $1 AND is_active FOR UPDATE",
    )
    .bind(auth.id)
    .fetch_optional(&mut *tx)
    .await?;
    let (current_username, changed_at) = current.ok_or(AppError::Unauthorized)?;

    // Тот же username — не смена (и не тратит попытку).
    let username = req
        .username
        .map(|username| username.trim().to_string())
        .filter(|username| *username != current_username);
    if let Some(username) = &username {
        validate::username(username)?;
        if let Some(next) = changed_at.map(|at| at + Duration::days(USERNAME_CHANGE_DAYS)) {
            if next > Utc::now() {
                return Err(AppError::BadRequest(format!(
                    "username can be changed once in {USERNAME_CHANGE_DAYS} days, next change after {}",
                    next.to_rfc3339()
                )));
            }
        }
    }

    let user: UserView = sqlx::query_as(&format!(
        "UPDATE users SET
            username = COALESCE($2, username),
            username_changed_at = CASE WHEN $2 IS NULL THEN username_changed_at ELSE now() END,
            display_name = CASE WHEN $3 THEN $4 ELSE display_name END,
            avatar_url = CASE WHEN $5 THEN $6 ELSE avatar_url END
         WHERE id = $1
         RETURNING {USER_COLUMNS}"
    ))
    .bind(auth.id)
    .bind(&username)
    .bind(display_name.is_some())
    .bind(display_name.flatten())
    .bind(avatar_url.is_some())
    .bind(avatar_url.flatten())
    .fetch_one(&mut *tx)
    .await
    .map_err(users::unique_violation)?;
    tx.commit().await?;

    if let Some(username) = username {
        tracing::info!(user_id = %auth.id, from = %current_username, to = %username, "username changed");
    }
    Ok(Json(user))
}

/// Обрезает пробелы; пустая строка — `None`.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Сменить email: на новый адрес уходит ссылка подтверждения (`/email/change/confirm`).
///
/// Если у аккаунта задан пароль, нужен текущий пароль: так угнанный токен не позволит увести
/// аккаунт на чужую почту. Новый запрос гасит ссылку из предыдущего.
#[utoipa::path(
    post, path = "/me/email", tag = "profile",
    security(("bearer" = [])),
    request_body = ChangeEmailRequest,
    responses(
        (status = 202, description = "Письмо с подтверждением отправлено на новый адрес"),
        (status = 400, description = "Невалидный адрес, он уже ваш, пароль не передан или неверный", body = ErrorBody),
        (status = 401, description = "Нет токена или аккаунт заблокирован", body = ErrorBody),
        (status = 409, description = "Адрес занят другим аккаунтом", body = ErrorBody),
        (status = 429, description = "Слишком много запросов", body = ErrorBody),
    )
)]
pub async fn change_email(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    auth: AuthUser,
    JsonBody(req): JsonBody<ChangeEmailRequest>,
) -> AppResult<StatusCode> {
    limits.check_ip(&headers)?;
    let new_email = req.new_email.trim().to_string();
    validate::email(&new_email)?;

    let mut conn = state.db.acquire().await?;
    let current: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT email::text, password_hash FROM users WHERE id = $1 AND is_active")
            .bind(auth.id)
            .fetch_optional(&mut *conn)
            .await?;
    let (current_email, password_hash) = current.ok_or(AppError::Unauthorized)?;
    if new_email.to_lowercase() == current_email.to_lowercase() {
        return Err(AppError::BadRequest("this is already your email".into()));
    }
    if password_hash.is_some() {
        let password = req
            .password
            .ok_or_else(|| AppError::BadRequest("password is required".into()))?;
        if !crypto::verify_password(password, password_hash).await? {
            return Err(AppError::BadRequest("password is incorrect".into()));
        }
    }
    limits.check_email(&new_email)?;
    if users::by_email(&mut conn, &new_email).await?.is_some() {
        return Err(AppError::Conflict("email already registered".into()));
    }
    drop(conn);

    let mut tx = state.db.begin().await?;
    // Ссылка из предыдущего запроса (возможно, на другой адрес) больше не работает.
    sqlx::query(
        "UPDATE email_tokens SET used_at = now()
         WHERE purpose = $1 AND user_id = $2 AND used_at IS NULL",
    )
    .bind(Purpose::ChangeEmail)
    .bind(auth.id)
    .execute(&mut *tx)
    .await?;
    let token =
        email_tokens::create(&mut tx, Purpose::ChangeEmail, &new_email, Some(auth.id)).await?;
    tx.commit().await?;

    state
        .mailer
        .send_or_log(emails::change_email(
            &state.config.app_base_url,
            &new_email,
            &token,
        ))
        .await;
    tracing::info!(user_id = %auth.id, "email change requested");
    Ok(StatusCode::ACCEPTED)
}

/// Подтвердить новый email по ссылке из письма. Токен не нужен: ссылку открывают с любого
/// устройства, владение адресом подтверждает сама ссылка.
///
/// Email заменяется и считается подтверждённым, на старый адрес уходит уведомление. Все
/// сессии отзываются, кроме текущей, если запрос пришёл с токеном этого же пользователя.
#[utoipa::path(
    post, path = "/email/change/confirm", tag = "profile",
    security((), ("bearer" = [])),
    request_body = EmailTokenRequest,
    responses(
        (status = 200, description = "Email изменён", body = UserView),
        (status = 400, description = "Ссылка недействительна, использована или истекла (живёт 1 час)", body = ErrorBody),
        (status = 401, description = "Аккаунт заблокирован или токен недействителен", body = ErrorBody),
        (status = 409, description = "Адрес за это время заняли", body = ErrorBody),
    )
)]
pub async fn confirm_email_change(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    JsonBody(req): JsonBody<EmailTokenRequest>,
) -> AppResult<Json<UserView>> {
    let mut tx = state.db.begin().await?;
    let consumed = email_tokens::consume(&mut tx, Purpose::ChangeEmail, &req.token).await?;
    let user_id = consumed
        .user_id
        .ok_or_else(|| AppError::BadRequest("invalid or expired token".into()))?;
    let old_email: Option<String> =
        sqlx::query_scalar("SELECT email::text FROM users WHERE id = $1 AND is_active FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let old_email = old_email.ok_or(AppError::Unauthorized)?;

    let user: UserView = sqlx::query_as(&format!(
        "UPDATE users SET email = $2, email_verified_at = now() WHERE id = $1
         RETURNING {USER_COLUMNS}"
    ))
    .bind(user_id)
    .bind(&consumed.email)
    .fetch_one(&mut *tx)
    .await
    .map_err(users::unique_violation)?;
    // Ссылки, отправленные на старый адрес (вход, сброс пароля), больше не работают.
    sqlx::query("UPDATE email_tokens SET used_at = now() WHERE user_id = $1 AND used_at IS NULL")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let keep = viewer
        .filter(|viewer| viewer.id == user_id)
        .map(|viewer| viewer.session_id);
    session::revoke_all(&mut tx, user_id, keep).await?;
    tx.commit().await?;

    state
        .mailer
        .send_or_log(emails::email_changed(&old_email, &user.email))
        .await;
    tracing::info!(user_id = %user_id, "email changed");
    Ok(Json(user))
}
