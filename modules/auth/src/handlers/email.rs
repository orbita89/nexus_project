//! Почта: подтверждение email и вход по ссылке без пароля.
//!
//! Эндпоинты, отправляющие письма, всегда отвечают 202 — даже если адрес не зарегистрирован,
//! чтобы по ответу нельзя было проверить, есть ли такой пользователь.

use crate::email_tokens::{self, Purpose};
use crate::models::{EmailRequest, EmailTokenRequest, TokenResponse};
use crate::rate_limit::RateLimits;
use crate::session::{self, ClientInfo};
use crate::{emails, users, validate};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use shared::error::ErrorBody;
use shared::extract::JsonBody;
use shared::{AppError, AppResult, AppState};
use std::sync::Arc;

/// Подтверждение email по ссылке из письма после регистрации. Сразу выполняет вход.
#[utoipa::path(
    post, path = "/email/verify", tag = "auth",
    request_body = EmailTokenRequest,
    responses(
        (status = 200, description = "Email подтверждён, пара токенов", body = TokenResponse),
        (status = 400, description = "Токен недействителен, использован или истёк", body = ErrorBody),
        (status = 401, description = "Аккаунт заблокирован", body = ErrorBody),
    )
)]
pub async fn verify_email(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<EmailTokenRequest>,
) -> AppResult<Json<TokenResponse>> {
    let mut tx = state.db.begin().await?;
    let consumed = email_tokens::consume(&mut tx, Purpose::VerifyEmail, &req.token).await?;
    let user_id = consumed.user_id.ok_or(AppError::Unauthorized)?;
    let user = users::mark_email_verified(&mut tx, user_id).await?;
    if !user.is_active {
        return Err(AppError::Unauthorized);
    }
    let tokens = session::issue(&state, &mut tx, user, &ClientInfo::from_headers(&headers)).await?;
    tx.commit().await?;
    Ok(Json(tokens))
}

/// Отправить письмо для подтверждения email ещё раз.
#[utoipa::path(
    post, path = "/email/verify/resend", tag = "auth",
    request_body = EmailRequest,
    responses(
        (status = 202, description = "Если адрес зарегистрирован и не подтверждён — письмо отправлено"),
        (status = 429, description = "Слишком много писем на этот адрес", body = ErrorBody),
    )
)]
pub async fn resend_verification(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<EmailRequest>,
) -> AppResult<StatusCode> {
    limits.check_ip(&headers)?;
    let email = req.email.trim();
    validate::email(email)?;
    limits.check_email(email)?;

    let mut tx = state.db.begin().await?;
    let user = users::by_email(&mut tx, email)
        .await?
        .filter(|user| user.is_active && user.email_verified_at.is_none());
    if let Some(user) = user {
        let token =
            email_tokens::create(&mut tx, Purpose::VerifyEmail, &user.email, Some(user.id)).await?;
        tx.commit().await?;
        state
            .mailer
            .send_or_log(emails::verify_email(
                &state.config.app_base_url,
                &user.email,
                &token,
            ))
            .await;
    }
    Ok(StatusCode::ACCEPTED)
}

/// Вход по почте без пароля: шаг 1 — отправить ссылку.
///
/// Работает и для новых адресов: аккаунт создаётся, когда пользователь перейдёт по ссылке.
#[utoipa::path(
    post, path = "/email/login", tag = "auth",
    request_body = EmailRequest,
    responses(
        (status = 202, description = "Ссылка для входа отправлена"),
        (status = 400, description = "Невалидный email", body = ErrorBody),
        (status = 429, description = "Слишком много писем на этот адрес", body = ErrorBody),
    )
)]
pub async fn email_login_start(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<EmailRequest>,
) -> AppResult<StatusCode> {
    limits.check_ip(&headers)?;
    let email = req.email.trim();
    validate::email(email)?;
    limits.check_email(email)?;

    let mut tx = state.db.begin().await?;
    let user = users::by_email(&mut tx, email).await?;
    // Заблокированному письмо не шлём, но ответ тот же.
    if user.as_ref().is_some_and(|user| !user.is_active) {
        return Ok(StatusCode::ACCEPTED);
    }
    let to = user.as_ref().map_or(email, |user| user.email.as_str());
    let token =
        email_tokens::create(&mut tx, Purpose::Login, to, user.as_ref().map(|u| u.id)).await?;
    tx.commit().await?;

    state
        .mailer
        .send_or_log(emails::login_link(&state.config.app_base_url, to, &token))
        .await;
    Ok(StatusCode::ACCEPTED)
}

/// Вход по почте без пароля: шаг 2 — токен из ссылки. Новый адрес — создаётся аккаунт.
#[utoipa::path(
    post, path = "/email/login/confirm", tag = "auth",
    request_body = EmailTokenRequest,
    responses(
        (status = 200, description = "Вход выполнен (аккаунт создан, если его не было)", body = TokenResponse),
        (status = 400, description = "Токен недействителен, использован или истёк", body = ErrorBody),
        (status = 401, description = "Аккаунт заблокирован", body = ErrorBody),
    )
)]
pub async fn email_login_confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<EmailTokenRequest>,
) -> AppResult<Json<TokenResponse>> {
    let mut tx = state.db.begin().await?;
    let consumed = email_tokens::consume(&mut tx, Purpose::Login, &req.token).await?;

    // Пока письмо шло, адрес мог зарегистрироваться обычным способом — ищем ещё раз.
    let existing = match consumed.user_id {
        Some(id) => users::by_id(&mut tx, id).await?,
        None => users::by_email(&mut tx, &consumed.email).await?,
    };
    let user = match existing {
        // Переход по ссылке доказывает владение адресом — заодно подтверждаем email.
        Some(user) if user.is_active => users::mark_email_verified(&mut tx, user.id).await?,
        Some(_) => return Err(AppError::Unauthorized),
        None => {
            let user = users::create_from_email(&mut tx, &consumed.email).await?;
            tracing::info!(user_id = %user.id, "user registered via email link");
            user
        }
    };

    let tokens = session::issue(&state, &mut tx, user, &ClientInfo::from_headers(&headers)).await?;
    tx.commit().await?;
    Ok(Json(tokens))
}
