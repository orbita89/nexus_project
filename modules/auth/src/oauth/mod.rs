//! Вход через внешних провайдеров (OAuth 2.0, authorization code + PKCE).
//!
//! 1. Фронтенд открывает `GET /oauth/{provider}/start` — редирект на страницу входа провайдера.
//! 2. Провайдер возвращает браузер на `GET /oauth/{provider}/callback` с кодом. Сервер меняет
//!    код на профиль, находит/создаёт пользователя и редиректит на фронтенд
//!    `{APP_BASE_URL}/auth/oauth/callback?code=...` (или `?error=...`).
//! 3. Фронтенд меняет одноразовый код на токены: `POST /oauth/exchange`.
//!
//! Токены не попадают в URL (историю браузера, логи прокси) — только короткоживущий код.

mod provider;

use crate::email_tokens::{self, Purpose};
use crate::models::{TokenResponse, UserView};
use crate::rate_limit::RateLimits;
use crate::session::{self, ClientInfo};
use crate::{crypto, users};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Redirect;
use axum::{Extension, Json};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use provider::Profile;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared::config::OAuthProviderConfig;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AppError, AppResult, AppState, API_PREFIX};
use sqlx::PgConnection;
use std::sync::Arc;
use std::time::Duration;
use utoipa::{IntoParams, ToSchema};

/// HTTP-клиент для запросов к провайдерам.
#[derive(Clone)]
pub struct OAuthHttp(pub reqwest::Client);

impl Default for OAuthHttp {
    fn default() -> Self {
        Self(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("build http client"),
        )
    }
}

const STATE_TTL_MINUTES: i32 = 10;

#[derive(Serialize, ToSchema)]
pub struct ProvidersResponse {
    /// Включённые провайдеры: для них показывать кнопки входа.
    #[schema(example = json!(["google", "github"]))]
    pub providers: Vec<String>,
}

#[derive(Deserialize, IntoParams)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// Провайдер вернул ошибку (например, пользователь нажал «Отмена»).
    error: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct ExchangeRequest {
    /// Параметр `code` из адреса `{APP_BASE_URL}/auth/oauth/callback?code=...`.
    pub code: String,
}

/// Какие провайдеры включены (для кнопок «Войти через ...»).
#[utoipa::path(
    get, path = "/oauth/providers", tag = "oauth",
    responses((status = 200, body = ProvidersResponse))
)]
pub async fn providers(State(state): State<AppState>) -> Json<ProvidersResponse> {
    Json(ProvidersResponse {
        providers: state
            .config
            .oauth_providers
            .iter()
            .map(|p| p.name.clone())
            .collect(),
    })
}

/// Начать вход через провайдера: редирект на его страницу входа.
///
/// Открывать в браузере (не через fetch): дальше браузер сам пройдёт по редиректам.
#[utoipa::path(
    get, path = "/oauth/{provider}/start", tag = "oauth",
    params(("provider" = String, Path, description = "google, github, yandex")),
    responses(
        (status = 303, description = "Редирект на страницу входа провайдера"),
        (status = 404, description = "Провайдер не включён", body = ErrorBody),
        (status = 429, description = "Слишком много запросов", body = ErrorBody),
    )
)]
pub async fn start(
    State(state): State<AppState>,
    Extension(limits): Extension<Arc<RateLimits>>,
    headers: HeaderMap,
    Path(provider_name): Path<String>,
) -> AppResult<Redirect> {
    limits.check_ip(&headers)?;
    let provider = find_provider(&state, &provider_name).ok_or(AppError::NotFound)?;

    let csrf_state = crypto::generate_token();
    let code_verifier = crypto::generate_token();
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

    sqlx::query(
        "INSERT INTO oauth_states (state_hash, provider, code_verifier, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(mins => $4))",
    )
    .bind(crypto::hash_token(&csrf_state))
    .bind(&provider.name)
    .bind(&code_verifier)
    .bind(STATE_TTL_MINUTES)
    .execute(&state.db)
    .await?;

    let url = Url::parse_with_params(
        &provider.authorize_url,
        &[
            ("response_type", "code"),
            ("client_id", provider.client_id.as_str()),
            (
                "redirect_uri",
                redirect_uri(&state, &provider.name).as_str(),
            ),
            ("scope", provider.scopes.as_str()),
            ("state", csrf_state.as_str()),
            ("code_challenge", code_challenge.as_str()),
            ("code_challenge_method", "S256"),
        ],
    )
    .map_err(|e| AppError::Internal(format!("authorize url: {e}")))?;
    Ok(Redirect::to(url.as_str()))
}

