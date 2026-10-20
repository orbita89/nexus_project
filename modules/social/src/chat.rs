//! Чат по интересам: комната на каждую сущность (`entity:<slug>`), человека (`person:<slug>`) и
//! тег (`tag:<slug>`) каталога.
//!
//! Отправка и «прочитано» — по WebSocket: `realtime` принимает `chat.send` / `chat.read` и
//! вызывает [`PgChat`] через `shared::directory::Chat` (`state.chat`). Здесь же REST: мои чаты
//! (по интересам всех трёх видов), история, правка и удаление своих сообщений, заглушение комнаты.
//!
//! Комната создаётся при первом сообщении. Сообщения хранятся навсегда; удаление мягкое — в
//! истории остаётся заглушка без текста и автора. Событие `chat.message.created` несёт само
//! сообщение (исключение из правила «в событиях только id»: иначе каждое сообщение — запрос
//! каждого клиента); правка и удаление — только id, клиент перечитывает `GET /chat/messages/{id}`.

use crate::events::{self, room_parts};
use crate::refs;
use crate::validate::{self, MAX_CHAT_BODY};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::directory::{async_trait, Chat, ChatRoom, EntityRef, PersonRef, TagRef, UserRef};
use shared::error::ErrorBody;
use shared::extract::{JsonBody, Path, Query};
use shared::{AppError, AppResult, AppState, AuthUser, Role};
use sqlx::PgConnection;
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Лимит частоты: не больше `RATE_MESSAGES` сообщений за `RATE_WINDOW_SECS` секунд.
pub const RATE_MESSAGES: i64 = 5;
pub const RATE_WINDOW_SECS: u64 = 10;
/// Непрочитанные считаются до этого числа (фронтенд показывает «99+»).
pub const MAX_UNREAD: i64 = 100;
const HISTORY_LIMIT: i64 = 50;
const MAX_HISTORY_LIMIT: i64 = 100;

/// Вид комнаты (`entity`, `person`, `tag`) и id того, чья она, — из строки `chat_rooms` `r`.
const ROOM_COLUMNS: &str = "CASE WHEN r.entity_id IS NOT NULL THEN 'entity'
          WHEN r.person_id IS NOT NULL THEN 'person' ELSE 'tag' END AS kind,
     coalesce(r.entity_id, r.person_id, r.tag_id) AS target_id";

const MESSAGE_COLUMNS: &str = "m.id, m.author_id, m.reply_to, m.body, m.created_at,
     m.edited_at, m.deleted_at";

/// Колонка `chat_rooms` комнаты.
fn column(room: ChatRoom) -> &'static str {
    match room {
        ChatRoom::Entity(_) => "entity_id",
        ChatRoom::Person(_) => "person_id",
        ChatRoom::Tag(_) => "tag_id",
    }
}

fn room_from(kind: &str, id: Uuid) -> ChatRoom {
    match kind {
        "entity" => ChatRoom::Entity(id),
        "person" => ChatRoom::Person(id),
        _ => ChatRoom::Tag(id),
    }
}

#[derive(Debug, sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    kind: String,
    target_id: Uuid,
    author_id: Uuid,
    reply_to: Option<Uuid>,
    body: String,
    created_at: DateTime<Utc>,
    edited_at: Option<DateTime<Utc>>,
    deleted_at: Option<DateTime<Utc>>,
}

impl MessageRow {
    fn room(&self) -> ChatRoom {
        room_from(&self.kind, self.target_id)
    }
}

/// Сообщение чата. Удалённое — заглушка: `deleted: true`, без `author` и `body`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatMessage {
    pub id: Uuid,
    /// Вид комнаты: `entity`, `person`, `tag`.
    #[schema(example = "entity")]
    pub kind: String,
    /// id сущности, человека или тега комнаты.
    pub target_id: Uuid,
    pub author: Option<UserRef>,
    pub body: Option<String>,
    /// Ответ на сообщение той же комнаты (`null` — нет или оно удалено насовсем).
    pub reply_to: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted: bool,
}

/// Комната в списке «мои чаты». Заполнено ровно одно из `entity`, `person`, `tag` — по `kind`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSummary {
    /// Имя канала и комнаты для WebSocket: `entity:<slug>`, `person:<slug>`, `tag:<slug>`.
    #[schema(example = "entity:dune-2021")]
    pub room: String,
    #[schema(example = "entity")]
    pub kind: String,
    pub entity: Option<EntityRef>,
    pub person: Option<PersonRef>,
    pub tag: Option<TagRef>,
    /// `null` — сообщений ещё нет.
    pub last_message: Option<ChatMessage>,
    /// Непрочитанные чужие сообщения, не больше 100.
    pub unread: i64,
    pub muted: bool,
}

