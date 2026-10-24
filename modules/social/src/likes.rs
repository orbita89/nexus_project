//! Лайки профилей. Только лайк: дизлайка у пользователя нет — оценивают человека, а не
//! рецензию, и минус здесь работал бы как травля. Себе поставить нельзя.
//!
//! Счётчик и «лайкнул ли я» приходят в шапке профиля (`GET /users/{username}`) — отдельного
//! запроса под кнопку не нужно. Список лайкнувших открыт всем.

use crate::models::{Like, PageQuery};
use crate::refs;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use shared::error::ErrorBody;
use shared::extract::{Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use uuid::Uuid;

/// Лайкнуть профиль. Повторный лайк ничего не меняет.
#[utoipa::path(
    put, operation_id = "like_user", path = "/users/{username}/like", tag = "users",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "Кого лайкнуть", example = "author")),
    responses(
        (status = 204, description = "Лайкнут"),
        (status = 400, description = "Лайк самому себе (`like_self`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn like(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let liked = refs::user(&state, &username).await?;
    if liked.id == user.id {
        return Err(AppError::BadRequest("cannot like yourself".into()).with_code("like_self"));
    }
    sqlx::query(
        "INSERT INTO user_likes (user_id, liked_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(liked.id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять свой лайк. Если его не было — тоже 204.
#[utoipa::path(
    delete, operation_id = "unlike_user", path = "/users/{username}/like", tag = "users",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "С кого снять лайк", example = "author")),
    responses(
        (status = 204, description = "Лайка нет"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn unlike(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let liked = refs::user(&state, &username).await?;
    sqlx::query("DELETE FROM user_likes WHERE user_id = $1 AND liked_id = $2")
        .bind(user.id)
        .bind(liked.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Кто лайкнул профиль, новые сверху.
#[utoipa::path(
    get, operation_id = "list_user_likes", path = "/users/{username}/likes", tag = "users",
    params(
        ("username" = String, Path, description = "username", example = "author"),
        PageQuery,
    ),
    responses(
        (status = 200, description = "Страница лайкнувших", body = Page<Like>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Like>>> {
    let liked = refs::user(&state, &username).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);

    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM user_likes WHERE liked_id = $1")
        .bind(liked.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT user_id, created_at FROM user_likes WHERE liked_id = $1
         ORDER BY created_at DESC, user_id LIMIT $2 OFFSET $3",
    )
    .bind(liked.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    // Пользователи — одним вызовом справочника; удалённый прямо сейчас просто выпадает.
    let users = refs::users(&state, rows.iter().map(|(id, _)| *id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|(id, since)| {
            Some(Like {
                user: users.get(&id)?.clone(),
                since,
            })
        })
        .collect();
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}
