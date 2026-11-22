//! Админка промптов ИИ: посмотреть действующие тексты и переопределить их — для всех сущностей
//! (`default`) или для одного типа (`book`, `movie`, …). Встроенные тексты — в
//! [`crate::tutor::templates`]; удаление переопределения возвращает к общему/встроенному.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::{IntoParams, ToSchema};

use crate::models::EntityKind;
use crate::tutor::prompt::{self, Book, Stored};
use crate::tutor::templates::{self, Placeholder, Templates, DEFAULT_SCOPE, DEFS, MAX_TEMPLATE};

/// Переопределение промпта для области.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromptOverride {
    /// `default` — для всех сущностей, иначе тип: `book`, `movie`, …
    #[schema(example = "book")]
    pub scope: String,
    pub template: String,
    pub updated_at: DateTime<Utc>,
}

/// Промпт: назначение, подстановки, встроенный текст и переопределения.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPrompt {
    #[schema(example = "tutor.system")]
    pub key: String,
    pub title: String,
    pub description: String,
    pub placeholders: Vec<Placeholder>,
    /// Встроенный текст (действует, если переопределений нет).
    pub builtin: String,
    pub overrides: Vec<AiPromptOverride>,
}

/// Числовой предел промпта (меняется только в коде).
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromptLimit {
    pub name: String,
    pub value: i64,
    pub description: String,
}

/// Все промпты ИИ.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPrompts {
    /// Области: `default` и типы сущностей.
    pub scopes: Vec<String>,
    pub prompts: Vec<AiPrompt>,
    pub limits: Vec<AiPromptLimit>,
}

/// Новый текст промпта.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PutAiPrompt {
    pub template: String,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PreviewQuery {
    /// Область: `default` или тип сущности. По умолчанию `book`.
    pub scope: Option<String>,
}

/// Что уйдёт в модель на примере книги.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromptPreview {
    pub scope: String,
    /// Системный промпт (читатель на 30%, есть резюме).
    pub system: String,
    /// Вопрос по фрагменту из читалки.
    pub quote_question: String,
    /// Запрос на сжатие разговора.
    pub summarize: String,
}

fn scopes() -> Vec<String> {
    std::iter::once(DEFAULT_SCOPE)
        .chain(EntityKind::ALL.iter().map(|k| k.as_str()))
        .map(String::from)
        .collect()
}

fn check_scope(scope: &str) -> AppResult<()> {
    if scopes().iter().any(|s| s == scope) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "неизвестная область «{scope}»"
        )))
    }
}

fn limits() -> Vec<AiPromptLimit> {
    let l = |name: &str, value: usize, description: &str| AiPromptLimit {
        name: name.into(),
        value: value as i64,
        description: description.into(),
    };
    vec![
        l("WINDOW", prompt::WINDOW, "последних реплик в запросе"),
        l(
            "SUMMARIZE_AFTER",
            prompt::SUMMARIZE_AFTER,
            "реплик вне окна до пересжатия",
        ),
        l(
            "ANSWER_TOKENS",
            prompt::ANSWER_TOKENS as usize,
            "предел ответа, токенов",
        ),
        l(
            "ANSWER_IN_HISTORY",
            prompt::ANSWER_IN_HISTORY,
            "ответ ИИ в истории, символов",
        ),
        l(
            "MAX_QUOTE",
            prompt::MAX_QUOTE,
            "фрагмент из читалки, символов",
        ),
        l(
            "SUMMARY_CHARS",
            prompt::SUMMARY_CHARS,
            "резюме разговора, символов",
        ),
        l(
            "SUMMARY_TOKENS",
            prompt::SUMMARY_TOKENS as usize,
            "предел ответа на сжатие, токенов",
        ),
    ]
}

#[utoipa::path(
    get, operation_id = "list_ai_prompts", path = "/admin/ai/prompts", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Промпты ИИ: встроенные тексты и переопределения", body = AiPrompts),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(State(state): State<AppState>, _admin: AdminUser) -> AppResult<Json<AiPrompts>> {
    let rows: Vec<(String, String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT key, scope, template, updated_at FROM ai_prompts ORDER BY scope")
            .fetch_all(&state.db)
            .await?;
    let prompts = DEFS
        .iter()
        .map(|d| AiPrompt {
            key: d.key.into(),
            title: d.title.into(),
            description: d.description.into(),
            placeholders: d
                .placeholders
                .iter()
                .map(|(name, description)| Placeholder {
                    name: (*name).into(),
                    description: (*description).into(),
                })
                .collect(),
            builtin: d.builtin.into(),
            overrides: rows
                .iter()
                .filter(|(key, ..)| key == d.key)
                .map(|(_, scope, template, updated_at)| AiPromptOverride {
                    scope: scope.clone(),
                    template: template.clone(),
                    updated_at: *updated_at,
                })
                .collect(),
        })
        .collect();
    Ok(Json(AiPrompts {
        scopes: scopes(),
        prompts,
        limits: limits(),
    }))
}

