//! Учёт соединений: у пользователя одно ([`MAX_CONNECTIONS_PER_USER`](crate::MAX_CONNECTIONS_PER_USER)),
//! новое вытесняет прежнее.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
pub struct Registry {
    /// Пользователь → (номер соединения, сигнал «тебя заменили»).
    connections: Mutex<HashMap<Uuid, (u64, CancellationToken)>>,
    next: AtomicU64,
    /// Все открытые соединения процесса.
    total: AtomicUsize,
}

impl Registry {
    /// Место под любое соединение (до апгрейда); больше `max` — `None`.
    pub fn connect(self: &Arc<Self>, max: usize) -> Option<ConnGuard> {
        self.total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(ConnGuard(Arc::clone(self)))
    }

    /// Занять место пользователя. Прежнее соединение того же пользователя получает сигнал
    /// [`Slot::replaced`] и закрывается само. Место освобождается, когда [`Slot`] удалён.
    pub fn acquire(self: &Arc<Self>, user_id: Uuid) -> Slot {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let replaced = CancellationToken::new();
        let mut connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, old)) = connections.insert(user_id, (id, replaced.clone())) {
            old.cancel();
        }
        Slot {
            registry: Arc::clone(self),
            user_id,
            id,
            replaced,
        }
    }

    /// Открытых соединений пользователя (0 или 1).
    #[cfg(test)]
    pub fn user_connections(&self, user_id: Uuid) -> usize {
        let connections = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        usize::from(connections.contains_key(&user_id))
    }
}

/// Занятое место в общем лимите соединений; освобождается при drop.
pub struct ConnGuard(Arc<Registry>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.total.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Slot {
    registry: Arc<Registry>,
    user_id: Uuid,
    id: u64,
    replaced: CancellationToken,
}

impl Slot {
    /// Срабатывает, когда тот же пользователь вошёл в другом соединении.
    pub fn replaced(&self) -> CancellationToken {
        self.replaced.clone()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut connections = self
            .registry
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Место уже заняло новое соединение — его не трогаем.
        if connections
            .get(&self.user_id)
            .is_some_and(|(id, _)| *id == self.id)
        {
            connections.remove(&self.user_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_slot_replaces_old_one() {
        let registry = Arc::new(Registry::default());
        let user = Uuid::new_v4();
        let a = registry.acquire(user);
        assert!(!a.replaced().is_cancelled());
        let b = registry.acquire(user);
        assert!(a.replaced().is_cancelled());
        assert!(!b.replaced().is_cancelled());
        // Другой пользователь не мешает.
        let c = registry.acquire(Uuid::new_v4());
        assert!(!c.replaced().is_cancelled());
        // Старое место уходит — новое остаётся.
        drop(a);
        assert_eq!(registry.user_connections(user), 1);
        drop(b);
        assert_eq!(registry.user_connections(user), 0);
    }

    #[test]
    fn total_connections_are_limited_and_released() {
        let registry = Arc::new(Registry::default());
        let a = registry.connect(2).unwrap();
        let _b = registry.connect(2).unwrap();
        assert!(registry.connect(2).is_none());
        drop(a);
        assert!(registry.connect(2).is_some());
    }
}
