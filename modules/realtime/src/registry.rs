//! Учёт соединений пользователей: не больше [`MAX_CONNECTIONS_PER_USER`](crate::MAX_CONNECTIONS_PER_USER).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
pub struct Registry {
    connections: Mutex<HashMap<Uuid, usize>>,
}

impl Registry {
    /// Занять место под соединение пользователя; мест нет — `None`. Место освобождается,
    /// когда [`Slot`] удалён (выход, истечение токена, разрыв).
    pub fn acquire(self: &Arc<Self>, user_id: Uuid, max: usize) -> Option<Slot> {
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let count = connections.entry(user_id).or_default();
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(Slot {
            registry: Arc::clone(self),
            user_id,
        })
    }
}

pub struct Slot {
    registry: Arc<Registry>,
    user_id: Uuid,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut connections = self
            .registry
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = connections.get_mut(&self.user_id) {
            *count -= 1;
            if *count == 0 {
                connections.remove(&self.user_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_limited_and_released() {
        let registry = Arc::new(Registry::default());
        let user = Uuid::new_v4();
        let a = registry.acquire(user, 2).unwrap();
        let _b = registry.acquire(user, 2).unwrap();
        assert!(registry.acquire(user, 2).is_none());
        assert!(registry.acquire(Uuid::new_v4(), 2).is_some());
        drop(a);
        assert!(registry.acquire(user, 2).is_some());
    }
}
