//! Админские эндпоинты: доступны только роли `admin` (extractor `AdminUser`).

use crate::models::{ListUsersQuery, SetRoleRequest, SetStatusRequest, UserView, USER_COLUMNS};
use crate::session;
use axum::extract::State;
use axum::Json;
use shared::db::like_pattern;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::Page;
use shared::{AdminUser, AppError, AppResult, AppState, Role};
use uuid::Uuid;

const MAX_PAGE_SIZE: i64 = 100;

/// Фильтр списка: `$1` — шаблон ILIKE или `NULL`.
const USERS_FILTER: &str =
    "FROM users WHERE $1::text IS NULL OR username::text ILIKE $1 OR email::text ILIKE $1";

/// Список пользователей массивом, старые сверху. Страница с `total` —
/// `GET /admin/users/page`.
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
    Ok(Json(users_page(&state, query).await?.items))
}

/// Страница пользователей с `total`, старые сверху; `q` — поиск по username и email.
#[utoipa::path(
    get, operation_id = "list_users_page", path = "/admin/users/page", tag = "admin",
    security(("bearer" = [])),
    params(ListUsersQuery),
    responses(
        (status = 200, description = "Страница пользователей", body = Page<UserView>),
        (status = 400, description = "Неверный параметр", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
    )
)]
pub async fn list_users_page(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<ListUsersQuery>,
) -> AppResult<Json<Page<UserView>>> {
    Ok(Json(users_page(&state, query).await?))
}

async fn users_page(state: &AppState, query: ListUsersQuery) -> AppResult<Page<UserView>> {
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_PAGE_SIZE);
    let offset = query.offset.unwrap_or(0).max(0);
    let pattern = query.q.as_deref().and_then(like_pattern);

    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) {USERS_FILTER}"))
        .bind(&pattern)
        .fetch_one(&state.db)
        .await?;
    let items = sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS} {USERS_FILTER} ORDER BY created_at, id LIMIT $2 OFFSET $3"
    ))
    .bind(&pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Page {
        items,
        total,
        limit,
        offset,
    })
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