/// Переопределить промпт для области. Неизвестные подстановки `{…}` — 400.
#[utoipa::path(
    put, operation_id = "put_ai_prompt", path = "/admin/ai/prompts/{key}/{scope}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("key" = String, Path), ("scope" = String, Path)),
    request_body = PutAiPrompt,
    responses(
        (status = 200, description = "Сохранено", body = AiPromptOverride),
        (status = 400, description = "Пустой/длинный текст, неизвестная область или подстановка", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого промпта", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((key, scope)): Path<(String, String)>,
    JsonBody(req): JsonBody<PutAiPrompt>,
) -> AppResult<Json<AiPromptOverride>> {
    let d = templates::def(&key).ok_or(AppError::NotFound)?;
    check_scope(&scope)?;
    let template = req.template;
    if template.trim().is_empty() || template.chars().count() > MAX_TEMPLATE {
        return Err(
            AppError::BadRequest(format!("текст 1–{MAX_TEMPLATE} символов")).on_field("template"),
        );
    }
    let unknown = templates::unknown_placeholders(d, &template);
    if !unknown.is_empty() {
        return Err(AppError::BadRequest(format!(
            "неизвестные подстановки: {{{}}}",
            unknown.join("}, {")
        ))
        .on_field("template"));
    }
    let updated_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO ai_prompts (key, scope, template, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (key, scope) DO UPDATE
             SET template = EXCLUDED.template, updated_by = EXCLUDED.updated_by, updated_at = now()
         RETURNING updated_at",
    )
    .bind(d.key)
    .bind(&scope)
    .bind(&template)
    .bind(admin.id)
    .fetch_one(&state.db)
    .await?;
    tracing::info!(admin_id = %admin.id, key = d.key, %scope, "ai prompt updated");
    Ok(Json(AiPromptOverride {
        scope,
        template,
        updated_at,
    }))
}

/// Убрать переопределение: действует общее или встроенное.
#[utoipa::path(
    delete, operation_id = "delete_ai_prompt", path = "/admin/ai/prompts/{key}/{scope}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("key" = String, Path), ("scope" = String, Path)),
    responses(
        (status = 204, description = "Сброшено"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого переопределения", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path((key, scope)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let done = sqlx::query("DELETE FROM ai_prompts WHERE key = $1 AND scope = $2")
        .bind(&key)
        .bind(&scope)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    tracing::info!(admin_id = %admin.id, %key, %scope, "ai prompt reset");
    Ok(StatusCode::NO_CONTENT)
}

/// Собранные промпты на примере книги — как их увидит модель.
#[utoipa::path(
    get, operation_id = "preview_ai_prompts", path = "/admin/ai/prompts/preview", tag = "catalog-admin",
    security(("bearer" = [])),
    params(PreviewQuery),
    responses(
        (status = 200, description = "Пример собранных промптов", body = AiPromptPreview),
        (status = 400, description = "Неизвестная область", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn preview(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<PreviewQuery>,
) -> AppResult<Json<AiPromptPreview>> {
    let scope = query.scope.unwrap_or_else(|| "book".into());
    check_scope(&scope)?;
    let t = Templates::load(&state.db, &scope).await?;
    let book = Book {
        title: "Мастер и Маргарита".into(),
        authors: vec!["Михаил Булгаков".into()],
        year: Some(1967),
    };
    let sample = |role: &str, content: &str| Stored {
        role: role.into(),
        content: content.into(),
        quote: None,
        chapter: None,
    };
    let (summarize, _) = prompt::summarize(
        &t,
        "",
        &[
            sample("user", "Кто такой Воланд?"),
            sample("assistant", "Сатана, пришедший в Москву 1930-х."),
        ],
    );
    Ok(Json(AiPromptPreview {
        system: prompt::system(
            &t,
            &book,
            Some(0.3),
            "читатель спрашивал о Воланде и его свите",
        ),
        quote_question: prompt::question(
            &t,
            "Почему рукописи не горят?",
            Some("Рукописи не горят"),
            Some("Глава 24. Извлечение мастера"),
        ),
        summarize,
        scope,
    }))
}
