//! Свои провайдеры ИИ пользователя (`/me/ai-providers`): ключ, адрес, модель. Ключ шифрует
//! Postgres (pgcrypto, `AI_SECRET_KEY`) — как у админских провайдеров (`catalog::ai`); пишется и
//! не читается: наружу только подсказка `sk-…a1b2`. Платит за запросы сам пользователь.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AppError, AppResult, AppState, AuthUser};
use utoipa::ToSchema;
use uuid::Uuid;

use super::llm::{self, Provider, Turn};

/// Провайдеров у одного пользователя — не больше.
const MAX_PROVIDERS: i64 = 5;

/// Протокол API: `openai` — любой OpenAI-совместимый (OpenAI, DeepSeek, OpenRouter, свой
/// сервер), `anthropic` — Claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum UserAiKind {
    Openai,
    Anthropic,
}

impl UserAiKind {
    fn as_str(self) -> &'static str {
        match self {
            UserAiKind::Openai => "openai",
            UserAiKind::Anthropic => "anthropic",
        }
    }
}

/// Свой провайдер (без ключа).
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct UserAiProvider {
    pub id: Uuid,
    #[schema(example = "openai")]
    pub kind: String,
    #[schema(example = "DeepSeek")]
    pub title: String,
    /// Пусто — адрес по умолчанию протокола.
    #[schema(example = "https://api.deepseek.com")]
    pub base_url: Option<String>,
    #[schema(example = "deepseek-chat")]
    pub model: String,
    /// `sk-…a1b2`.
    pub key_hint: String,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, kind, title, base_url, model, key_hint, is_default, created_at";

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateUserAiProvider {
    pub kind: UserAiKind,
    pub title: String,
    pub base_url: Option<String>,
    pub model: String,
    pub api_key: String,
    /// Сделать провайдером по умолчанию (первый — всегда по умолчанию).
    #[serde(default)]
    pub is_default: bool,
}

/// Правка: переданное поле меняется; `api_key` — новый ключ.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateUserAiProvider {
    pub title: Option<String>,
    /// Пустая строка — адрес по умолчанию.
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// `true` — сделать провайдером по умолчанию.
    pub is_default: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserAiTest {
    pub ok: bool,
    /// Ответ модели или текст ошибки (ключ вырезан).
    pub message: String,
}

pub(super) fn secret(state: &AppState) -> AppResult<&str> {
    state
        .config
        .ai_secret_key
        .as_deref()
        .ok_or_else(|| AppError::Unavailable("ИИ не настроен на сервере (AI_SECRET_KEY)".into()))
}

fn invalid(field: &str, message: &str) -> AppError {
    AppError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

fn text(field: &str, value: &str, max: usize) -> AppResult<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        return Err(invalid(field, &format!("1–{max} символов")));
    }
    Ok(value.to_string())
}

/// Адрес провайдера: только https (http — только localhost, для своего сервера в деве).
fn base_url(value: Option<&str>) -> AppResult<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let local = value.starts_with("http://localhost") || value.starts_with("http://127.0.0.1");
    if value.len() > 300 || !(value.starts_with("https://") || local) {
        return Err(invalid("base_url", "адрес https:// до 300 символов"));
    }
    Ok(Some(value.trim_end_matches('/').to_string()))
}

