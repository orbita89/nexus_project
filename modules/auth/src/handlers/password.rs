//! Сброс пароля по почте и смена пароля из профиля.

use crate::email_tokens::{self, Purpose};
use crate::models::{ChangePasswordRequest, EmailRequest, ResetPasswordRequest};
use crate::rate_limit::RateLimits;
use crate::session;
use crate::{crypto, emails, users, validate};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use shared::error::ErrorBody;
use shared::{AppError, AppResult, AppState, AuthUser};
use std::sync::Arc;

/// Забыл пароль: отправить ссылку для сброса. Всегда 202 — не раскрываем, есть ли адрес.
#[utoipa::path(
    post, path = "/password/forgot", tag = "auth",
    request_body = EmailRequest,
    responses(
        (status = 202, description = "Если адрес зарегистрирован — письмо отправлено"),
        (status = 429, description = "Слишком много писем на этот адрес", body = ErrorBody),
    )
)]
pub async fn forgot(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    Json(req): Json<EmailRequest>,
) -> AppResult<StatusCode> {
    limits.check_ip(&headers)?;
    let email = req.email.trim();
    validate::email(email)?;
    limits.check_email(email)?;

    let mut tx = state.db.begin().await?;
    if let Some(user) = users::by_email(&mut tx, email)
        .await?
        .filter(|user| user.is_active)
    {
        let token =
            email_tokens::create(&mut tx, Purpose::ResetPassword, &user.email, Some(user.id))
                .await?;
        tx.commit().await?;
        state
            .mailer
            .send_or_log(emails::reset_password(
                &state.config.app_base_url,
                &user.email,
                &token,
            ))
            .await;
    }
    Ok(StatusCode::ACCEPTED)
}

/// Новый пароль по токену из письма. Все сессии пользователя завершаются.
#[utoipa::path(
    post, path = "/password/reset", tag = "auth",
    request_body = ResetPasswordRequest,
    responses(
        (status = 204, description = "Пароль изменён, все устройства разлогинены"),
        (status = 400, description = "Токен недействителен или пароль не подходит", body = ErrorBody),
    )
)]
pub async fn reset(
    State(state): State<AppState>,
    Json(req): Json<ResetPasswordRequest>,
) -> AppResult<StatusCode> {
    validate::password(&req.password)?;
    let password_hash = crypto::hash_password(req.password).await?;

    let mut tx = state.db.begin().await?;
    let consumed = email_tokens::consume(&mut tx, Purpose::ResetPassword, &req.token).await?;
    let user_id = consumed.user_id.ok_or(AppError::Unauthorized)?;
    // Ссылка пришла на почту — значит, адрес подтверждён.
    sqlx::query(
        "UPDATE users SET password_hash = $2, email_verified_at = COALESCE(email_verified_at, now())
         WHERE id = $1",
    )
    .bind(user_id)
    .bind(&password_hash)
    .execute(&mut *tx)
    .await?;
    session::revoke_all(&mut tx, user_id, None).await?;
    tx.commit().await?;

    tracing::info!(%user_id, "password reset");
    Ok(StatusCode::NO_CONTENT)
}

/// Смена пароля из профиля. Остальные устройства разлогиниваются, текущее — нет.
///
/// Если пароля ещё нет (вход был по ссылке из письма), задать его можно через `/password/forgot`.
#[utoipa::path(
    post, path = "/password/change", tag = "auth",
    security(("bearer" = [])),
    request_body = ChangePasswordRequest,
    responses(
        (status = 204, description = "Пароль изменён"),
        (status = 400, description = "Текущий пароль неверный, не задан, или новый не подходит", body = ErrorBody),
        (status = 401, description = "Нет токена или он недействителен", body = ErrorBody),
    )
)]
pub async fn change(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ChangePasswordRequest>,
) -> AppResult<StatusCode> {
    validate::password(&req.new_password)?;

    let mut conn = state.db.acquire().await?;
    let stored_hash: Option<Option<String>> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1 AND is_active")
            .bind(auth.id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(stored_hash) = stored_hash else {
        return Err(AppError::Unauthorized);
    };
    if stored_hash.is_none() {
        return Err(AppError::BadRequest(
            "password is not set, use password reset".to_string(),
        ));
    }
    if !crypto::verify_password(req.current_password, stored_hash).await? {
        return Err(AppError::BadRequest(
            "current password is incorrect".to_string(),
        ));
    }

    let password_hash = crypto::hash_password(req.new_password).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(auth.id)
        .bind(&password_hash)
        .execute(&mut *tx)
        .await?;
    session::revoke_all(&mut tx, auth.id, Some(auth.session_id)).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
