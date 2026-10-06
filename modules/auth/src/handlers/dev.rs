//! Вход для разработчиков и тестировщиков: под любым пользователем без пароля.
//! Работает только при `DEV_LOGIN=true`, иначе эндпоинт отвечает 404, как будто его нет.

use crate::models::{TokenResponse, USER_COLUMNS};
use crate::session::{self, ClientInfo};
use crate::{users, validate};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::{AppError, AppResult, AppState, Role};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub struct DevLoginRequest {
    /// Email или username. Неизвестный email — пользователь создаётся (email подтверждён, без пароля).
    #[schema(example = "author")]
    pub login: String,
    /// Выдать пользователю эту роль перед входом.
    pub role: Option<Role>,
}

/// **Только для разработки.** Вход под любым пользователем без пароля и подтверждения email.
///
/// Включается `DEV_LOGIN=true` (в dev-окружении включено). Выключенный — 404.
#[utoipa::path(
    post, path = "/dev/login", tag = "dev",
    request_body = DevLoginRequest,
    responses(
        (status = 200, description = "Пара токенов", body = TokenResponse),
        (status = 401, description = "Пользователь заблокирован", body = ErrorBody),
        (status = 404, description = "Dev login выключен, или username не найден", body = ErrorBody),
    )
)]
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DevLoginRequest>,
) -> AppResult<Json<TokenResponse>> {
    if !state.config.dev_login {
        return Err(AppError::NotFound);
    }

    let mut tx = state.db.begin().await?;
    let user = match users::by_login_with_hash(&mut tx, &req.login).await? {
        Some(found) => found.user,
        None if req.login.contains('@') => {
            validate::email(req.login.trim())?;
            users::create_external(&mut tx, req.login.trim(), true, None, None).await?
        }
        None => return Err(AppError::NotFound),
    };
    let user = match req.role {
        Some(role) if role != user.role => {
            sqlx::query_as(&format!(
                "UPDATE users SET role = $2 WHERE id = $1 RETURNING {USER_COLUMNS}"
            ))
            .bind(user.id)
            .bind(role)
            .fetch_one(&mut *tx)
            .await?
        }
        _ => user,
    };
    if !user.is_active {
        return Err(AppError::Unauthorized);
    }

    let tokens = session::issue(&state, &mut tx, user, &ClientInfo::from_headers(&headers)).await?;
    tx.commit().await?;
    tracing::warn!(user_id = %tokens.user.id, role = ?tokens.user.role, "dev login");
    Ok(Json(tokens))
}
