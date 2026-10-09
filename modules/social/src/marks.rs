//! Личные отметки «Посмотреть позже» (`later`) и «Просмотрено» (`done`). Взаимоисключающие: одна на
//! сущность. Приватные: видит только сам пользователь, чужие не отдаются никак.

use crate::models::{Mark, MarkKind, MarkRow, MarkedEntity, MarksQuery};
use crate::refs;
use crate::validate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Своя отметка на сущности.
#[utoipa::path(
    get, operation_id = "get_own_mark", path = "/entities/{slug}/mark", tag = "marks",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "Отметка", body = Mark),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или отметки нет", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Mark>> {
    let entity = refs::entity(&state, &slug).await?;
    let kind: Option<MarkKind> =
        sqlx::query_scalar("SELECT kind FROM entity_marks WHERE entity_id = $1 AND user_id = $2")
            .bind(entity.id)
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?;
    let kind = kind.ok_or(AppError::NotFound)?;
    Ok(Json(Mark { kind }))
}

/// Поставить отметку; другая заменяет прежнюю («просмотрено» убирает из «позже»).
#[utoipa::path(
    put, operation_id = "put_mark", path = "/entities/{slug}/mark", tag = "marks",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    request_body = Mark,
    responses(
        (status = 200, description = "Отметка сохранена", body = Mark),
        (status = 400, description = "Неизвестная отметка", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<Mark>,
) -> AppResult<Json<Mark>> {
    let entity = refs::entity(&state, &slug).await?;
    // Смена отметки — новая отметка: в списке «просмотрено» она встаёт сверху.
    sqlx::query(
        "INSERT INTO entity_marks (entity_id, user_id, kind) VALUES ($1, $2, $3)
         ON CONFLICT (entity_id, user_id) DO UPDATE SET kind = EXCLUDED.kind, created_at = now()
         WHERE entity_marks.kind <> EXCLUDED.kind",
    )
    .bind(entity.id)
    .bind(user.id)
    .bind(req.kind)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    Ok(Json(req))
}

/// Снять отметку → 204 (даже если её не было).
#[utoipa::path(
    delete, operation_id = "delete_mark", path = "/entities/{slug}/mark", tag = "marks",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Отметки нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    sqlx::query("DELETE FROM entity_marks WHERE entity_id = $1 AND user_id = $2")
        .bind(entity.id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Свои отметки, свежие сверху: списки «Посмотреть позже» и «Просмотрено» в личном кабинете.
#[utoipa::path(
    get, operation_id = "list_own_marks", path = "/users/me/marks", tag = "marks",
    security(("bearer" = [])),
    params(MarksQuery),
    responses(
        (status = 200, description = "Страница отметок", body = Page<MarkedEntity>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<MarksQuery>,
) -> AppResult<Json<Page<MarkedEntity>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    const FILTER: &str = "FROM entity_marks WHERE user_id = $1 AND ($2::text IS NULL OR kind = $2)";
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {FILTER}"))
        .bind(user.id)
        .bind(query.kind)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<MarkRow> = sqlx::query_as(&format!(
        "SELECT entity_id, kind, created_at {FILTER}
         ORDER BY created_at DESC, entity_id LIMIT $3 OFFSET $4"
    ))
    .bind(user.id)
    .bind(query.kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: refs::marks(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}