/// Страница истории: старые сверху, последнее — самое новое.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatHistory {
    pub items: Vec<ChatMessage>,
    /// Курсор для более старых: `?before=`. `null` — это начало истории.
    pub next_before: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryQuery {
    /// Сообщения старше этого id (курсор `next_before`). Без него — самые новые.
    pub before: Option<Uuid>,
    /// 1–100, по умолчанию 50.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateChatMessage {
    /// 1–2000 символов.
    pub body: String,
}

/// Комната из пути REST: `/chat/{kind}/{slug}/...`. Неизвестный вид или slug — 404.
async fn room_by_slug(state: &AppState, kind: &str, slug: &str) -> AppResult<ChatRoom> {
    Ok(match kind {
        "entity" => ChatRoom::Entity(refs::entity(state, slug).await?.id),
        "person" => ChatRoom::Person(refs::person(state, slug).await?.id),
        "tag" => ChatRoom::Tag(refs::tag(state, slug).await?.id),
        _ => return Err(AppError::NotFound),
    })
}

/// Строки → сообщения: авторы одним запросом.
async fn messages(state: &AppState, rows: Vec<MessageRow>) -> AppResult<Vec<ChatMessage>> {
    let ids = rows
        .iter()
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.author_id)
        .collect();
    let users = refs::users(state, ids).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let deleted = row.deleted_at.is_some();
            ChatMessage {
                id: row.id,
                kind: row.kind,
                target_id: row.target_id,
                author: (!deleted)
                    .then(|| users.get(&row.author_id).cloned())
                    .flatten(),
                body: (!deleted).then_some(row.body),
                reply_to: row.reply_to,
                created_at: row.created_at,
                edited_at: row.edited_at,
                deleted,
            }
        })
        .collect())
}

async fn message_row(db: &mut PgConnection, id: Uuid) -> AppResult<Option<MessageRow>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS}, {ROOM_COLUMNS}
         FROM chat_messages m JOIN chat_rooms r ON r.id = m.room_id
         WHERE m.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Комната: найти или создать. Строка комнаты остаётся заблокированной до конца транзакции —
/// счётчик и последнее сообщение меняются по очереди. Нет сущности, человека или тега — 404.
async fn room_id(tx: &mut PgConnection, room: ChatRoom) -> AppResult<Uuid> {
    let column = column(room);
    sqlx::query_scalar(&format!(
        "INSERT INTO chat_rooms ({column}) VALUES ($1)
         ON CONFLICT ({column}) DO UPDATE SET {column} = EXCLUDED.{column}
         RETURNING id"
    ))
    .bind(room_parts(room).1)
    .fetch_one(tx)
    .await
    .map_err(validate::missing_reference)
}

/// Реализация `shared::directory::Chat`: вызывает `realtime` на `chat.send` и `chat.read`.
pub struct PgChat;

#[async_trait]
impl Chat for PgChat {
    async fn send(
        &self,
        state: &AppState,
        user_id: Uuid,
        room: ChatRoom,
        body: &str,
        reply_to: Option<Uuid>,
    ) -> AppResult<Value> {
        let body = validate::required("body", body, MAX_CHAT_BODY)?;
        let recent: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM chat_messages
             WHERE author_id = $1 AND created_at > now() - make_interval(secs => $2)",
        )
        .bind(user_id)
        .bind(RATE_WINDOW_SECS as f64)
        .fetch_one(&state.db)
        .await?;
        if recent >= RATE_MESSAGES {
            return Err(AppError::TooManyRequests {
                retry_after_secs: RATE_WINDOW_SECS,
            });
        }

        let mut tx = state.db.begin().await?;
        let room_id = room_id(&mut tx, room).await?;
        if let Some(parent) = reply_to {
            let deleted: Option<bool> = sqlx::query_scalar(
                "SELECT deleted_at IS NOT NULL FROM chat_messages WHERE id = $1 AND room_id = $2",
            )
            .bind(parent)
            .bind(room_id)
            .fetch_optional(&mut *tx)
            .await?;
            match deleted {
                None => {
                    return Err(AppError::BadRequest(
                        "reply_to: no such message in this room".into(),
                    ))
                }
                Some(true) => {
                    return Err(AppError::BadRequest(
                        "reply_to: cannot reply to a deleted message".into(),
                    ))
                }
                Some(false) => {}
            }
        }
        let (id, created_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
            "INSERT INTO chat_messages (room_id, author_id, reply_to, body)
             VALUES ($1, $2, $3, $4) RETURNING id, created_at",
        )
        .bind(room_id)
        .bind(user_id)
        .bind(reply_to)
        .bind(&body)
        .fetch_one(&mut *tx)
        .await
        .map_err(validate::missing_reference)?;
        sqlx::query(
            "UPDATE chat_rooms SET messages_count = messages_count + 1,
                 last_message_id = $2, last_message_at = $3
             WHERE id = $1",
        )
        .bind(room_id)
        .bind(id)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        // Своё сообщение прочитано.
        mark_read(&mut tx, user_id, room_id, id).await?;
        tx.commit().await?;

