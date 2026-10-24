//! Личные переписки 1:1, блокировки и настройка «кто может написать первым».
//!
//! Переписка — строка `chat_rooms` того же протокола, что и остальные комнаты: канал `dm:<id>`,
//! та же история (`GET /chat/dm/{id}/messages`), та же отправка по WebSocket, те же
//! `chat.message.*`. Пара хранится канонически (`dm_user_a < dm_user_b`), поэтому на двоих
//! ровно одна строка и повторный `PUT /users/{username}/dm` возвращает её же.
//!
//! Отличие от публичных комнат одно и важное: **читать и писать могут только двое**. Проверка —
//! [`member`]; её зовут история, панель комнаты и отправка.
//!
//! «Убрать переписку у себя» — та же `chat_room_hidden`, что и у системных комнат: строка
//! пропадает из `GET /chats`, история цела, новое сообщение возвращает её обратно.

use crate::chat::{self, ChatRoomInfo};
use crate::events;
use crate::models::PageQuery;
use crate::refs;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use shared::directory::UserRef;
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::pagination::{page_bounds, Page};
use shared::{AppError, AppResult, AppState, AuthUser};
use utoipa::ToSchema;
use uuid::Uuid;

/// Сколько переписок можно завести за сутки: защита от рассылок. Ответ в существующую
/// переписку под лимит не попадает.
pub const MAX_NEW_DMS_PER_DAY: i64 = 20;

/// Кто может начать переписку с пользователем.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    /// Любой вошедший с подтверждённым email.
    #[default]
    All,
    /// Только те, на кого он подписан сам.
    Following,
    /// Никто.
    None,
}

impl DmPolicy {
    fn from_db(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("following") => Self::Following,
            Some("none") => Self::None,
            _ => Self::All,
        }
    }

    fn as_db(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Following => "following",
            Self::None => "none",
        }
    }
}

/// Переписка в списке чатов: id комнаты и собеседник (не вы).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DmRef {
    pub id: Uuid,
    pub peer: UserRef,
}

/// Настройки чата пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSettings {
    pub dm_policy: DmPolicy,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateChatSettings {
    /// `all`, `following` или `none`; не передан — не меняется.
    pub dm_policy: Option<DmPolicy>,
}

/// Пара в каноническом порядке: на двоих — одна строка `chat_rooms`.
fn pair(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Участник ли пользователь переписки. Для всех остальных она не существует — отсюда 404,
/// а не 403: чужая переписка не должна подтверждать даже факт своего существования.
pub(crate) async fn member(state: &AppState, dm_id: Uuid, user_id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM chat_rooms
                        WHERE id = $1 AND (dm_user_a = $2 OR dm_user_b = $2))",
    )
    .bind(dm_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await?)
}

/// Переписка, видимая этому пользователю, иначе 404.
pub(crate) async fn visible(state: &AppState, dm_id: Uuid, viewer: Option<Uuid>) -> AppResult<()> {
    let Some(user_id) = viewer else {
        // Гостю личных переписок не видно вовсе.
        return Err(AppError::NotFound);
    };
    if member(state, dm_id, user_id).await? {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

/// Собеседник в переписке; не участник — 404 (чужая переписка не существует).
pub(crate) async fn peer_of(state: &AppState, dm_id: Uuid, me: Uuid) -> AppResult<Uuid> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT dm_user_a, dm_user_b FROM chat_rooms
         WHERE id = $1 AND (dm_user_a = $2 OR dm_user_b = $2)",
    )
    .bind(dm_id)
    .bind(me)
    .fetch_optional(&state.db)
    .await?;
    let (a, b) = row.ok_or(AppError::NotFound)?;
    Ok(if a == me { b } else { a })
}

