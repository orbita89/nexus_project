//! Админка статических страниц фронтенда: готовые HTML и `__data.json` карточек, которые nginx
//! отдаёт с диска (ISR, `shared::isr`). Пересобирает их сервер фронтенда; бэкенд только передаёт
//! команду (`POST /_isr/revalidate`) и читает ход (`GET /_isr/status`), по токену `ISR_SECRET`.
//! Своего состояния тут нет: ход живёт в памяти сервера фронтенда.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use shared::error::ErrorBody;
use shared::extract::JsonBody;
use shared::isr::IsrError;
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::ToSchema;

/// Разделы со статикой карточек (`kind` сущности).
const KINDS: [&str; 5] = ["movie", "series", "book", "game", "painting"];

/// Что пересобрать.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegenerateStatic {
    /// Раздел: `book`, `movie`, `series`, `game`, `painting`. Не задан — вся статика.
    #[schema(example = "book")]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegenerateStarted {
    /// `false` — пересборка уже идёт, вторая не запущена.
    pub started: bool,
}

/// Страницы для перерисовки сразу (до ответа).
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegeneratePages {
    /// Адреса страниц: `/books/belaya-gvardiya`. 1–100.
    #[schema(example = json!(["/books/belaya-gvardiya"]))]
    pub paths: Vec<String>,
}

/// Страница, которая не перерисовалась.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StaticFailure {
    pub path: String,
    pub status: u16,
}

/// Ход последней пересборки статики (в памяти сервера фронтенда, с его перезапуска).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StaticRun {
    /// Раздел; `null` — вся статика.
    pub kind: Option<String>,
    /// Страниц в работе (каталог из API + файлы на диске).
    pub total: u64,
    pub done: u64,
    /// Сколько страниц действительно изменилось (остальные совпали байт в байт и не тронуты).
    pub changed: u64,
    /// Первые 100 ошибок.
    pub failed: Vec<StaticFailure>,
    pub started_at: String,
    /// `null` — ещё идёт.
    pub finished_at: Option<String>,
    /// Пересборка прервалась целиком (например, API каталога не ответил).
    pub error: Option<String>,
    pub running: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StaticStatus {
    /// `null` — с запуска сервера фронтенда пересборок не было.
    pub run: Option<StaticRun>,
}

fn isr_error(e: IsrError) -> AppError {
    match e {
        IsrError::Disabled => AppError::Unavailable("ISR не настроен (ISR_URL, ISR_SECRET)".into()),
        other => AppError::Unavailable(format!("сервер фронтенда: {other}")),
    }
}

/// Пересобрать статику раздела (или всю) в фоне. Ход — `GET /admin/static/status`.
#[utoipa::path(
    post, operation_id = "regenerate_static", path = "/admin/static/regenerate", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = RegenerateStatic,
    responses(
        (status = 202, description = "Запущено (или уже идёт: started = false)", body = RegenerateStarted),
        (status = 400, description = "Неизвестный kind", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "ISR не настроен или фронтенд не отвечает", body = ErrorBody),
    )
)]
pub async fn regenerate(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(body): JsonBody<RegenerateStatic>,
) -> AppResult<(StatusCode, Json<RegenerateStarted>)> {
    if let Some(kind) = &body.kind {
        if !KINDS.contains(&kind.as_str()) {
            return Err(AppError::Invalid {
                field: "kind".into(),
                message: format!("один из: {}", KINDS.join(", ")),
            });
        }
    }
    let started = state
        .isr
        .revalidate_kind(body.kind.as_deref())
        .await
        .map_err(isr_error)?;
    tracing::info!(admin_id = %admin.id, kind = ?body.kind, started, "static pages regeneration");
    Ok((StatusCode::ACCEPTED, Json(RegenerateStarted { started })))
}

/// Перерисовать несколько страниц сразу (до ответа): одну книгу после ручной проверки и т.п.
#[utoipa::path(
    post, operation_id = "regenerate_static_pages", path = "/admin/static/pages", tag = "catalog-admin",
    security(("bearer" = [])),
    request_body = RegeneratePages,
    responses(
        (status = 204, description = "Перерисованы"),
        (status = 400, description = "Пустой список или больше 100 адресов", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "ISR не настроен или страница не нарисовалась", body = ErrorBody),
    )
)]
pub async fn regenerate_pages(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    JsonBody(body): JsonBody<RegeneratePages>,
) -> AppResult<StatusCode> {
    if body.paths.is_empty() || body.paths.len() > shared::isr::PATHS_PER_REQUEST {
        return Err(AppError::Invalid {
            field: "paths".into(),
            message: "1–100 адресов".into(),
        });
    }
    if !body.paths.iter().all(|p| p.starts_with('/')) {
        return Err(AppError::Invalid {
            field: "paths".into(),
            message: "адреса страниц начинаются с /".into(),
        });
    }
    state.isr.revalidate(&body.paths).await.map_err(isr_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Ход последней пересборки статики.
#[utoipa::path(
    get, operation_id = "static_status", path = "/admin/static/status", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, body = StaticStatus),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 503, description = "ISR не настроен или фронтенд не отвечает", body = ErrorBody),
    )
)]
pub async fn status(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
) -> AppResult<Json<StaticStatus>> {
    let raw = state.isr.status().await.map_err(isr_error)?;
    let status = serde_json::from_value(raw)
        .map_err(|e| AppError::Internal(format!("ответ /_isr/status: {e}")))?;
    Ok(Json(status))
}
