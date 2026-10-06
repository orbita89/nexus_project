//! Активные сессии пользователя: список, завершение одной, выход на всех устройствах.

use crate::models::SessionView;
use crate::session;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use shared::error::ErrorBody;
use shared::extract::Path;
use shared::{AppError, AppResult, AppState, AuthUser};
use uuid::Uuid;

/// Мои активные сессии (устройства), новые сверху.
#[utoipa::path(
    get, operation_id = "list_sessions", path = "/sessions", tag = "auth",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Активные сессии", body = Vec<SessionView>),
        (status = 401, description = "Нет токена или он недействителен", body = ErrorBody),
    )
)]
pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<Vec<SessionView>>> {
    let mut sessions: Vec<SessionView> = sqlx::query_as(
        "SELECT id, user_agent, ip::text AS ip, created_at, expires_at FROM refresh_tokens
         WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()
         ORDER BY created_at DESC",
    )
    .bind(auth.id)
    .fetch_all(&state.db)
    .await?;
    for session in &mut sessions {
        session.current = session.id == auth.session_id;
    }
    Ok(Json(sessions))
}

/// Завершить одну свою сессию (выйти на конкретном устройстве).
#[utoipa::path(
    delete, path = "/sessions/{id}", tag = "auth",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сессии из списка")),
    responses(
        (status = 204, description = "Сессия завершена"),
        (status = 401, description = "Нет токена или он недействителен", body = ErrorBody),
        (status = 404, description = "Нет такой активной сессии у текущего пользователя", body = ErrorBody),
    )
)]
pub async fn revoke(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(session_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let result = sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(session_id)
    .bind(auth.id)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Выйти на всех устройствах, включая текущее.
#[utoipa::path(
    post, path = "/logout-all", tag = "auth",
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Все сессии завершены"),
        (status = 401, description = "Нет токена или он недействителен", body = ErrorBody),
    )
)]
pub async fn logout_all(State(state): State<AppState>, auth: AuthUser) -> AppResult<StatusCode> {
    let mut conn = state.db.acquire().await?;
    let revoked = session::revoke_all(&mut conn, auth.id, None).await?;
    tracing::info!(user_id = %auth.id, revoked, "logged out everywhere");
    Ok(StatusCode::NO_CONTENT)
}