/// Сюда провайдер возвращает браузер. Редиректит на фронтенд с одноразовым кодом или ошибкой.
///
/// Коды ошибок в `?error=`: `access_denied` (пользователь отменил вход), `invalid_state`
/// (ссылка устарела или подделана), `provider_error`, `email_required` (провайдер не дал email),
/// `email_in_use` (адрес занят, а провайдер не подтвердил, что он принадлежит пользователю),
/// `account_blocked`, `internal_error`.
#[utoipa::path(
    get, path = "/oauth/{provider}/callback", tag = "oauth",
    params(("provider" = String, Path), CallbackQuery),
    responses((status = 303, description = "Редирект на фронтенд: `/auth/oauth/callback?code=...` или `?error=...`"))
)]
pub async fn callback(
    State(state): State<AppState>,
    Extension(http): Extension<OAuthHttp>,
    Path(provider_name): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> Redirect {
    let param = match complete_login(&state, &http, &provider_name, query).await {
        Ok(code) => ("code", code),
        Err(error) => ("error", error.code().to_string()),
    };
    let url = Url::parse_with_params(
        &format!("{}/auth/oauth/callback", state.config.app_base_url),
        &[param],
    )
    .map(|url| url.to_string())
    .unwrap_or_else(|_| {
        format!(
            "{}/auth/oauth/callback?error=internal_error",
            state.config.app_base_url
        )
    });
    Redirect::to(&url)
}

/// Одноразовый код с фронтенда → пара токенов.
#[utoipa::path(
    post, path = "/oauth/exchange", tag = "oauth",
    request_body = ExchangeRequest,
    responses(
        (status = 200, description = "Вход выполнен", body = TokenResponse),
        (status = 400, description = "Код недействителен, использован или истёк (живёт 2 минуты)", body = ErrorBody),
        (status = 401, description = "Аккаунт заблокирован", body = ErrorBody),
    )
)]
pub async fn exchange(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(req): JsonBody<ExchangeRequest>,
) -> AppResult<Json<TokenResponse>> {
    let mut tx = state.db.begin().await?;
    let consumed = email_tokens::consume(&mut tx, Purpose::OAuthLogin, &req.code).await?;
    let user = match consumed.user_id {
        Some(id) => users::by_id(&mut tx, id).await?,
        None => None,
    };
    let user = user
        .filter(|user| user.is_active)
        .ok_or(AppError::Unauthorized)?;
    let tokens = session::issue(&state, &mut tx, user, &ClientInfo::from_headers(&headers)).await?;
    tx.commit().await?;
    Ok(Json(tokens))
}

#[derive(Debug)]
enum OAuthError {
    AccessDenied,
    InvalidState,
    Provider(String),
    EmailRequired,
    EmailInUse,
    AccountBlocked,
    Internal(String),
}

impl OAuthError {
    fn code(&self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
            Self::InvalidState => "invalid_state",
            Self::Provider(_) => "provider_error",
            Self::EmailRequired => "email_required",
            Self::EmailInUse => "email_in_use",
            Self::AccountBlocked => "account_blocked",
            Self::Internal(_) => "internal_error",
        }
    }
}