        let (kind, target_id) = room_parts(room);
        let row = MessageRow {
            id,
            kind: kind.to_string(),
            target_id,
            author_id: user_id,
            reply_to,
            body,
            created_at,
            edited_at: None,
            deleted_at: None,
        };
        let message = serde_json::json!(messages(state, vec![row]).await?.remove(0));
        events::publish(state, events::chat_message_created(room, &message));
        Ok(message)
    }

    async fn read(
        &self,
        state: &AppState,
        user_id: Uuid,
        room: ChatRoom,
        message_id: Uuid,
    ) -> AppResult<()> {
        let mut tx = state.db.begin().await?;
        let room_id: Option<Uuid> = sqlx::query_scalar(&format!(
            "SELECT m.room_id FROM chat_messages m JOIN chat_rooms r ON r.id = m.room_id
             WHERE m.id = $1 AND r.{} = $2",
            column(room)
        ))
        .bind(message_id)
        .bind(room_parts(room).1)
        .fetch_optional(&mut *tx)
        .await?;
        let room_id = room_id.ok_or(AppError::NotFound)?;
        mark_read(&mut tx, user_id, room_id, message_id).await?;
        tx.commit().await?;
        events::publish(state, events::chat_read(user_id, room, message_id));
        Ok(())
    }
}

/// Прочитано до `message_id`; назад не двигается (uuidv7 растёт со временем).
async fn mark_read(
    tx: &mut PgConnection,
    user_id: Uuid,
    room_id: Uuid,
    message_id: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO chat_reads (user_id, room_id, last_read_id) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, room_id) DO UPDATE
         SET last_read_id = GREATEST(chat_reads.last_read_id, EXCLUDED.last_read_id),
             updated_at = now()",
    )
    .bind(user_id)
    .bind(room_id)
    .bind(message_id)
    .execute(tx)
    .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct SummaryRow {
    kind: String,
    target_id: Uuid,
    last_message_id: Option<Uuid>,
    unread: i64,
    muted: bool,
}

