//! Подписки на пользователей. Списки открыты всем, подписаться — любой вошедший.

use crate::models::{Follow, PageQuery};
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

/// Подписаться на пользователя. Повторная подписка ничего не меняет.
#[utoipa::path(
    put, path = "/users/{username}/follow", tag = "follows",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "На кого подписаться", example = "author")),
    responses(
        (status = 204, description = "Подписан"),
        (status = 400, description = "Подписка на себя", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn follow(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let followee = refs::user(&state, &username).await?;
    if followee.id == user.id {
        return Err(AppError::BadRequest("cannot follow yourself".into()));
    }
    sqlx::query(
        "INSERT INTO follows (follower_id, followee_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(followee.id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Отписаться. Если подписки не было — тоже 204.
#[utoipa::path(
    delete, path = "/users/{username}/follow", tag = "follows",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "От кого отписаться", example = "author")),
    responses(
        (status = 204, description = "Не подписан"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Пользователь не найден или заблокирован", body = ErrorBody),
    )
)]
pub async fn unfollow(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let followee = refs::user(&state, &username).await?;
    sqlx::query("DELETE FROM follows WHERE follower_id = $1 AND followee_id = $2")
        .bind(user.id)
        .bind(followee.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Подписчики пользователя, новые сверху.
#[utoipa::path(
    get, path = "/users/{username}/followers", tag = "follows",
    params(("username" = String, Path, description = "username", example = "author"), PageQuery),
    responses(
        (status = 200, description = "Страница подписчиков", body = Page<Follow>),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn followers(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Follow>>> {
    list(&state, &username, query, Direction::Followers).await
}

/// На кого подписан пользователь, новые сверху.
#[utoipa::path(
    get, path = "/users/{username}/following", tag = "follows",
    params(("username" = String, Path, description = "username", example = "user"), PageQuery),
    responses(
        (status = 200, description = "Страница подписок", body = Page<Follow>),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn following(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<Follow>>> {
    list(&state, &username, query, Direction::Following).await
}

enum Direction {
    Followers,
    Following,
}

async fn list(
    state: &AppState,
    username: &str,
    query: PageQuery,
    direction: Direction,
) -> AppResult<Json<Page<Follow>>> {
    let user = refs::user(state, username).await?;
    let (limit, offset) = page_bounds(query.limit, query.offset);
    // (кого показываем, по какому полю ищем)
    let (other, this) = match direction {
        Direction::Followers => ("follower_id", "followee_id"),
        Direction::Following => ("followee_id", "follower_id"),
    };

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM follows WHERE {this} = $1"))
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(&format!(
        "SELECT {other}, created_at FROM follows WHERE {this} = $1
         ORDER BY created_at DESC, {other} LIMIT $2 OFFSET $3"
    ))
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    let users = refs::users(state, rows.iter().map(|(id, _)| *id).collect()).await?;
    let items = rows
        .into_iter()
        .filter_map(|(id, since)| {
            Some(Follow {
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
