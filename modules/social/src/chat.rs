//! Чат по интересам: комната на каждую сущность (`entity:<slug>`), человека (`person:<slug>`) и
//! тег (`tag:<slug>`) каталога, системные комнаты и свои комнаты (`room:<slug>`, см. `rooms`).
//!
//! Отправка и «прочитано» — по WebSocket: `realtime` принимает `chat.send` / `chat.read` и
//! вызывает [`PgChat`] через `shared::directory::Chat` (`state.chat`). Здесь же REST: мои чаты
//! (по интересам всех трёх видов), история, правка и удаление своих сообщений, заглушение комнаты.
//!
//! Комната создаётся при первом сообщении. Сообщения хранятся навсегда; удаление мягкое — в
//! истории остаётся заглушка без текста и автора. Событие `chat.message.created` несёт само
//! сообщение (исключение из правила «в событиях только id»: иначе каждое сообщение — запрос
//! каждого клиента); правка и удаление — только id, клиент перечитывает `GET /chat/messages/{id}`.

use crate::attachments::{self, Attachment};
use crate::dm::{self, DmRef};
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
/// У своей комнаты (`slug`) target — id самой комнаты.
const ROOM_COLUMNS: &str = "CASE WHEN r.entity_id IS NOT NULL THEN 'entity'
          WHEN r.person_id IS NOT NULL THEN 'person'
          WHEN r.tag_id IS NOT NULL THEN 'tag'
          WHEN r.dm_user_a IS NOT NULL THEN 'dm' ELSE 'room' END AS kind,
     coalesce(r.entity_id, r.person_id, r.tag_id, r.id) AS target_id";

const MESSAGE_COLUMNS: &str = "m.id, m.author_id, m.reply_to, m.body, m.created_at,
     m.edited_at, m.deleted_at";

/// Колонка `chat_rooms` комнаты.
fn column(room: ChatRoom) -> &'static str {
    match room {
        ChatRoom::Entity(_) => "entity_id",
        ChatRoom::Person(_) => "person_id",
        ChatRoom::Tag(_) => "tag_id",
        ChatRoom::Room(_) | ChatRoom::Dm(_) => "id",
    }
}

