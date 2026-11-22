//! Админка провайдеров ИИ (`catalog::ai`): ключ, адрес, модель, лимиты. Ключи пишутся и не
//! читаются — в ответах только подсказка `sk-…a1b2`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path};
use shared::{AdminUser, AppResult, AppState};
use utoipa::ToSchema;

use crate::ai::{self, AiProvider, AiProviderInput, AiTest};

/// Новый провайдер.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAiProvider {
    #[schema(example = "deepseek")]
    pub slug: String,
    #[serde(flatten)]
    pub fields: AiProviderInput,
}

#[utoipa::path(
    get, operation_id = "list_ai_providers", path = "/admin/ai/providers", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Провайдеры ИИ, по умолчанию — первым", body = Vec<AiProvider>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> AppResult<Json<Vec<AiProvider>>> {
    Ok(Json(ai::list(&state).await?))
}

/// Завести провайдера. Первый становится провайдером по умолчанию.
#[utoipa::path(
    post, operation_id = "create_ai_provider", path = "/admin/ai/providers", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = CreateAiProvider,
    responses(
        (status = 201, description = "Заведён", body = AiProvider),
        (status = 400, description = "Нет kind/model/api_keys, лимиты вне диапазона", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "Такой slug уже есть", body = ErrorBody),
        (status = 503, description = "Не задан AI_SECRET_KEY", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateAiProvider>,
) -> AppResult<(StatusCode, Json<AiProvider>)> {
    let provider = ai::create(&state, &req.slug, req.fields).await?;
    tracing::info!(admin_id = %admin.id, slug = %provider.slug, "ai provider created");
    Ok((StatusCode::CREATED, Json(provider)))
}

/// Поменять переданные поля; `api_keys` заменяют прежние ключи целиком.
#[utoipa::path(
    patch, operation_id = "update_ai_provider", path = "/admin/ai/providers/{slug}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path)),
    request_body = AiProviderInput,
    responses(
        (status = 200, description = "Обновлён", body = AiProvider),
        (status = 400, description = "Поле вне диапазона", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого провайдера", body = ErrorBody),
        (status = 503, description = "Не задан AI_SECRET_KEY (при смене ключей)", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<AiProviderInput>,
) -> AppResult<Json<AiProvider>> {
    let provider = ai::update(&state, &slug, req).await?;
    tracing::info!(admin_id = %admin.id, %slug, "ai provider updated");
    Ok(Json(provider))
}

#[utoipa::path(
    delete, operation_id = "delete_ai_provider", path = "/admin/ai/providers/{slug}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path)),
    responses(
        (status = 204, description = "Удалён"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого провайдера", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    ai::delete(&state, &slug).await?;
    tracing::info!(admin_id = %admin.id, %slug, "ai provider deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Пробный запрос «ответь OK» на каждый ключ: проверить адрес, модель и ключи.
#[utoipa::path(
    post, operation_id = "test_ai_provider", path = "/admin/ai/providers/{slug}/test", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path)),
    responses(
        (status = 200, description = "Ответ по каждому ключу", body = AiTest),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого провайдера", body = ErrorBody),
        (status = 503, description = "Не задан или не тот AI_SECRET_KEY", body = ErrorBody),
    )
)]
pub async fn test(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(slug): Path<String>,
) -> AppResult<Json<AiTest>> {
    Ok(Json(ai::test(&state, &slug).await?))
}
