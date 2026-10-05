//! Админские эндпоинты: доступны только роли `admin` (extractor `AdminUser`).

use crate::models::{ListUsersQuery, SetRoleRequest, UserView, USER_COLUMNS};
use axum::extract::{Path, Query, State};
use axum::Json;
use shared::{AdminUser, AppError, AppResult, AppState, Role};
use uuid::Uuid;

const MAX_PAGE_SIZE: i64 = 100;

pub async fn list_users(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<ListUsersQuery>,
) -> AppResult<Json<Vec<UserView>>> {
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_PAGE_SIZE);
    let offset = query.offset.unwrap_or(0).max(0);

    let users = sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY created_at, id LIMIT $1 OFFSET $2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(users))
}

/// Новая роль попадёт в токены при следующем refresh, то есть не позже чем через 15 минут.
pub async fn set_role(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(user_id): Path<Uuid>,
    Json(req): Json<SetRoleRequest>,
) -> AppResult<Json<UserView>> {
    // Защита от ситуации «последний админ разжаловал сам себя».
    if user_id == admin.id && req.role != Role::Admin {
        return Err(AppError::BadRequest(
            "admins cannot demote themselves".into(),
        ));
    }

    let user: Option<UserView> = sqlx::query_as(&format!(
        "UPDATE users SET role = $2 WHERE id = $1 RETURNING {USER_COLUMNS}"
    ))
    .bind(user_id)
    .bind(req.role)
    .fetch_optional(&state.db)
    .await?;
    let user = user.ok_or(AppError::NotFound)?;

    tracing::info!(admin_id = %admin.id, %user_id, role = ?req.role, "user role changed");
    Ok(Json(user))
}