/// Переписки по id: собеседник каждой — одним запросом за пользователями.
pub(crate) async fn refs_for(
    state: &AppState,
    ids: Vec<Uuid>,
    me: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, DmRef>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, dm_user_a, dm_user_b FROM chat_rooms WHERE id = ANY($1) AND dm_user_a IS NOT NULL",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    let peers: Vec<Uuid> = rows
        .iter()
        .map(|(_, a, b)| if *a == me { *b } else { *a })
        .collect();
    let users = refs::users(state, peers).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, a, b)| {
            let peer_id = if a == me { b } else { a };
            Some((
                id,
                DmRef {
                    id,
                    peer: users.get(&peer_id)?.clone(),
                },
            ))
        })
        .collect())
}

/// Кто кого заблокировал в паре: `(я его, он меня)`.
pub(crate) async fn blocks(state: &AppState, me: Uuid, peer: Uuid) -> AppResult<(bool, bool)> {
    Ok(sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM user_blocks WHERE user_id = $1 AND blocked_id = $2),
                EXISTS (SELECT 1 FROM user_blocks WHERE user_id = $2 AND blocked_id = $1)",
    )
    .bind(me)
    .bind(peer)
    .fetch_one(&state.db)
    .await?)
}

/// Кого не показывать в поиске людей: кого я заблокировал и кто заблокировал меня.
pub(crate) async fn hidden_from(state: &AppState, me: Uuid) -> AppResult<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT blocked_id FROM user_blocks WHERE user_id = $1
         UNION SELECT user_id FROM user_blocks WHERE blocked_id = $1",
    )
    .bind(me)
    .fetch_all(&state.db)
    .await?)
}

/// Можно ли писать в эту переписку: блокировка в любую сторону закрывает её обоим.
/// Зовётся и при открытии, и на каждой отправке — блокировка действует на уже открытую.
pub(crate) async fn check_not_blocked(state: &AppState, me: Uuid, peer: Uuid) -> AppResult<()> {
    let (i_blocked, blocked_me) = blocks(state, me, peer).await?;
    if i_blocked || blocked_me {
        return Err(AppError::Forbidden.with_code("blocked"));
    }
    Ok(())
}

/// Политика собеседника: пускает ли он этого отправителя первым сообщением.
async fn check_policy(state: &AppState, me: Uuid, peer: Uuid) -> AppResult<()> {
    let policy = DmPolicy::from_db(
        sqlx::query_scalar("SELECT dm_policy FROM user_chat_settings WHERE user_id = $1")
            .bind(peer)
            .fetch_optional(&state.db)
            .await?
            .flatten(),
    );
    let allowed =
        match policy {
            DmPolicy::All => true,
            DmPolicy::None => false,
            // Пишет тот, на кого собеседник подписан сам.
            DmPolicy::Following => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM follows WHERE follower_id = $1 AND followee_id = $2)",
            )
            .bind(peer)
            .bind(me)
            .fetch_one(&state.db)
            .await?,
        };
    if allowed {
        Ok(())
    } else {
        Err(AppError::Forbidden.with_code("dm_closed"))
    }
}

/// Лимит на новые переписки в сутки.
async fn check_dm_limit(state: &AppState, me: Uuid) -> AppResult<()> {
    let (started, oldest): (i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT count(*), min(created_at) FROM chat_rooms
         WHERE dm_user_a IS NOT NULL AND created_by = $1
           AND created_at > now() - interval '24 hours'",
    )
    .bind(me)
    .fetch_one(&state.db)
    .await?;
    if started < MAX_NEW_DMS_PER_DAY {
        return Ok(());
    }
    let retry_after_secs = oldest
        .map(|at| (at + chrono::Duration::hours(24) - chrono::Utc::now()).num_seconds())
        .unwrap_or(0)
        .clamp(1, 24 * 3600) as u64;
    Err(AppError::TooManyRequests { retry_after_secs })
}