/// Свои провайдеры.
#[utoipa::path(
    get, operation_id = "list_my_ai_providers", path = "/me/ai-providers", tag = "ai-tutor",
    security(("bearer" = [])),
    responses(
        (status = 200, body = Vec<UserAiProvider>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Vec<UserAiProvider>>> {
    let rows = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM user_ai_providers WHERE user_id = $1 ORDER BY created_at"
    ))
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Добавить провайдера (свой ключ).
#[utoipa::path(
    post, operation_id = "create_my_ai_provider", path = "/me/ai-providers", tag = "ai-tutor",
    security(("bearer" = [])),
    request_body = CreateUserAiProvider,
    responses(
        (status = 201, body = UserAiProvider),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 409, description = "Уже 5 провайдеров", body = ErrorBody),
        (status = 503, description = "ИИ не настроен на сервере", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(req): JsonBody<CreateUserAiProvider>,
) -> AppResult<(StatusCode, Json<UserAiProvider>)> {
    let title = text("title", &req.title, 60)?;
    let model = text("model", &req.model, 120)?;
    let key = text("api_key", &req.api_key, 500)?;
    let base = base_url(req.base_url.as_deref())?;
    let secret = secret(&state)?;

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_ai_providers WHERE user_id = $1")
            .bind(user.id)
            .fetch_one(&state.db)
            .await?;
    if count >= MAX_PROVIDERS {
        return Err(AppError::Conflict(format!(
            "не больше {MAX_PROVIDERS} провайдеров"
        )));
    }
    let make_default = req.is_default || count == 0;
    let mut tx = state.db.begin().await?;
    if make_default {
        sqlx::query("UPDATE user_ai_providers SET is_default = false WHERE user_id = $1")
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
    }
    let row: UserAiProvider = sqlx::query_as(&format!(
        "INSERT INTO user_ai_providers
             (user_id, kind, title, base_url, model, api_key, key_hint, is_default)
         VALUES ($1, $2, $3, $4, $5, pgp_sym_encrypt($6, $7), $8, $9)
         RETURNING {COLUMNS}"
    ))
    .bind(user.id)
    .bind(req.kind.as_str())
    .bind(&title)
    .bind(&base)
    .bind(&model)
    .bind(&key)
    .bind(secret)
    .bind(crate::ai::hint(&key))
    .bind(make_default)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// Изменить своего провайдера.
#[utoipa::path(
    patch, operation_id = "update_my_ai_provider", path = "/me/ai-providers/{id}", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("id" = Uuid, Path)),
    request_body = UpdateUserAiProvider,
    responses(
        (status = 200, body = UserAiProvider),
        (status = 400, description = "Неверные поля", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого своего провайдера", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateUserAiProvider>,
) -> AppResult<Json<UserAiProvider>> {
    let title = req
        .title
        .as_deref()
        .map(|t| text("title", t, 60))
        .transpose()?;
    let model = req
        .model
        .as_deref()
        .map(|m| text("model", m, 120))
        .transpose()?;
    let key = req
        .api_key
        .as_deref()
        .map(|k| text("api_key", k, 500))
        .transpose()?;
    let set_base = req.base_url.is_some();
    let base = base_url(req.base_url.as_deref())?;
    let secret = secret(&state)?;

    let mut tx = state.db.begin().await?;
    if req.is_default == Some(true) {
        sqlx::query(
            "UPDATE user_ai_providers SET is_default = false WHERE user_id = $1 AND id <> $2",
        )
        .bind(user.id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    let row: Option<UserAiProvider> = sqlx::query_as(&format!(
        "UPDATE user_ai_providers SET
             title = COALESCE($3, title),
             model = COALESCE($4, model),
             base_url = CASE WHEN $5 THEN $6 ELSE base_url END,
             api_key = CASE WHEN $7::text IS NULL THEN api_key ELSE pgp_sym_encrypt($7, $8) END,
             key_hint = COALESCE($9, key_hint),
             is_default = CASE WHEN $10 THEN true ELSE is_default END
         WHERE id = $1 AND user_id = $2
         RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(user.id)
    .bind(&title)
    .bind(&model)
    .bind(set_base)
    .bind(&base)
    .bind(&key)
    .bind(secret)
    .bind(key.as_deref().map(crate::ai::hint))
    .bind(req.is_default == Some(true))
    .fetch_optional(&mut *tx)
    .await?;
    let row = row.ok_or(AppError::NotFound)?;
    tx.commit().await?;
    Ok(Json(row))
}

/// Удалить своего провайдера. Удалили «по умолчанию» — им становится самый старый оставшийся.
#[utoipa::path(
    delete, operation_id = "delete_my_ai_provider", path = "/me/ai-providers/{id}", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого своего провайдера", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let was_default: Option<bool> = sqlx::query_scalar(
        "DELETE FROM user_ai_providers WHERE id = $1 AND user_id = $2 RETURNING is_default",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?;
    if was_default.ok_or(AppError::NotFound)? {
        sqlx::query(
            "UPDATE user_ai_providers SET is_default = true WHERE id = (
                 SELECT id FROM user_ai_providers WHERE user_id = $1 ORDER BY created_at LIMIT 1)",
        )
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Проверить ключ: короткий запрос к модели.
#[utoipa::path(
    post, operation_id = "test_my_ai_provider", path = "/me/ai-providers/{id}/test", tag = "ai-tutor",
    security(("bearer" = [])),
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = UserAiTest),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого своего провайдера", body = ErrorBody),
    )
)]
pub async fn test(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<UserAiTest>> {
    let provider = resolve(&state, user.id, Some(id)).await?;
    let result = llm::complete(
        &http(),
        &provider,
        "Отвечай одним словом.",
        &[Turn {
            role: "user",
            content: "Скажи: готово".into(),
        }],
        16,
    )
    .await;
    Ok(Json(match result {
        Ok((reply, _)) => UserAiTest {
            ok: true,
            message: reply,
        },
        Err(error) => UserAiTest {
            ok: false,
            message: error,
        },
    }))
}

/// Клиент запросов к моделям: соединение — быстро, ответ — до 2 минут (поток длинный).
pub(super) fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("reqwest client")
}

/// Провайдер пользователя с расшифрованным ключом: указанный или по умолчанию.
/// Нет ни одного — 409 с понятным текстом (фронт ведёт в настройки).
pub(super) async fn resolve(
    state: &AppState,
    user_id: Uuid,
    id: Option<Uuid>,
) -> AppResult<Provider> {
    let secret = secret(state)?;
    let row: Option<(String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT kind, base_url, model, pgp_sym_decrypt(api_key, $3)
         FROM user_ai_providers
         WHERE user_id = $1 AND ($2::uuid IS NULL AND is_default OR id = $2)
         LIMIT 1",
    )
    .bind(user_id)
    .bind(id)
    .bind(secret)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| AppError::Unavailable("не удалось расшифровать ключ ИИ".into()))?;
    let (kind, base_url, model, key) = row.ok_or_else(|| match id {
        Some(_) => AppError::NotFound,
        None => AppError::Conflict("no_ai_provider: добавьте свой ключ ИИ в настройках".into()),
    })?;
    Ok(Provider {
        kind,
        base_url,
        model,
        key,
    })
}