/// Мои чаты: комнаты всех сущностей, людей и тегов из интересов (сообщений может ещё не быть).
/// Сверху — с самыми свежими сообщениями, затем без сообщений (новые интересы сверху).
#[utoipa::path(
    get, operation_id = "list_own_chats", path = "/chats", tag = "chat",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Комнаты по интересам", body = Vec<ChatSummary>),
        (status = 401, description = "Нет токена", body = ErrorBody),
    )
)]
pub async fn list_own(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Vec<ChatSummary>>> {
    // Каждая ветка находит комнату по своей колонке (UNIQUE-индекс), без OR в JOIN.
    let rows: Vec<SummaryRow> = sqlx::query_as(
        "WITH mine AS (
             SELECT 'entity' AS kind, i.entity_id AS target_id, i.created_at, c.id AS room_id
             FROM user_interests i LEFT JOIN chat_rooms c ON c.entity_id = i.entity_id
             WHERE i.user_id = $1
             UNION ALL
             SELECT 'person', i.person_id, i.created_at, c.id
             FROM user_person_interests i LEFT JOIN chat_rooms c ON c.person_id = i.person_id
             WHERE i.user_id = $1
             UNION ALL
             SELECT 'tag', i.tag_id, i.created_at, c.id
             FROM user_tag_interests i LEFT JOIN chat_rooms c ON c.tag_id = i.tag_id
             WHERE i.user_id = $1)
         SELECT mine.kind, mine.target_id, r.last_message_id,
                (SELECT count(*) FROM (
                     SELECT 1 FROM chat_messages m
                     WHERE m.room_id = r.id AND m.deleted_at IS NULL AND m.author_id <> $1
                       AND (rd.last_read_id IS NULL OR m.id > rd.last_read_id)
                     LIMIT $2) unread) AS unread,
                EXISTS (SELECT 1 FROM chat_mutes mu
                        WHERE mu.user_id = $1 AND mu.room_id = r.id) AS muted
         FROM mine
         LEFT JOIN chat_rooms r ON r.id = mine.room_id
         LEFT JOIN chat_reads rd ON rd.user_id = $1 AND rd.room_id = r.id
         ORDER BY r.last_message_at DESC NULLS LAST, mine.created_at DESC, mine.target_id",
    )
    .bind(user.id)
    .bind(MAX_UNREAD)
    .fetch_all(&state.db)
    .await?;

    let last_ids: Vec<Uuid> = rows.iter().filter_map(|row| row.last_message_id).collect();
    let last_rows: Vec<MessageRow> = sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS}, {ROOM_COLUMNS}
         FROM chat_messages m JOIN chat_rooms r ON r.id = m.room_id
         WHERE m.id = ANY($1)"
    ))
    .bind(&last_ids)
    .fetch_all(&state.db)
    .await?;
    let mut last: HashMap<Uuid, ChatMessage> = messages(&state, last_rows)
        .await?
        .into_iter()
        .map(|message| (message.id, message))
        .collect();
    let ids_of = |kind: &str| -> Vec<Uuid> {
        rows.iter()
            .filter(|row| row.kind == kind)
            .map(|row| row.target_id)
            .collect()
    };
    let entities = refs::entities(&state, ids_of("entity")).await?;
    let people = refs::people(&state, ids_of("person")).await?;
    let tags = refs::tags(&state, ids_of("tag")).await?;

    Ok(Json(
        rows.into_iter()
            .filter_map(|row| {
                let (mut entity, mut person, mut tag) = (None, None, None);
                let slug = match row.kind.as_str() {
                    "entity" => {
                        let found = entities.get(&row.target_id)?.clone();
                        let slug = found.slug.clone();
                        entity = Some(found);
                        slug
                    }
                    "person" => {
                        let found = people.get(&row.target_id)?.clone();
                        let slug = found.slug.clone();
                        person = Some(found);
                        slug
                    }
                    _ => {
                        let found = tags.get(&row.target_id)?.clone();
                        let slug = found.slug.clone();
                        tag = Some(found);
                        slug
                    }
                };
                Some(ChatSummary {
                    room: format!("{}:{slug}", row.kind),
                    kind: row.kind,
                    entity,
                    person,
                    tag,
                    last_message: row.last_message_id.and_then(|id| last.remove(&id)),
                    unread: row.unread,
                    muted: row.muted,
                })
            })
            .collect(),
    ))
}

/// История комнаты: страница по 50 (до 100), старые сверху; дальше в прошлое —
/// `?before=<next_before>`. Публичная, как форум.
#[utoipa::path(
    get, operation_id = "chat_history", path = "/chat/{kind}/{slug}/messages", tag = "chat",
    params(
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека или тега", example = "dune-2021"),
        HistoryQuery,
    ),
    responses(
        (status = 200, description = "Страница истории", body = ChatHistory),
        (status = 400, description = "`limit` вне 1–100", body = ErrorBody),
        (status = 404, description = "Неизвестный вид комнаты или slug", body = ErrorBody),
    )
)]
pub async fn history(
    State(state): State<AppState>,
    Path((kind, slug)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> AppResult<Json<ChatHistory>> {
    let limit = query.limit.unwrap_or(HISTORY_LIMIT);
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(AppError::BadRequest(format!(
            "limit must be 1-{MAX_HISTORY_LIMIT}"
        )));
    }
    let room = room_by_slug(&state, &kind, &slug).await?;
    let mut rows: Vec<MessageRow> = sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS}, {ROOM_COLUMNS}
         FROM chat_messages m JOIN chat_rooms r ON r.id = m.room_id
         WHERE r.{} = $1 AND ($2::uuid IS NULL OR m.id < $2)
         ORDER BY m.id DESC LIMIT $3",
        column(room)
    ))
    .bind(room_parts(room).1)
    .bind(query.before)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    rows.reverse();
    let next_before = if more {
        rows.first().map(|row| row.id)
    } else {
        None
    };
    Ok(Json(ChatHistory {
        items: messages(&state, rows).await?,
        next_before,
    }))
}

/// Одно сообщение: клиент перечитывает его по событию `chat.message.updated` / `deleted`.
#[utoipa::path(
    get, operation_id = "get_chat_message", path = "/chat/messages/{id}", tag = "chat",
    params(("id" = Uuid, Path, description = "id сообщения")),
    responses(
        (status = 200, description = "Сообщение", body = ChatMessage),
        (status = 404, description = "Не найдено", body = ErrorBody),
    )
)]
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ChatMessage>> {
    let mut db = state.db.acquire().await?;
    let row = message_row(&mut db, id).await?.ok_or(AppError::NotFound)?;
    drop(db);
    Ok(Json(messages(&state, vec![row]).await?.remove(0)))
}

