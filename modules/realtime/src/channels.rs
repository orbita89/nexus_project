//! Имена каналов для клиента и их внутренний вид (`shared::events::Channel`).
//!
//! `thread:<id>` — тема форума, `entity:<slug>` — сущность каталога (внутри по id: события
//! приходят с id), `user:me` — личный канал вошедшего, подписывается сам после `auth`.

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
        _ if name == USER_ME => Err(bad("user:me is subscribed automatically after auth".into())),
        _ => Err(bad(format!("unknown channel: {name}"))),
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
