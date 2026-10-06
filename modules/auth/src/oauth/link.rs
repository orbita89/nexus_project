//! Привязка провайдеров из профиля: начать, список, отвязать.
//!
//! Браузерный переход к провайдеру не несёт заголовок с токеном, поэтому привязка начинается
//! запросом с токеном (`POST /me/oauth/{provider}`): сервер запоминает пользователя в
//! `oauth_states` и отдаёт ссылку, которую фронтенд открывает в браузере. По возврату
//! (`callback`) аккаунт провайдера привязывается к этому пользователю — email у провайдера
//! может быть любым.

use super::{authorize_url, find_provider, provider::Profile, OAuthError};
use crate::models::{LinkStartResponse, LinkedAccount};
use crate::rate_limit::RateLimits;
use crate::users;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppError, AppResult, AppState, AuthUser};
use sqlx::PgConnection;
use std::sync::Arc;
use uuid::Uuid;

/// Привязанные к своему аккаунту провайдеры.
#[utoipa::path(
    get, operation_id = "list_linked_accounts", path = "/me/oauth", tag = "profile",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Привязанные аккаунты, старые сверху", body = Vec<LinkedAccount>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<LinkedAccount>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT provider, email::text AS email, created_at FROM oauth_accounts
             WHERE user_id = $1 ORDER BY created_at, provider",
        )
        .bind(auth.id)
        .fetch_all(&state.db)
        .await?,
    ))
}

/// Начать привязку провайдера: ссылка на его страницу входа. Открыть в браузере
/// (`window.location`); вернётся на `{APP_BASE_URL}/settings/accounts?linked=...` или `?error=...`.
#[utoipa::path(
    post, operation_id = "start_oauth_link", path = "/me/oauth/{provider}", tag = "profile",
    security(("bearer" = [])),
    params(("provider" = String, Path, description = "google, github, yandex")),
    responses(
        (status = 200, description = "Ссылка на страницу провайдера (действует 10 минут)", body = LinkStartResponse),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Провайдер не включён", body = ErrorBody),
        (status = 429, description = "Слишком много запросов", body = ErrorBody),
    )
)]
pub async fn start(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    auth: AuthUser,
    Path(provider_name): Path<String>,
) -> AppResult<Json<LinkStartResponse>> {
    limits.check_ip(&headers)?;
    let provider = find_provider(&state, &provider_name).ok_or(AppError::NotFound)?;
    let url = authorize_url(&state, provider, Some(auth.id)).await?;
    Ok(Json(LinkStartResponse { url }))
}

/// Отвязать провайдера. Войти можно и без него — по ссылке на email.
#[utoipa::path(
    delete, path = "/me/oauth/{provider}", tag = "profile",
    security(("bearer" = [])),
    params(("provider" = String, Path, description = "google, github, yandex")),
    responses(
        (status = 204, description = "Отвязан"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Этот провайдер не привязан", body = ErrorBody),
    )
)]
pub async fn unlink(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(provider_name): Path<String>,
) -> AppResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM oauth_accounts WHERE user_id = $1 AND provider = $2")
        .bind(auth.id)
        .bind(&provider_name)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(user_id = %auth.id, provider = %provider_name, "oauth account unlinked");
    Ok(StatusCode::NO_CONTENT)
}

/// Привязать аккаунт провайдера к `user_id`. Уже привязан к нему же — ничего не делает.
pub(super) async fn link_account(
    conn: &mut PgConnection,
    provider: &str,
    profile: Profile,
    user_id: Uuid,
) -> Result<(), OAuthError> {
    let user = users::by_id(conn, user_id)
        .await?
        .ok_or(OAuthError::AccountBlocked)?;
    if !user.is_active {
        return Err(OAuthError::AccountBlocked);
    }
    let linked: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM oauth_accounts WHERE provider = $1 AND provider_user_id = $2",
    )
    .bind(provider)
    .bind(&profile.provider_user_id)
    .fetch_optional(&mut *conn)
    .await?;
    match linked {
        Some(owner) if owner == user_id => return Ok(()),
        Some(_) => return Err(OAuthError::AlreadyLinked),
        None => {}
    }
    let has_other: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM oauth_accounts WHERE user_id = $1 AND provider = $2)",
    )
    .bind(user_id)
    .bind(provider)
    .fetch_one(&mut *conn)
    .await?;
    if has_other {
        return Err(OAuthError::ProviderAlreadyLinked);
    }
    sqlx::query(
        "INSERT INTO oauth_accounts (provider, provider_user_id, user_id, email)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(provider)
    .bind(&profile.provider_user_id)
    .bind(user_id)
    .bind(&profile.email)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
