//! Админские эндпоинты: доступны только роли `admin` (extractor `AdminUser`).

use crate::models::{ListUsersQuery, SetRoleRequest, SetStatusRequest, UserView, USER_COLUMNS};
use crate::session;
use axum::extract::State;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AdminUser, AppError, AppResult, AppState, Role};
use uuid::Uuid;

const MAX_PAGE_SIZE: i64 = 100;

/// Список пользователей, старые сверху.
#[utoipa::path(
    get, path = "/admin/users", tag = "admin",
    security(("bearer" = [])),
    params(ListUsersQuery),
    responses(
        (status = 200, description = "Пользователи", body = Vec<UserView>),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
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

/// Сменить роль. Новая роль попадёт в токены при следующем refresh (не позже чем через 15 минут).
#[utoipa::path(
    patch, path = "/admin/users/{id}/role", tag = "admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id пользователя")),
    request_body = SetRoleRequest,
    responses(
        (status = 200, description = "Роль изменена", body = UserView),
        (status = 400, description = "Админ не может понизить сам себя", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn set_role(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(user_id): Path<Uuid>,
    JsonBody(req): JsonBody<SetRoleRequest>,
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

/// Заблокировать или разблокировать пользователя.
///
/// При блокировке все его сессии сразу отзываются: refresh перестаёт работать, а access-токен
/// доживает не больше 15 минут.
#[utoipa::path(
    patch, path = "/admin/users/{id}/status", tag = "admin",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id пользователя")),
    request_body = SetStatusRequest,
    responses(
        (status = 200, description = "Статус изменён", body = UserView),
        (status = 400, description = "Админ не может заблокировать сам себя", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Пользователь не найден", body = ErrorBody),
    )
)]
pub async fn set_status(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(user_id): Path<Uuid>,
    JsonBody(req): JsonBody<SetStatusRequest>,
) -> AppResult<Json<UserView>> {
    if user_id == admin.id && !req.is_active {
        return Err(AppError::BadRequest(
            "admins cannot block themselves".into(),
        ));
    }

    let mut tx = state.db.begin().await?;
    let user: Option<UserView> = sqlx::query_as(&format!(
        "UPDATE users SET is_active = $2 WHERE id = $1 RETURNING {USER_COLUMNS}"
    ))
    .bind(user_id)
    .bind(req.is_active)
    .fetch_optional(&mut *tx)
    .await?;
    let user = user.ok_or(AppError::NotFound)?;
    if !req.is_active {
        session::revoke_all(&mut tx, user_id, None).await?;
    }
    tx.commit().await?;

    tracing::info!(admin_id = %admin.id, %user_id, is_active = req.is_active, "user status changed");
    Ok(Json(user))
}