impl From<AppError> for OAuthError {
    fn from(error: AppError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<sqlx::Error> for OAuthError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

async fn complete_login(
    state: &AppState,
    http: &OAuthHttp,
    provider_name: &str,
    query: CallbackQuery,
) -> Result<String, OAuthError> {
    let result = async {
        let provider = find_provider(state, provider_name).ok_or(OAuthError::InvalidState)?;
        if query.error.is_some() {
            return Err(OAuthError::AccessDenied);
        }
        let (Some(code), Some(csrf_state)) = (query.code, query.state) else {
            return Err(OAuthError::InvalidState);
        };

        // state одноразовый и привязан к провайдеру.
        let code_verifier: Option<String> = sqlx::query_scalar(
            "DELETE FROM oauth_states
             WHERE state_hash = $1 AND provider = $2 AND expires_at > now()
             RETURNING code_verifier",
        )
        .bind(crypto::hash_token(&csrf_state))
        .bind(&provider.name)
        .fetch_optional(&state.db)
        .await?;
        let code_verifier = code_verifier.ok_or(OAuthError::InvalidState)?;

        let redirect_uri = redirect_uri(state, &provider.name);
        let access_token =
            provider::exchange_code(&http.0, provider, &code, &redirect_uri, &code_verifier)
                .await
                .map_err(|e| OAuthError::Provider(e.0))?;
        let profile = provider::fetch_profile(&http.0, provider, &access_token)
            .await
            .map_err(|e| OAuthError::Provider(e.0))?;

        let mut tx = state.db.begin().await?;
        let user = find_or_create_user(&mut tx, &provider.name, profile).await?;
        let code =
            email_tokens::create(&mut tx, Purpose::OAuthLogin, &user.email, Some(user.id)).await?;
        tx.commit().await?;
        Ok(code)
    }
    .await;

    match &result {
        Err(OAuthError::Provider(e) | OAuthError::Internal(e)) => {
            tracing::error!(provider = provider_name, error = %e, "oauth login failed");
        }
        Err(e) => tracing::info!(
            provider = provider_name,
            error = e.code(),
            "oauth login rejected"
        ),
        Ok(_) => {}
    }
    result
}

/// Привязанный аккаунт → его пользователь. Иначе — существующий пользователь с тем же
/// подтверждённым email (привязываем) или новый пользователь.
async fn find_or_create_user(
    conn: &mut PgConnection,
    provider: &str,
    profile: Profile,
) -> Result<UserView, OAuthError> {
    let linked: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM oauth_accounts WHERE provider = $1 AND provider_user_id = $2",
    )
    .bind(provider)
    .bind(&profile.provider_user_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(user_id) = linked {
        let user = users::by_id(conn, user_id)
            .await?
            .ok_or_else(|| OAuthError::Internal("linked user not found".into()))?;
        return active(user);
    }

    let email = profile.email.clone().ok_or(OAuthError::EmailRequired)?;
    let user = match users::by_email(conn, &email).await? {
        // Без подтверждения от провайдера привязка к чужому аккаунту — это его захват.
        Some(_) if !profile.email_verified => return Err(OAuthError::EmailInUse),
        Some(user) => {
            let user = active(user)?;
            users::mark_email_verified(conn, user.id).await?
        }
        None => {
            let user = users::create_external(
                conn,
                &email,
                profile.email_verified,
                profile.name.as_deref(),
                profile.avatar_url.as_deref(),
            )
            .await?;
            tracing::info!(user_id = %user.id, provider, "user registered via oauth");
            user
        }
    };

    sqlx::query(
        "INSERT INTO oauth_accounts (provider, provider_user_id, user_id, email)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(provider)
    .bind(&profile.provider_user_id)
    .bind(user.id)
    .bind(&email)
    .execute(&mut *conn)
    .await?;
    Ok(user)
}

fn active(user: UserView) -> Result<UserView, OAuthError> {
    if user.is_active {
        Ok(user)
    } else {
        Err(OAuthError::AccountBlocked)
    }
}

fn find_provider<'a>(state: &'a AppState, name: &str) -> Option<&'a OAuthProviderConfig> {
    state.config.oauth_providers.iter().find(|p| p.name == name)
}

/// Его же нужно указать в настройках приложения у провайдера.
fn redirect_uri(state: &AppState, provider: &str) -> String {
    format!(
        "{}{API_PREFIX}/auth/oauth/{provider}/callback",
        state.config.public_url
    )
}
