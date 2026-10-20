//! Свои комнаты чата (этап 3): создаёт, меняет и удаляет admin, пользователи вступают и выходят.
//!
//! Вступление — как интерес: комната попадает в «мои чаты» (`GET /chats`), канал `room:<slug>`
//! подписывается сам (`interest.added` / `interest.removed` с `room_id`). Писать в комнату, как и в
//! остальные, может любой вошедший; сообщения и история — общие (`chat`, `/chat/room/{slug}/...`).

use crate::chat::{custom_room_id, CustomRoom, CUSTOM_COLUMNS};
use crate::events;
use crate::models::PageQuery;
use crate::validate::{self, MAX_ROOM_DESCRIPTION, MAX_ROOM_TITLE};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AdminUser, AppError, AppResult, AppState, AuthUser};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateRoom {
    /// a-z, 0-9 и дефисы между ними, 1–80: адрес `room:<slug>`.
    #[schema(example = "cyberpunk-fans")]
    pub slug: String,
    /// 1–100 символов.
    #[schema(example = "Киберпанк")]
    pub title: String,
    /// До 500 символов.
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateRoom {
    /// 1–100 символов; не передан — не меняется.
    pub title: Option<String>,
    /// До 500 символов; пустая строка — убрать; не передан — не меняется.
    pub description: Option<String>,
}

async fn room(state: &AppState, id: Uuid) -> AppResult<CustomRoom> {
    sqlx::query_as(&format!(
        "SELECT {CUSTOM_COLUMNS} FROM chat_rooms r WHERE r.id = $1 AND r.slug IS NOT NULL"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Свои комнаты: больше участников — выше.
#[utoipa::path(
    get, operation_id = "list_chat_rooms", path = "/chat/rooms", tag = "chat",
    params(PageQuery),
    responses((status = 200, description = "Страница комнат", body = Page<CustomRoom>))
)]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<CustomRoom>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM chat_rooms WHERE slug IS NOT NULL")
        .fetch_one(&state.db)
        .await?;
    let items = sqlx::query_as(&format!(
        "SELECT {CUSTOM_COLUMNS} FROM chat_rooms r WHERE r.slug IS NOT NULL
         ORDER BY members_count DESC, r.created_at, r.id LIMIT $1 OFFSET $2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Своя комната по slug.
#[utoipa::path(
    get, operation_id = "get_chat_room", path = "/chat/rooms/{slug}", tag = "chat",
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 200, description = "Комната", body = CustomRoom),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<CustomRoom>> {
    let id = custom_room_id(&state.db, &slug).await?;
    Ok(Json(room(&state, id).await?))
}

/// Вступить в комнату → 204 (повторно — тоже 204): она в «моих чатах», канал подписан.
#[utoipa::path(
    put, operation_id = "join_chat_room", path = "/chat/rooms/{slug}/membership", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Участник"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn join(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let id = custom_room_id(&state.db, &slug).await?;
    let added = sqlx::query(
        "INSERT INTO chat_room_members (user_id, room_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(validate::missing_reference)?;
    if added.rows_affected() > 0 {
        events::publish(
            &state,
            events::interest("interest.added", user.id, "room_id", id),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Выйти из комнаты → 204 (даже если не был участником).
#[utoipa::path(
    delete, operation_id = "leave_chat_room", path = "/chat/rooms/{slug}/membership", tag = "chat",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Не участник"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn leave(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let id = custom_room_id(&state.db, &slug).await?;
    let deleted = sqlx::query("DELETE FROM chat_room_members WHERE user_id = $1 AND room_id = $2")
        .bind(user.id)
        .bind(id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() > 0 {
        events::publish(
            &state,
            events::interest("interest.removed", user.id, "room_id", id),
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Создать свою комнату.
#[utoipa::path(
    post, operation_id = "admin_create_chat_room", path = "/admin/chat/rooms", tag = "social-admin",
    security(("bearer" = [])),
    request_body = CreateRoom,
    responses(
        (status = 201, description = "Создана", body = CustomRoom),
        (status = 400, description = "Неверный slug, название или описание", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 409, description = "slug занят", body = ErrorBody),
    )
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    JsonBody(req): JsonBody<CreateRoom>,
) -> AppResult<(StatusCode, Json<CustomRoom>)> {
    let slug = validate::room_slug(&req.slug)?;
    let title = validate::required("title", &req.title, MAX_ROOM_TITLE)?;
    let description = validate::optional("description", req.description, MAX_ROOM_DESCRIPTION)?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO chat_rooms (slug, title, description, created_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (slug) DO NOTHING RETURNING id",
    )
    .bind(&slug)
    .bind(&title)
    .bind(&description)
    .bind(admin.id)
    .fetch_optional(&state.db)
    .await?;
    let id = id.ok_or_else(|| AppError::Conflict(format!("room {slug} already exists")))?;
    Ok((StatusCode::CREATED, Json(room(&state, id).await?)))
}

/// Изменить название или описание своей комнаты (slug не меняется: это адрес канала).
#[utoipa::path(
    patch, operation_id = "admin_update_chat_room", path = "/admin/chat/rooms/{slug}", tag = "social-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    request_body = UpdateRoom,
    responses(
        (status = 200, description = "Изменена", body = CustomRoom),
        (status = 400, description = "Неверное название или описание", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Path(slug): Path<String>,
    JsonBody(req): JsonBody<UpdateRoom>,
) -> AppResult<Json<CustomRoom>> {
    let id = custom_room_id(&state.db, &slug).await?;
    let title = req
        .title
        .map(|title| validate::required("title", &title, MAX_ROOM_TITLE))
        .transpose()?;
    let change_description = req.description.is_some();
    let description = validate::optional("description", req.description, MAX_ROOM_DESCRIPTION)?;
    sqlx::query(
        "UPDATE chat_rooms SET title = coalesce($2, title),
             description = CASE WHEN $3 THEN $4 ELSE description END
         WHERE id = $1",
    )
    .bind(id)
    .bind(&title)
    .bind(change_description)
    .bind(&description)
    .execute(&state.db)
    .await?;
    events::publish(&state, events::chat_room_changed("chat.room.updated", id));
    Ok(Json(room(&state, id).await?))
}

/// Удалить свою комнату вместе с сообщениями и участниками → 204.
#[utoipa::path(
    delete, operation_id = "admin_delete_chat_room", path = "/admin/chat/rooms/{slug}", tag = "social-admin",
    security(("bearer" = [])),
    params(("slug" = String, Path, description = "slug комнаты", example = "cyberpunk-fans")),
    responses(
        (status = 204, description = "Удалена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Нужна роль admin", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    Path(slug): Path<String>,
) -> AppResult<StatusCode> {
    let id = custom_room_id(&state.db, &slug).await?;
    sqlx::query("DELETE FROM chat_rooms WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    events::publish(&state, events::chat_room_changed("chat.room.deleted", id));
    Ok(StatusCode::NO_CONTENT)
}
