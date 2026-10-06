//! Интересы: на какие сущности подписан пользователь. Список личный — видит только владелец.
//! По интересам `realtime` подписывает соединения пользователя на каналы сущностей; позже из них
//! строится лента.

use crate::events;
use crate::models::{Interest, InterestRow, PageQuery};
use crate::refs;
use crate::validate::{self, MAX_INTERESTS};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};

/// Свои интересы, новые сверху.
#[utoipa::path(
    get, path = "/interests", tag = "interests",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница интересов", body = Page<Interest>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Interest>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM user_interests WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<InterestRow> = sqlx::query_as(
        "SELECT entity_id, created_at FROM user_interests WHERE user_id = $1
         ORDER BY created_at DESC, entity_id LIMIT $2 OFFSET $3",
    )
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items: refs::interests(&state, rows).await?,
        total,
        limit,
        offset,
    }))
}

/// Есть ли сущность в своих интересах: кнопка «Следить» на карточке.
#[utoipa::path(
    get, path = "/entities/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 200, description = "В интересах", body = Interest),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена или её нет в интересах", body = ErrorBody),
    )
)]
pub async fn get_own(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<Json<Interest>> {
    let entity = refs::entity(&state, &slug).await?;
    let since: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT created_at FROM user_interests WHERE user_id = $1 AND entity_id = $2",
    )
    .bind(user.id)
    .bind(entity.id)
    .fetch_optional(&state.db)
    .await?;
    let since = since.ok_or(AppError::NotFound)?;
    Ok(Json(Interest { entity, since }))
}

/// Добавить сущность в интересы → 204 (повторно — тоже 204).
#[utoipa::path(
    put, path = "/entities/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "В интересах"),
        (status = 400, description = "Уже 500 интересов", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn add(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    let mut tx = state.db.begin().await?;
    // Параллельные добавления одного пользователя идут по очереди: лимит не превысить.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM user_interests WHERE user_id = $1 AND entity_id = $2)",
    )
    .bind(user.id)
    .bind(entity.id)
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        return Ok(StatusCode::NO_CONTENT);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM user_interests WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_INTERESTS {
        return Err(AppError::BadRequest(format!(
            "no more than {MAX_INTERESTS} interests"
        )));
    }
    sqlx::query("INSERT INTO user_interests (user_id, entity_id) VALUES ($1, $2)")
        .bind(user.id)
        .bind(entity.id)
        .execute(&mut *tx)
        .await
        .map_err(validate::missing_reference)?;
    tx.commit().await?;
    events::publish(
        &state,
        events::interest("interest.added", user.id, entity.id),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Убрать сущность из интересов → 204 (даже если её там не было).
#[utoipa::path(
    delete, path = "/entities/{slug}/interest", tag = "interests",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug сущности", example = "dune-2021")),
    responses(
        (status = 204, description = "Не в интересах"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Сущность не найдена", body = ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let entity = refs::entity(&state, &slug).await?;
    let deleted = sqlx::query("DELETE FROM user_interests WHERE user_id = $1 AND entity_id = $2")
        .bind(user.id)
        .bind(entity.id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() > 0 {
        events::publish(
            &state,
            events::interest("interest.removed", user.id, entity.id),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}