fn room_from(kind: &str, id: Uuid) -> ChatRoom {
    match kind {
        "entity" => ChatRoom::Entity(id),
        "person" => ChatRoom::Person(id),
        "tag" => ChatRoom::Tag(id),
        "dm" => ChatRoom::Dm(id),
        _ => ChatRoom::Room(id),
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
    /// Вид комнаты: `entity`, `person`, `tag`, `room`.
    #[schema(example = "entity")]
    pub kind: String,
    /// id сущности, человека или тега комнаты; у своей комнаты — id комнаты.
    pub target_id: Uuid,
    pub author: Option<UserRef>,
    pub body: Option<String>,
    /// Ответ на сообщение той же комнаты (`null` — нет или оно удалено насовсем).
    pub reply_to: Option<Uuid>,
    /// Картинка, приложенная к сообщению; `null` — нет или сообщение удалено.
    pub attachment: Option<Attachment>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted: bool,
}

/// Комната со slug: системная (вкладка панели), заведённая админом или своя комната
/// пользователя — название, описание, счётчики.
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct CustomRoom {
    pub id: Uuid,
    #[schema(example = "cyberpunk-fans")]
    pub slug: String,
    #[schema(example = "Киберпанк")]
    pub title: String,
    pub description: Option<String>,
    /// Участников. У системной комнаты — все пользователи сайта.
    pub members_count: i64,
    pub messages_count: i32,
    /// Сообщений за последние сутки — «насколько комната жива сейчас».
    pub messages_24h: i64,
    /// Когда написали в последний раз; `null` — сообщений не было.
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Системная комната: в ней состоят все, её нельзя покинуть, изменить и удалить.
    pub system: bool,
    /// Владелец, который может править и удалять комнату; `null` — системная или админская.
    #[sqlx(skip)]
    pub owner: Option<UserRef>,
    /// Вступил ли текущий пользователь; `null` — гость. В системной — всегда `true`.
    #[sqlx(skip)]
    pub joined: Option<bool>,
    /// Убрана ли комната из «Моих чатов» текущим пользователем; `null` — гость. Скрытая не
    /// приходит в `GET /chats`, но остаётся рабочей: история, отправка и канал не меняются.
    #[sqlx(skip)]
    pub hidden: Option<bool>,
    #[serde(skip)]
    pub owner_id: Option<Uuid>,
}

/// Комната по id, иначе 404. Владелец и участники системной — [`fill_rooms`].
pub(crate) async fn custom_room(state: &AppState, id: Uuid) -> AppResult<CustomRoom> {
    let mut rooms = [sqlx::query_as(&format!(
        "SELECT {CUSTOM_COLUMNS} FROM chat_rooms r WHERE r.id = $1 AND r.slug IS NOT NULL"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?];
    fill_rooms(state, &mut rooms).await?;
    let [room] = rooms;
    Ok(room)
}

/// Владельцы одним запросом и `members_count` системных комнат (участник — каждый
/// пользователь, строк в `chat_room_members` для них нет).
pub(crate) async fn fill_rooms(state: &AppState, rooms: &mut [CustomRoom]) -> AppResult<()> {
    let owner_ids: Vec<Uuid> = rooms.iter().filter_map(|room| room.owner_id).collect();
    let owners = if owner_ids.is_empty() {
        HashMap::new()
    } else {
        state.users.by_ids(&owner_ids).await?
    };
    let members = if rooms.iter().any(|room| room.system) {
        state.users.count().await?
    } else {
        0
    };
    for room in rooms {
        room.owner = room.owner_id.and_then(|id| owners.get(&id).cloned());
        if room.system {
            room.members_count = members;
        }
    }
    Ok(())
}

/// Комната для открытой панели чата: что это, последнее сообщение и состояние текущего
/// пользователя. Заполнено одно из `entity`, `person`, `tag`, `custom` — по `kind`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatRoomInfo {
    /// Имя канала и комнаты для WebSocket.
    #[schema(example = "entity:dune-2021")]
    pub room: String,
    #[schema(example = "entity")]
    pub kind: String,
    pub entity: Option<EntityRef>,
    pub person: Option<PersonRef>,
    pub tag: Option<TagRef>,
    pub custom: Option<CustomRoom>,
    /// Личная переписка (`kind = dm`): id и собеседник.
    pub dm: Option<DmRef>,
    /// `null` — сообщений ещё нет.
    pub last_message: Option<ChatMessage>,
    /// Состояние текущего пользователя; `null` — гость.
    pub me: Option<ChatRoomMe>,
}

/// Комната глазами текущего пользователя.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatRoomMe {
    /// Комната в «моих чатах»: сущность, человек или тег в интересах, в свою комнату — вступил.
    pub member: bool,
    pub muted: bool,
    /// Непрочитанные чужие сообщения, не больше 100.
    pub unread: i64,
    /// Последнее прочитанное сообщение (`null` — ничего): разделитель «новые сообщения».
    pub last_read_id: Option<Uuid>,
}

/// Колонки [`CustomRoom`] из `chat_rooms r`. У системной комнаты `members_count` здесь 0 —
/// его заполняет [`fill_rooms`].
pub(crate) const CUSTOM_COLUMNS: &str = "r.id, r.slug, r.title, r.description,
     (SELECT count(*) FROM chat_room_members cm WHERE cm.room_id = r.id) AS members_count,
     r.messages_count,
     -- Сообщений за сутки: диапазон по chat_messages_room_recent_idx. Сортировка
     -- `?sort=active` ссылается на этот алиас.
     (SELECT count(*) FROM chat_messages m24
      WHERE m24.room_id = r.id AND m24.deleted_at IS NULL
        AND m24.created_at > now() - interval '24 hours') AS messages_24h,
     r.last_message_at, r.created_at, r.system, r.owner_id";

/// Комната в списке «мои чаты». Заполнено ровно одно из `entity`, `person`, `tag`, `custom` —
/// по `kind`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSummary {
    /// Имя канала и комнаты для WebSocket: `entity:<slug>`, `person:<slug>`, `tag:<slug>`,
    /// `room:<slug>`.
    #[schema(example = "entity:dune-2021")]
    pub room: String,
    #[schema(example = "entity")]
    pub kind: String,
    pub entity: Option<EntityRef>,
    pub person: Option<PersonRef>,
    pub tag: Option<TagRef>,
    /// Комната со slug (`kind = room`).
    pub custom: Option<CustomRoom>,
    /// Личная переписка (`kind = dm`): id и собеседник.
    pub dm: Option<DmRef>,
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
/// Комната из пути REST с проверкой доступа: личная переписка видна только её двоим,
/// для всех остальных её нет (404, а не 403 — чужая переписка не подтверждает своё
/// существование). `viewer` — кто смотрит; гость переписок не видит.
async fn room_by_slug(
    state: &AppState,
    kind: &str,
    slug: &str,
    viewer: Option<Uuid>,
) -> AppResult<ChatRoom> {
    Ok(match kind {
        "entity" => ChatRoom::Entity(refs::entity(state, slug).await?.id),
        "person" => ChatRoom::Person(refs::person(state, slug).await?.id),
        "tag" => ChatRoom::Tag(refs::tag(state, slug).await?.id),
        "room" => ChatRoom::Room(custom_room_id(&state.db, slug).await?),
        "dm" => {
            let id: Uuid = slug.parse().map_err(|_| AppError::NotFound)?;
            dm::visible(state, id, viewer).await?;
            ChatRoom::Dm(id)
        }
        _ => return Err(AppError::NotFound),
    })
}

/// id своей комнаты по slug, иначе 404.
pub(crate) async fn custom_room_id(db: &sqlx::PgPool, slug: &str) -> AppResult<Uuid> {
    sqlx::query_scalar("SELECT id FROM chat_rooms WHERE slug = $1")
        .bind(slug)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Виден ли зрителю такой-то месседж: решает его комната. Публичные комнаты открыты всем,
/// личную переписку видят только её двое. Нет сообщения — 404.
pub(crate) async fn visible_message(
    state: &AppState,
    message_id: Uuid,
    viewer: Option<Uuid>,
) -> AppResult<()> {
    let row: Option<(String, Uuid)> = sqlx::query_as(&format!(
        "SELECT {ROOM_COLUMNS} FROM chat_messages m JOIN chat_rooms r ON r.id = m.room_id
         WHERE m.id = $1"
    ))
    .bind(message_id)
    .fetch_optional(&state.db)
    .await?;
    let (kind, target_id) = row.ok_or(AppError::NotFound)?;
    if kind == "dm" {
        dm::visible(state, target_id, viewer).await?;
    }
    Ok(())
}

/// Строки → сообщения: авторы одним запросом.
async fn messages(state: &AppState, rows: Vec<MessageRow>) -> AppResult<Vec<ChatMessage>> {
    let ids = rows
        .iter()
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.author_id)
        .collect();
    let users = refs::users(state, ids).await?;
    // Вложения живых сообщений — одним запросом на страницу истории, не по запросу на строку.
    let live: Vec<Uuid> = rows
        .iter()
        .filter(|row| row.deleted_at.is_none())
        .map(|row| row.id)
        .collect();
    let mut files = attachments::for_messages(state, &live).await?;
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
                attachment: (!deleted).then(|| files.remove(&row.id)).flatten(),
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

/// Комната: найти или создать (своя — только найти). Строка комнаты остаётся заблокированной до
/// конца транзакции — счётчик и последнее сообщение меняются по очереди. Нет сущности, человека,
/// тега или своей комнаты — 404.
async fn room_id(tx: &mut PgConnection, room: ChatRoom) -> AppResult<Uuid> {
    // Комнату со slug и личную переписку заводят отдельные эндпоинты: здесь их только находят.
    // Создавать их этим INSERT'ом нельзя — у них нет колонки-цели, по которой он работает.
    if let ChatRoom::Room(id) | ChatRoom::Dm(id) = room {
        return sqlx::query_scalar("SELECT id FROM chat_rooms WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(tx)
            .await?
            .ok_or(AppError::NotFound);
    }
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

/// Длина username — как в `auth` (`[a-zA-Z0-9_]`, до 32 символов).
const MAX_USERNAME: usize = 32;

/// `@username` из текста сообщения, без дублей и в нижнем регистре. `@` считается началом
/// упоминания, только если перед ним не буква и не цифра: `mail@example.com` — не упоминание.
fn mentions(body: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    for (at, _) in body.match_indices('@') {
        let before_is_word = at
            .checked_sub(1)
            .and_then(|i| bytes.get(i))
            .is_some_and(|b| b.is_ascii_alphanumeric());
        if before_is_word {
            continue;
        }
        let name: String = body[at + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(MAX_USERNAME)
            .collect();
        if name.is_empty() {
            continue;
        }
        let name = name.to_lowercase();
        if !found.contains(&name) {
            found.push(name);
        }
    }
    found
}

/// Упоминания в только что отправленном сообщении: каждому названному — `chat.mention` в его
/// личный канал. Себя пропускаем, несуществующих — тоже (одним запросом в `UserDirectory`).
async fn notify_mentions(
    state: &AppState,
    author_id: Uuid,
    room: ChatRoom,
    body: &str,
    message: &Value,
) -> AppResult<()> {
    let names = mentions(body);
    if names.is_empty() {
        return Ok(());
    }
    for user in state.users.by_usernames(&names).await?.into_values() {
        if user.id != author_id {
            events::publish(state, events::chat_mention(user.id, room, message));
        }
    }
    Ok(())
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
        attachment_id: Option<Uuid>,
    ) -> AppResult<Value> {
        // С вложением текст необязателен: одна картинка — законное сообщение.
        let body = match attachment_id {
            Some(_) => validate::optional("body", Some(body.to_string()), MAX_CHAT_BODY)?
                .unwrap_or_default(),
            None => validate::required("body", body, MAX_CHAT_BODY)?,
        };
        // В личную переписку пишут только её двое и только если никто никого не заблокировал.
        let dm_peer = match room {
            ChatRoom::Dm(id) => Some(dm::peer_of(state, id, user_id).await?),
            _ => None,
        };
        if let Some(peer) = dm_peer {
            dm::check_not_blocked(state, user_id, peer).await?;
        }
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
        // Вложение привязываем здесь же: не своё или уже использованное откатит всю отправку.
        if let Some(attachment_id) = attachment_id {
            attachments::attach(&mut tx, attachment_id, user_id, id).await?;
        }
        // Своё сообщение прочитано.
        mark_read(&mut tx, user_id, room_id, id).await?;
        tx.commit().await?;

        let (kind, target_id) = room_parts(room);
        let message_body = body.clone();
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
        // Новое сообщение возвращает переписку в список собеседнику, если он её убирал.
        if let Some(peer) = dm_peer {
            sqlx::query("DELETE FROM chat_room_hidden WHERE user_id = $1 AND room_id = $2")
                .bind(peer)
                .bind(room_id)
                .execute(&state.db)
                .await?;
        }
        notify_mentions(state, user_id, room, &message_body, &message).await?;
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

    async fn room_by_slug(
        &self,
        state: &AppState,
        slug: &str,
    ) -> AppResult<Option<(Uuid, String)>> {
        Ok(
            sqlx::query_as("SELECT id, slug FROM chat_rooms WHERE slug = $1")
                .bind(slug)
                .fetch_optional(&state.db)
                .await?,
        )
    }

    async fn dm_room_ids(&self, state: &AppState, user_id: Uuid) -> AppResult<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT id FROM chat_rooms WHERE dm_user_a = $1 OR dm_user_b = $1")
                .bind(user_id)
                .fetch_all(&state.db)
                .await?,
        )
    }

    async fn dm_member(&self, state: &AppState, dm_id: Uuid, user_id: Uuid) -> AppResult<bool> {
        dm::member(state, dm_id, user_id).await
    }

    async fn room_slugs(&self, state: &AppState, ids: &[Uuid]) -> AppResult<HashMap<Uuid, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, slug FROM chat_rooms WHERE id = ANY($1) AND slug IS NOT NULL",
        )
        .bind(ids)
        .fetch_all(&state.db)
        .await?;
        Ok(rows.into_iter().collect())
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

/// Мои чаты: комнаты всех сущностей, людей и тегов из интересов и свои комнаты, в которые
/// вступил (сообщений может ещё не быть).
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
             WHERE i.user_id = $1
             UNION ALL
             SELECT 'room', cm.room_id, cm.created_at, cm.room_id
             FROM chat_room_members cm JOIN chat_rooms cr ON cr.id = cm.room_id
             WHERE cm.user_id = $1 AND NOT cr.system
             UNION ALL
             -- В системных комнатах состоят все: своих строк в chat_room_members у них нет.
             SELECT 'room', sr.id, sr.created_at, sr.id FROM chat_rooms sr
             WHERE sr.system AND NOT EXISTS (SELECT 1 FROM chat_room_hidden h
                                             WHERE h.user_id = $1 AND h.room_id = sr.id)
             UNION ALL
             -- Личные переписки: участников двое, своих строк участия у них нет.
             SELECT 'dm', d.id, d.created_at, d.id FROM chat_rooms d
             WHERE (d.dm_user_a = $1 OR d.dm_user_b = $1)
               AND NOT EXISTS (SELECT 1 FROM chat_room_hidden h
                               WHERE h.user_id = $1 AND h.room_id = d.id))
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
         ORDER BY coalesce(r.system, false) DESC,
                  -- Системные — вкладки панели: порядок заведения, он же порядок вкладок.
                  CASE WHEN r.system THEN mine.created_at END,
                  r.last_message_at DESC NULLS LAST, mine.created_at DESC, mine.target_id",
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
    let dms = dm::refs_for(&state, ids_of("dm"), user.id).await?;
    let entities = refs::entities(&state, ids_of("entity")).await?;
    let people = refs::people(&state, ids_of("person")).await?;
    let tags = refs::tags(&state, ids_of("tag")).await?;
    let mut custom_rows: Vec<CustomRoom> = sqlx::query_as::<_, CustomRoom>(&format!(
        "SELECT {CUSTOM_COLUMNS} FROM chat_rooms r WHERE r.id = ANY($1)"
    ))
    .bind(ids_of("room"))
    .fetch_all(&state.db)
    .await?;
    fill_rooms(&state, &mut custom_rows).await?;
    let custom: HashMap<Uuid, CustomRoom> = custom_rows
        .into_iter()
        .map(|room| {
            (
                room.id,
                CustomRoom {
                    joined: Some(true),
                    // Скрытые в «Мои чаты» не попадают.
                    hidden: Some(false),
                    ..room
                },
            )
        })
        .collect();

    Ok(Json(
        rows.into_iter()
            .filter_map(|row| {
                let (mut entity, mut person, mut tag, mut own) = (None, None, None, None);
                let mut peer = None;
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
                    "tag" => {
                        let found = tags.get(&row.target_id)?.clone();
                        let slug = found.slug.clone();
                        tag = Some(found);
                        slug
                    }
                    "dm" => {
                        let found = dms.get(&row.target_id)?.clone();
                        let slug = found.id.to_string();
                        peer = Some(found);
                        slug
                    }
                    _ => {
                        let found = custom.get(&row.target_id)?.clone();
                        let slug = found.slug.clone();
                        own = Some(found);
                        slug
                    }
                };
                Some(ChatSummary {
                    room: format!("{}:{slug}", row.kind),
                    kind: row.kind,
                    entity,
                    person,
                    tag,
                    custom: own,
                    dm: peer,
                    last_message: row.last_message_id.and_then(|id| last.remove(&id)),
                    unread: row.unread,
                    muted: row.muted,
                })
            })
            .collect(),
    ))
}