/// Открыть переписку с пользователем, заведя её при первом обращении. Повторный вызов
/// возвращает ту же; скрытую у себя — возвращает в список.
#[utoipa::path(
    put, operation_id = "open_dm", path = "/users/{username}/dm", tag = "chat",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "username собеседника", example = "alice")),
    responses(
        (status = 200, description = "Переписка", body = ChatRoomInfo),
        (status = 400, description = "Переписка с самим собой (`dm_self`)", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Блокировка (`blocked`), закрытые личные (`dm_closed`) или неподтверждённый email (`email_not_verified`)", body = ErrorBody),
        (status = 404, description = "Нет такого пользователя", body = ErrorBody),
        (status = 429, description = "Слишком много новых переписок за сутки (`Retry-After`)", body = ErrorBody),
    )
)]
pub async fn open(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<Json<ChatRoomInfo>> {
    let peer = refs::user(&state, &username).await?;
    if peer.id == user.id {
        return Err(
            AppError::BadRequest("cannot open a chat with yourself".into()).with_code("dm_self"),
        );
    }
    check_not_blocked(&state, user.id, peer.id).await?;

    let (a, b) = pair(user.id, peer.id);
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM chat_rooms WHERE dm_user_a = $1 AND dm_user_b = $2")
            .bind(a)
            .bind(b)
            .fetch_optional(&state.db)
            .await?;

    let id = match existing {
        Some(id) => id,
        None => {
            // Первое обращение: проверяем право написать и лимит, потом заводим.
            if !state.users.is_verified(user.id).await? {
                return Err(AppError::Forbidden.with_code("email_not_verified"));
            }
            check_policy(&state, user.id, peer.id).await?;
            check_dm_limit(&state, user.id).await?;
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO chat_rooms (dm_user_a, dm_user_b, created_by) VALUES ($1, $2, $3)
                 ON CONFLICT (dm_user_a, dm_user_b) DO UPDATE SET dm_user_a = excluded.dm_user_a
                 RETURNING id",
            )
            .bind(a)
            .bind(b)
            .bind(user.id)
            .fetch_one(&state.db)
            .await
            .map_err(crate::validate::missing_reference)?;
            // Обе стороны узнают о переписке и подписываются на её канал без перезагрузки.
            events::publish(&state, events::dm_created([user.id, peer.id], id));
            id
        }
    };
    // Открыли — значит, она снова нужна в списке.
    sqlx::query("DELETE FROM chat_room_hidden WHERE user_id = $1 AND room_id = $2")
        .bind(user.id)
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(chat::dm_info(&state, id, &user).await?))
}

/// Панель переписки: то же, что `GET /chat/{kind}/{slug}` при `kind = dm`. Отдельный
/// обработчик нужен потому, что `DELETE /chat/dm/{id}` делает путь статическим и без него
/// GET на него отвечал бы 405 вместо панели.
#[utoipa::path(
    get, operation_id = "get_dm", path = "/chat/dm/{id}", tag = "chat",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id переписки")),
    responses(
        (status = 200, description = "Переписка", body = ChatRoomInfo),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Нет такой переписки или вы не её участник", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ChatRoomInfo>> {
    Ok(Json(chat::dm_info(&state, id, &user).await?))
}

