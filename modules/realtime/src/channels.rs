//! Имена каналов для клиента и их внутренний вид (`shared::events::Channel`).
//!
//! `thread:<id>` — тема форума, `entity:<slug>` — сущность каталога, `person:<slug>` — человек,
//! `tag:<slug>` — тег, `room:<slug>` — своя комната чата (внутри по id: события приходят с id;
//! у людей, тегов и комнат — только чат),
//! `user:me` — личный канал вошедшего, подписывается сам после `auth`.

use shared::directory::ChatRoom;
use shared::events::Channel;
use shared::{AppError, AppResult, AppState};
use uuid::Uuid;

pub const USER_ME: &str = "user:me";

/// Разобрать имя канала из `subscribe`: канал и его каноничное имя.
pub async fn parse(state: &AppState, name: &str) -> AppResult<(Channel, String)> {
    let bad = |message: String| AppError::BadRequest(message);
    match name.split_once(':') {
        Some(("thread", id)) => {
            let id: Uuid = id
                .parse()
                .map_err(|_| bad(format!("invalid thread id in channel: {name}")))?;
            Ok((Channel::Thread(id), format!("thread:{id}")))
        }
        Some(("entity", slug)) => {
            let entity = state
                .entities
                .by_slug(slug)
                .await?
                .ok_or_else(|| bad(format!("unknown entity in channel: {name}")))?;
            Ok((
                Channel::Entity(entity.id),
                format!("entity:{}", entity.slug),
            ))
        }
        Some(("person", slug)) => {
            let person = state
                .people
                .by_slug(slug)
                .await?
                .ok_or_else(|| bad(format!("unknown person in channel: {name}")))?;
            Ok((
                Channel::Person(person.id),
                format!("person:{}", person.slug),
            ))
        }
        Some(("room", slug)) => {
            let (id, slug) = state
                .chat
                .room_by_slug(state, slug)
                .await?
                .ok_or_else(|| bad(format!("unknown room in channel: {name}")))?;
            Ok((Channel::Room(id), format!("room:{slug}")))
        }
        Some(("tag", slug)) => {
            let tag = state
                .tags
                .by_slug(slug)
                .await?
                .ok_or_else(|| bad(format!("unknown tag in channel: {name}")))?;
            Ok((Channel::Tag(tag.id), format!("tag:{}", tag.slug)))
        }
        _ if name == USER_ME => Err(bad("user:me is subscribed automatically after auth".into())),
        _ => Err(bad(format!("unknown channel: {name}"))),
    }
}

/// Комната чата из `chat.send` / `chat.read`: `entity:`, `person:` или `tag:<slug>` → комната и
/// каноничное имя. Сообщения комнаты приходят в канал с тем же именем.
pub async fn chat_room(state: &AppState, name: &str) -> AppResult<(ChatRoom, String)> {
    let unknown = || AppError::BadRequest(format!("unknown chat room: {name}"));
    if !["entity:", "person:", "tag:", "room:"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Err(unknown());
    }
    match parse(state, name).await? {
        (Channel::Entity(id), name) => Ok((ChatRoom::Entity(id), name)),
        (Channel::Person(id), name) => Ok((ChatRoom::Person(id), name)),
        (Channel::Tag(id), name) => Ok((ChatRoom::Tag(id), name)),
        (Channel::Room(id), name) => Ok((ChatRoom::Room(id), name)),
        _ => Err(unknown()),
    }
}

/// Каналы сущностей по id (интересы): имена `entity:<slug>`. Удалённые сущности пропускаются.
pub async fn entities(state: &AppState, ids: &[Uuid]) -> AppResult<Vec<(Channel, String)>> {
    let refs = state.entities.by_ids(ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| {
            let entity = refs.get(id)?;
            Some((
                Channel::Entity(entity.id),
                format!("entity:{}", entity.slug),
            ))
        })
        .collect())
}

/// Каналы людей по id (интересы): имена `person:<slug>`. Удалённые пропускаются.
pub async fn people(state: &AppState, ids: &[Uuid]) -> AppResult<Vec<(Channel, String)>> {
    let refs = state.people.by_ids(ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| {
            let person = refs.get(id)?;
            Some((
                Channel::Person(person.id),
                format!("person:{}", person.slug),
            ))
        })
        .collect())
}

/// Каналы тегов по id (интересы): имена `tag:<slug>`. Удалённые пропускаются.
pub async fn tags(state: &AppState, ids: &[Uuid]) -> AppResult<Vec<(Channel, String)>> {
    let refs = state.tags.by_ids(ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| {
            let tag = refs.get(id)?;
            Some((Channel::Tag(tag.id), format!("tag:{}", tag.slug)))
        })
        .collect())
}

/// Каналы своих комнат по id (вступил): имена `room:<slug>`. Удалённые пропускаются.
pub async fn rooms(state: &AppState, ids: &[Uuid]) -> AppResult<Vec<(Channel, String)>> {
    let slugs = state.chat.room_slugs(state, ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| Some((Channel::Room(*id), format!("room:{}", slugs.get(id)?))))
        .collect())
}

/// Все каналы интересов пользователя: сущности, люди, теги и свои комнаты, в которые вступил.
pub async fn interests(state: &AppState, user_id: Uuid) -> AppResult<Vec<(Channel, String)>> {
    let mut channels = entities(state, &state.interests.entity_ids(user_id).await?).await?;
    channels.extend(people(state, &state.interests.person_ids(user_id).await?).await?);
    channels.extend(tags(state, &state.interests.tag_ids(user_id).await?).await?);
    channels.extend(rooms(state, &state.interests.room_ids(user_id).await?).await?);
    Ok(channels)
}