/// Таблица и колонка «в моих чатах» для вида комнаты: интересы или участники своей комнаты.
fn membership(room: ChatRoom) -> (&'static str, &'static str) {
    match room {
        ChatRoom::Entity(_) => ("user_interests", "entity_id"),
        ChatRoom::Person(_) => ("user_person_interests", "person_id"),
        ChatRoom::Tag(_) => ("user_tag_interests", "tag_id"),
        ChatRoom::Room(_) => ("chat_room_members", "room_id"),
        // У переписки «участие» — сама пара: её проверяет dm::member.
        ChatRoom::Dm(_) => ("chat_room_members", "room_id"),
    }
}

/// Комната для открытой панели: что это, последнее сообщение и (для вошедшего) в «моих чатах»
/// ли она, заглушена ли, сколько непрочитанных. Работает и для комнаты не из «моих чатов»
/// (открытой с карточки). Публичный: у гостя `me: null`.
#[utoipa::path(
    get, operation_id = "get_chat_room_info", path = "/chat/{kind}/{slug}", tag = "chat",
    params(
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`, `room`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека, тега или своей комнаты", example = "dune-2021"),
    ),
    responses(
        (status = 200, description = "Комната", body = ChatRoomInfo),
        (status = 401, description = "Неверный токен", body = ErrorBody),
        (status = 404, description = "Неизвестный вид комнаты или slug", body = ErrorBody),
    )
)]
pub async fn info(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Path((kind, slug)): Path<(String, String)>,
) -> AppResult<Json<ChatRoomInfo>> {
    let viewer = viewer.map(|user| user.id);
    Ok(Json(room_info(&state, &kind, &slug, viewer).await?))
}

/// Панель переписки сразу после `PUT /users/{username}/dm`.
pub(crate) async fn dm_info(
    state: &AppState,
    id: Uuid,
    user: &AuthUser,
) -> AppResult<ChatRoomInfo> {
    room_info(state, "dm", &id.to_string(), Some(user.id)).await
}

/// Комната для панели: что это, последнее сообщение и состояние зрителя.
pub(crate) async fn room_info(
    state: &AppState,
    kind: &str,
    slug: &str,
    viewer: Option<Uuid>,
) -> AppResult<ChatRoomInfo> {
    let state = state.clone();
    let kind = kind.to_string();
    let slug = slug.to_string();
    let (mut entity, mut person, mut tag, mut custom, mut dm_ref) = (None, None, None, None, None);
    let (room, slug) = match kind.as_str() {
        "entity" => {
            let found = refs::entity(&state, &slug).await?;
            let room = (ChatRoom::Entity(found.id), found.slug.clone());
            entity = Some(found);
            room
        }
        "person" => {
            let found = refs::person(&state, &slug).await?;
            let room = (ChatRoom::Person(found.id), found.slug.clone());
            person = Some(found);
            room
        }
        "tag" => {
            let found = refs::tag(&state, &slug).await?;
            let room = (ChatRoom::Tag(found.id), found.slug.clone());
            tag = Some(found);
            room
        }
        "room" => {
            let found = custom_room(&state, custom_room_id(&state.db, &slug).await?).await?;
            let room = (ChatRoom::Room(found.id), found.slug.clone());
            custom = Some(found);
            room
        }
        "dm" => {
            let id: Uuid = slug.parse().map_err(|_| AppError::NotFound)?;
            dm::visible(&state, id, viewer).await?;
            let me = viewer.ok_or(AppError::NotFound)?;
            let found = dm::refs_for(&state, vec![id], me)
                .await?
                .remove(&id)
                .ok_or(AppError::NotFound)?;
            dm_ref = Some(found);
            (ChatRoom::Dm(id), id.to_string())
        }
        _ => return Err(AppError::NotFound),
    };
    let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(&format!(
        "SELECT id, last_message_id FROM chat_rooms WHERE {} = $1",
        column(room)
    ))
    .bind(room_parts(room).1)
    .fetch_optional(&state.db)
    .await?;
    let (room_id, last_id) = match row {
        Some((id, last)) => (Some(id), last),
        None => (None, None),
    };
    let last_message = match last_id {
        Some(id) => {
            let mut db = state.db.acquire().await?;
            let found = message_row(&mut db, id).await?;
            drop(db);
            match found {
                Some(row) => messages(&state, vec![row]).await?.pop(),
                None => None,
            }
        }
        None => None,
    };

    let me = match viewer {
        None => None,
        Some(viewer_id) => {
            let user = ViewerId { id: viewer_id };
            let (table, member_column) = membership(room);
            let member: bool = sqlx::query_scalar(&format!(
                "SELECT EXISTS (SELECT 1 FROM {table} WHERE user_id = $1 AND {member_column} = $2)"
            ))
            .bind(user.id)
            .bind(room_parts(room).1)
            .fetch_one(&state.db)
            .await?;
            // В системной комнате состоят все: строк в chat_room_members для неё нет.
            // В системной комнате состоят все, в переписке — оба её участника.
            let member =
                member || custom.as_ref().is_some_and(|room| room.system) || dm_ref.is_some();
            let (muted, unread, last_read_id) = match room_id {
                None => (false, 0, None),
                Some(room_id) => {
                    sqlx::query_as::<_, (bool, i64, Option<Uuid>)>(
                        "WITH rd AS (SELECT last_read_id FROM chat_reads
                                     WHERE user_id = $1 AND room_id = $2)
                         SELECT EXISTS (SELECT 1 FROM chat_mutes
                                        WHERE user_id = $1 AND room_id = $2),
                                (SELECT count(*) FROM (
                                     SELECT 1 FROM chat_messages m
                                     WHERE m.room_id = $2 AND m.deleted_at IS NULL
                                       AND m.author_id <> $1
                                       AND ((SELECT last_read_id FROM rd) IS NULL
                                            OR m.id > (SELECT last_read_id FROM rd))
                                     LIMIT $3) unread),
                                (SELECT last_read_id FROM rd)",
                    )
                    .bind(user.id)
                    .bind(room_id)
                    .bind(MAX_UNREAD)
                    .fetch_one(&state.db)
                    .await?
                }
            };
            if let Some(custom) = custom.as_mut() {
                custom.joined = Some(member);
                // Убрана ли из «Моих чатов»: панель показывает ссылку «Вернуть».
                custom.hidden = Some(
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS (SELECT 1 FROM chat_room_hidden
                                        WHERE user_id = $1 AND room_id = $2)",
                    )
                    .bind(user.id)
                    .bind(custom.id)
                    .fetch_one(&state.db)
                    .await?,
                );
            }
            Some(ChatRoomMe {
                member,
                muted,
                unread,
                last_read_id,
            })
        }
    };
    Ok(ChatRoomInfo {
        room: format!("{kind}:{slug}"),
        kind,
        entity,
        person,
        tag,
        custom,
        dm: dm_ref,
        last_message,
        me,
    })
}

/// Id зрителя в теле [`room_info`]: раньше там был `AuthUser`, поля читаются как `user.id`.
struct ViewerId {
    id: Uuid,
}

/// История комнаты: страница по 50 (до 100), старые сверху; дальше в прошлое —
/// `?before=<next_before>`. Публичная, как форум.
#[utoipa::path(
    get, operation_id = "chat_history", path = "/chat/{kind}/{slug}/messages", tag = "chat",
    params(
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`, `room`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека, тега или своей комнаты", example = "dune-2021"),
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
    viewer: Option<AuthUser>,
    Path((kind, slug)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> AppResult<Json<ChatHistory>> {
    let limit = query.limit.unwrap_or(HISTORY_LIMIT);
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(AppError::BadRequest(format!(
            "limit must be 1-{MAX_HISTORY_LIMIT}"
        )));
    }
    let room = room_by_slug(&state, &kind, &slug, viewer.map(|user| user.id)).await?;
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
    // Строку вложения убираем в той же транзакции, файл — после коммита: если удаление
    // сообщения откатится, вложение останется на месте.
    let file = attachments::detach(&mut tx, id).await?;
    sqlx::query("UPDATE chat_messages SET body = '', deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    attachments::remove_file(&state, file).await;
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
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`, `room`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека, тега или своей комнаты", example = "dune-2021"),
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
    let room = room_by_slug(&state, &kind, &slug, Some(user.id)).await?;
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
        ("kind" = String, Path, description = "Вид комнаты: `entity`, `person`, `tag`, `room`", example = "entity"),
        ("slug" = String, Path, description = "slug сущности, человека, тега или своей комнаты", example = "dune-2021"),
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
    let room = room_by_slug(&state, &kind, &slug, Some(user.id)).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_parsed_once_and_lowercased() {
        assert_eq!(mentions("привет, @Alice и @bob!"), ["alice", "bob"]);
        // Дубли схлопываются, регистр не важен.
        assert_eq!(mentions("@alice @ALICE @Alice"), ["alice"]);
        // Знаки препинания обрывают имя, дефис в username недопустим.
        assert_eq!(mentions("@bob, @ann-marie"), ["bob", "ann"]);
        assert_eq!(mentions("@user_1 пишет"), ["user_1"]);
    }

    #[test]
    fn at_inside_a_word_is_not_a_mention() {
        assert!(mentions("mail@example.com").is_empty());
        assert!(mentions("почта me@you").is_empty());
        assert_eq!(mentions("(@bob)"), ["bob"]);
    }

    #[test]
    fn empty_and_overlong_mentions() {
        assert!(mentions("просто текст").is_empty());
        assert!(mentions("@ пробел").is_empty());
        assert!(mentions("@@").is_empty());
        let long = format!("@{}", "a".repeat(40));
        assert_eq!(mentions(&long)[0].len(), MAX_USERNAME);
    }
}

// ---------------------------------------------------------------- поиск по сообщениям

/// Длина поискового запроса по сообщениям.
pub const MAX_CHAT_QUERY: usize = 100;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ChatSearchQuery {
    /// Слова для поиска, 1–100 символов.
    #[param(example = "дюна")]
    pub q: String,
    /// Комната вида `entity:dune-2021`, `person:<slug>`, `tag:<slug>` или `room:<slug>`.
    /// Без неё — по всем моим чатам (нужен вход).
    pub room: Option<String>,
    /// 1–100, по умолчанию 50.
    pub limit: Option<i64>,
    /// Сообщения старше этого id (курсор `next_before`).
    pub before: Option<Uuid>,
}

/// Найденное сообщение вместе с комнатой: по `room` фронтенд её открывает.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSearchHit {
    /// Имя комнаты: `entity:<slug>`, `person:<slug>`, `tag:<slug>` или `room:<slug>`.
    #[schema(example = "entity:dune-2021")]
    pub room: String,
    pub message: ChatMessage,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSearchPage {
    pub items: Vec<ChatSearchHit>,
    /// Курсор дальше в прошлое; `null` — найдено всё.
    pub next_before: Option<Uuid>,
}

/// Имена комнат для найденных сообщений: по запросу на вид, не по запросу на сообщение.
async fn room_names(state: &AppState, items: &[ChatMessage]) -> AppResult<HashMap<Uuid, String>> {
    let ids_of = |kind: &str| -> Vec<Uuid> {
        items
            .iter()
            .filter(|message| message.kind == kind)
            .map(|message| message.target_id)
            .collect()
    };
    let mut names = HashMap::new();
    for (id, found) in refs::entities(state, ids_of("entity")).await? {
        names.insert(id, format!("entity:{}", found.slug));
    }
    for (id, found) in refs::people(state, ids_of("person")).await? {
        names.insert(id, format!("person:{}", found.slug));
    }
    for (id, found) in refs::tags(state, ids_of("tag")).await? {
        names.insert(id, format!("tag:{}", found.slug));
    }
    let rooms: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, slug FROM chat_rooms WHERE id = ANY($1)")
            .bind(ids_of("room"))
            .fetch_all(&state.db)
            .await?;
    for (id, slug) in rooms {
        names.insert(id, format!("room:{slug}"));
    }
    Ok(names)
}

/// Поиск по сообщениям: по словам тела, с русской морфологией (`websearch_to_tsquery`, индекс
/// `chat_messages_body_fts`). Без `room` — по всем моим чатам, поэтому нужен вход; с `room` —
/// по одной комнате, и это доступно гостю, как и её история. Удалённые не находятся.
#[utoipa::path(
    get, operation_id = "search_chat_messages", path = "/chat/search", tag = "chat",
    params(ChatSearchQuery),
    responses(
        (status = 200, description = "Найденные сообщения, новые сверху", body = ChatSearchPage),
        (status = 400, description = "Пустой или слишком длинный `q`, неверный `limit` или `room`", body = ErrorBody),
        (status = 401, description = "Без `room` нужен вход", body = ErrorBody),
        (status = 404, description = "Нет такой комнаты", body = ErrorBody),
    )
)]
pub async fn search(
    State(state): State<AppState>,
    viewer: Option<AuthUser>,
    Query(query): Query<ChatSearchQuery>,
) -> AppResult<Json<ChatSearchPage>> {
    let q = validate::required("q", &query.q, MAX_CHAT_QUERY)?;
    let limit = match query.limit {
        None => HISTORY_LIMIT,
        Some(limit) if (1..=MAX_HISTORY_LIMIT).contains(&limit) => limit,
        Some(_) => {
            return Err(AppError::BadRequest(format!(
                "limit must be between 1 and {MAX_HISTORY_LIMIT}"
            ))
            .on_field("limit"))
        }
    };

    // Область поиска: одна комната ($3) или все мои чаты ($2). Обе ветки упоминают оба
    // параметра — Postgres не принимает привязку неиспользованного.
    let (room_scope, mine_scope, user_id, target_id) = match &query.room {
        Some(name) => {
            let (kind, slug) = name
                .split_once(':')
                .ok_or_else(|| AppError::BadRequest("room must be <kind>:<slug>".into()))?;
            let room = room_by_slug(&state, kind, slug, viewer.as_ref().map(|u| u.id)).await?;
            (
                format!("r.{} = $3", column(room)),
                "$2::uuid IS NULL".to_string(),
                None,
                Some(room_parts(room).1),
            )
        }
        None => {
            let user = viewer.ok_or(AppError::Unauthorized)?;
            // Те же комнаты, что в GET /chats: интересы всех трёх видов, свои и системные.
            let mine = "r.entity_id IN (SELECT entity_id FROM user_interests WHERE user_id = $2)
                 OR r.person_id IN (SELECT person_id FROM user_person_interests WHERE user_id = $2)
                 OR r.tag_id IN (SELECT tag_id FROM user_tag_interests WHERE user_id = $2)
                 OR r.id IN (SELECT room_id FROM chat_room_members WHERE user_id = $2)
                 OR r.dm_user_a = $2 OR r.dm_user_b = $2
                 OR r.system";
            (
                "$3::uuid IS NULL".to_string(),
                mine.to_string(),
                Some(user.id),
                None,
            )
        }
    };
    let scope = format!("({room_scope}) AND ({mine_scope})");

    // Берём на одну строку больше запрошенного: есть ли что дальше в прошлое.
    let rows: Vec<MessageRow> = sqlx::query_as(&format!(
        "SELECT {MESSAGE_COLUMNS}, {ROOM_COLUMNS}
         FROM chat_messages m JOIN chat_rooms r ON r.id = m.room_id
         WHERE m.deleted_at IS NULL AND {scope}
           AND to_tsvector('russian', m.body) @@ websearch_to_tsquery('russian', $1)
           AND ($4::uuid IS NULL OR m.id < $4)
         ORDER BY m.id DESC LIMIT $5"
    ))
    .bind(&q)
    .bind(user_id)
    .bind(target_id)
    .bind(query.before)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    let has_more = rows.len() as i64 > limit;
    let rows: Vec<MessageRow> = rows.into_iter().take(limit as usize).collect();
    let items = messages(&state, rows).await?;
    let next_before = has_more
        .then(|| items.last().map(|message| message.id))
        .flatten();
    let names = room_names(&state, &items).await?;
    let items = items
        .into_iter()
        .filter_map(|message| {
            Some(ChatSearchHit {
                room: names.get(&message.target_id)?.clone(),
                message,
            })
        })
        .collect();
    Ok(Json(ChatSearchPage { items, next_before }))
}