/// Убрать переписку у себя → 204. У собеседника она остаётся, история цела, новое сообщение
/// возвращает её в список.
#[utoipa::path(
    delete, operation_id = "delete_dm", path = "/chat/dm/{id}", tag = "chat",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id переписки")),
    responses(
        (status = 204, description = "Убрана из списка"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такой переписки", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    visible(&state, id, Some(user.id)).await?;
    sqlx::query(
        "INSERT INTO chat_room_hidden (user_id, room_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Заблокировать пользователя → 204 (повторно — тоже 204). Блокировка односторонняя, его
/// не уведомляют; личные сообщения закрываются в обе стороны, общие каналы не трогаются.
#[utoipa::path(
    put, operation_id = "block_user", path = "/users/{username}/block", tag = "users",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "username", example = "alice")),
    responses(
        (status = 204, description = "Заблокирован"),
        (status = 400, description = "Нельзя заблокировать себя", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого пользователя", body = ErrorBody),
    )
)]
pub async fn block(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let peer = refs::user(&state, &username).await?;
    if peer.id == user.id {
        return Err(AppError::BadRequest("cannot block yourself".into()));
    }
    sqlx::query(
        "INSERT INTO user_blocks (user_id, blocked_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user.id)
    .bind(peer.id)
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять блокировку → 204 (даже если её не было).
#[utoipa::path(
    delete, operation_id = "unblock_user", path = "/users/{username}/block", tag = "users",
    security(("bearer" = [])),
    params(("username" = String, Path, description = "username", example = "alice")),
    responses(
        (status = 204, description = "Не заблокирован"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Нет такого пользователя", body = ErrorBody),
    )
)]
pub async fn unblock(
    State(state): State<AppState>,
    user: AuthUser,
    Path(username): Path<String>,
) -> AppResult<StatusCode> {
    let peer = refs::user(&state, &username).await?;
    sqlx::query("DELETE FROM user_blocks WHERE user_id = $1 AND blocked_id = $2")
        .bind(user.id)
        .bind(peer.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Мой список блокировок, новые сверху — для экрана настроек.
#[utoipa::path(
    get, operation_id = "list_blocked_users", path = "/users/blocked", tag = "users",
    security(("bearer" = [])),
    params(PageQuery),
    responses(
        (status = 200, description = "Страница заблокированных", body = Page<UserRef>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_blocked(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Page<UserRef>>> {
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM user_blocks WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT blocked_id FROM user_blocks WHERE user_id = $1
         ORDER BY created_at DESC, blocked_id LIMIT $2 OFFSET $3",
    )
    .bind(user.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let found = refs::users(&state, ids.clone()).await?;
    let items = ids
        .into_iter()
        .filter_map(|id| found.get(&id).cloned())
        .collect();
    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

/// Мои настройки чата.
#[utoipa::path(
    get, operation_id = "get_chat_settings", path = "/users/me/chat-settings", tag = "users",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Настройки", body = ChatSettings),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn settings(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<ChatSettings>> {
    let dm_policy = DmPolicy::from_db(
        sqlx::query_scalar("SELECT dm_policy FROM user_chat_settings WHERE user_id = $1")
            .bind(user.id)
            .fetch_optional(&state.db)
            .await?
            .flatten(),
    );
    Ok(Json(ChatSettings { dm_policy }))
}

/// Изменить настройки чата.
#[utoipa::path(
    patch, operation_id = "update_chat_settings", path = "/users/me/chat-settings", tag = "users",
    security(("bearer" = [])),
    request_body = UpdateChatSettings,
    responses(
        (status = 200, description = "Настройки", body = ChatSettings),
        (status = 400, description = "Неизвестное значение `dm_policy`", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn update_settings(
    State(state): State<AppState>,
    user: AuthUser,
    JsonBody(req): JsonBody<UpdateChatSettings>,
) -> AppResult<Json<ChatSettings>> {
    let Some(policy) = req.dm_policy else {
        return settings(State(state), user).await;
    };
    sqlx::query(
        "INSERT INTO user_chat_settings (user_id, dm_policy) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET dm_policy = excluded.dm_policy",
    )
    .bind(user.id)
    .bind(policy.as_db())
    .execute(&state.db)
    .await
    .map_err(crate::validate::missing_reference)?;
    Ok(Json(ChatSettings { dm_policy: policy }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_is_canonical() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(pair(a, b), (a, b));
        assert_eq!(pair(b, a), (a, b));
    }

    #[test]
    fn policy_round_trip() {
        for policy in [DmPolicy::All, DmPolicy::Following, DmPolicy::None] {
            assert_eq!(DmPolicy::from_db(Some(policy.as_db().into())), policy);
        }
        // Нет строки настроек — значение по умолчанию.
        assert_eq!(DmPolicy::from_db(None), DmPolicy::All);
    }
}
