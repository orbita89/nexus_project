//! Админка скриптов (`catalog::scripts`): реестр с описаниями и флагами, запуск, лог, отмена,
//! отчёт. Запускается только то, что есть в реестре, с проверенными флагами.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AdminUser, AppError, AppResult, AppState};
use utoipa::{IntoParams, ToSchema};

use crate::scripts::{self, JobInfo, JobLog, Script};

/// Значения флагов: `{"limit": 5000, "yes": true, "ai": "deepseek"}`. `null`, `""`, `false` —
/// флаг не передаётся.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct RunScript {
    #[serde(default)]
    #[schema(value_type = std::collections::HashMap<String, serde_json::Value>)]
    pub flags: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    /// Строки лога с этого номера (с 0) — для догрузки: `lines_total` прошлого ответа.
    pub after: Option<u64>,
}

#[utoipa::path(
    get, operation_id = "list_admin_scripts", path = "/admin/scripts", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Скрипты: описание, команда для терминала, флаги", body = Vec<Script>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list(_admin: AdminUser) -> Json<&'static [Script]> {
    Json(scripts::SCRIPTS)
}

/// Запустить скрипт в фоне. Одновременно — одна задача.
#[utoipa::path(
    post, operation_id = "run_admin_script", path = "/admin/scripts/{id}/run", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "id скрипта из реестра")),
    request_body = RunScript,
    responses(
        (status = 202, description = "Запущен", body = JobInfo),
        (status = 400, description = "Неизвестный флаг или значение не подходит (field — флаг)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такого скрипта", body = ErrorBody),
        (status = 409, description = "Уже идёт другая задача", body = ErrorBody),
        (status = 503, description = "Не задан SCRIPTS_PYTHON", body = ErrorBody),
    )
)]
pub async fn run(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<RunScript>,
) -> AppResult<(StatusCode, Json<JobInfo>)> {
    let script = scripts::find(&id).ok_or(AppError::NotFound)?;
    let args = scripts::build_args(script, &req.flags)?;
    let job = scripts::start(&state, script, args)?;
    tracing::info!(admin_id = %admin.id, script = %id, job = job.id, args = ?job.args, "admin script started");
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Отчёт скрипта (у чистки книг — `apply_report.txt` сухого прогона), текст.
#[utoipa::path(
    get, operation_id = "admin_script_report", path = "/admin/scripts/{id}/report", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "Текст отчёта (хвост до 512 КБ)", body = String, content_type = "text/plain"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет скрипта, у него нет отчёта или отчёт ещё не создан", body = ErrorBody),
    )
)]
pub async fn report(_admin: AdminUser, Path(id): Path<String>) -> AppResult<String> {
    let script = scripts::find(&id).ok_or(AppError::NotFound)?;
    tokio::task::spawn_blocking(move || scripts::report(script))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or(AppError::NotFound)
}

/// Прогресс: у чистки книг — `{total, classified, written, remaining, pending_write,
/// exported_not_classified, last_batch, batches_written, updated_at}`.
#[utoipa::path(
    get, operation_id = "admin_script_progress", path = "/admin/scripts/{id}/progress", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "Прогресс (как записал скрипт)", body = Object),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет скрипта или прогресса ещё нет", body = ErrorBody),
    )
)]
pub async fn progress(
    _admin: AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let script = scripts::find(&id).ok_or(AppError::NotFound)?;
    tokio::task::spawn_blocking(move || scripts::progress(script))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[utoipa::path(
    get, operation_id = "list_admin_script_jobs", path = "/admin/scripts/jobs", tag = "catalog-admin",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Последние задачи этого процесса, новые первыми", body = Vec<JobInfo>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn jobs(_admin: AdminUser) -> Json<Vec<JobInfo>> {
    Json(scripts::list_jobs())
}

/// Задача и её лог (со строки `after`): опрашивать, пока `status = running`.
#[utoipa::path(
    get, operation_id = "get_admin_script_job", path = "/admin/scripts/jobs/{job}", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("job" = u64, Path), LogQuery),
    responses(
        (status = 200, description = "Задача и строки лога", body = JobLog),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой задачи (или сервер перезапущен)", body = ErrorBody),
    )
)]
pub async fn job(
    _admin: AdminUser,
    Path(job): Path<u64>,
    Query(query): Query<LogQuery>,
) -> AppResult<Json<JobLog>> {
    scripts::job_log(job, query.after.unwrap_or(0))
        .map(Json)
        .ok_or(AppError::NotFound)
}

#[utoipa::path(
    post, operation_id = "cancel_admin_script_job", path = "/admin/scripts/jobs/{job}/cancel", tag = "catalog-admin",
    security(("bearer" = [])),
    params(("job" = u64, Path)),
    responses(
        (status = 202, description = "Остановка запрошена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой идущей задачи", body = ErrorBody),
    )
)]
pub async fn cancel(AdminUser(admin): AdminUser, Path(job): Path<u64>) -> AppResult<StatusCode> {
    if !scripts::cancel(job) {
        return Err(AppError::NotFound);
    }
    tracing::info!(admin_id = %admin.id, job, "admin script cancel requested");
    Ok(StatusCode::ACCEPTED)
}