/// Изменить своё сообщение.
#[utoipa::path(
    patch, operation_id = "update_chat_message", path = "/chat/messages/{id}", tag = "chat",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сообщения")),
    request_body = UpdateChatMessage,
    responses(
        (status = 200, description = "Изменено", body = ChatMessage),
        (status = 400, description = "Пустой или длинный текст, сообщение удалено", body = ErrorBody),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужое сообщение", body = ErrorBody),
        (status = 404, description = "Не найдено", body = ErrorBody),
    )
)]
pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    JsonBody(req): JsonBody<UpdateChatMessage>,
) -> AppResult<Json<ChatMessage>> {
    let body = validate::required("body", &req.body, MAX_CHAT_BODY)?;
    let mut tx = state.db.begin().await?;
    let found: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT author_id, deleted_at IS NOT NULL FROM chat_messages WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    match found {
        None => return Err(AppError::NotFound),
        Some((author, _)) if author != user.id => return Err(AppError::Forbidden),
        Some((_, true)) => {
            return Err(AppError::BadRequest("cannot edit a deleted message".into()))
        }
        Some(_) => {}
    }
    sqlx::query("UPDATE chat_messages SET body = $2, edited_at = now() WHERE id = $1")
        .bind(id)
        .bind(&body)
        .execute(&mut *tx)
        .await?;
    let row = message_row(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    tx.commit().await?;
    let room = row.room();
    let message = messages(&state, vec![row]).await?.remove(0);
    events::publish(
        &state,
        events::chat_message_changed("chat.message.updated", room, id),
    );
    Ok(Json(message))
}

/// Удалить сообщение: автор или admin. В истории остаётся заглушка → 204.
#[utoipa::path(
    delete, operation_id = "delete_chat_message", path = "/chat/messages/{id}", tag = "chat",
    security(("bearer" = [])),
    params(("id" = Uuid, Path, description = "id сообщения")),
    responses(
        (status = 204, description = "Удалено (повторно — тоже 204)"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 403, description = "Чужое сообщение", body = ErrorBody),
        (status = 404, description = "Не найдено", body = ErrorBody),
    )
)]
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let row = message_row(&mut tx, id).await?.ok_or(AppError::NotFound)?;
    if row.author_id != user.id && user.role < Role::Admin {
        return Err(AppError::Forbidden);
    }
    if row.deleted_at.is_some() {
        return Ok(StatusCode::NO_CONTENT);
    }
    sqlx::query("UPDATE chat_messages SET body = '', deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    events::publish(
        &state,
        events::chat_message_changed("chat.message.deleted", row.room(), id),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Заглушить комнату: события приходят, но фронтенд не показывает уведомления → 204.
#[utoipa::path(
    put, operation_id = "mute_chat", path = "/chat/{kind}/{slug}/mute", tag = "chat",
    security(("bearer" = [])),
    params(
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека или тега", example = "dune-2021"),
    ),
    responses(
        (status = 204, description = "Заглушена (повторно — тоже 204)"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Неизвестный вид комнаты или slug", body = ErrorBody),
    )
)]
pub async fn mute(
    State(state): State<AppState>,
    user: AuthUser,
    Path((kind, slug)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let room = room_by_slug(&state, &kind, &slug).await?;
    let mut tx = state.db.begin().await?;
    let room_id = room_id(&mut tx, room).await?;
    sqlx::query("INSERT INTO chat_mutes (user_id, room_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(user.id)
        .bind(room_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снять заглушение → 204 (даже если не было).
#[utoipa::path(
    delete, operation_id = "unmute_chat", path = "/chat/{kind}/{slug}/mute", tag = "chat",
    security(("bearer" = [])),
    params(
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека или тега", example = "dune-2021"),
    ),
    responses(
        (status = 204, description = "Не заглушена"),
        (status = 401, description = "Нет токена", body = ErrorBody),
        (status = 404, description = "Неизвестный вид комнаты или slug", body = ErrorBody),
    )
)]
pub async fn unmute(
    State(state): State<AppState>,
    user: AuthUser,
    Path((kind, slug)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let room = room_by_slug(&state, &kind, &slug).await?;
    sqlx::query(&format!(
        "DELETE FROM chat_mutes mu USING chat_rooms r
         WHERE mu.room_id = r.id AND mu.user_id = $1 AND r.{} = $2",
        column(room)
    ))
    .bind(user.id)
    .bind(room_parts(room).1)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
