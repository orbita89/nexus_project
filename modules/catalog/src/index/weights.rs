//! Вес популярности в релевантности поиска по видам (`catalog_rank_kinds.search_popularity`):
//! несколько чисел в RAM. Загружаются при старте ([`super::sync::spawn_initial`]) и после
//! сохранения настроек в админке; до загрузки — [`POPULARITY_WEIGHT`].

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::sidecar::POPULARITY_WEIGHT;

#[derive(Debug, Default)]
pub struct Weights {
    search: HashMap<String, f32>,
}

impl Weights {
    /// Вес популярности вида (`movie`, `book`…, `person`).
    pub fn search(&self, kind: &str) -> f32 {
        self.search.get(kind).copied().unwrap_or(POPULARITY_WEIGHT)
    }
}

static WEIGHTS: RwLock<Option<Arc<Weights>>> = RwLock::new(None);

pub fn current() -> Arc<Weights> {
    WEIGHTS.read().unwrap().clone().unwrap_or_default()
}

/// Перечитать веса из БД.
pub async fn reload(db: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let rows: Vec<(String, f64)> =
        sqlx::query_as("SELECT kind, search_popularity FROM catalog_rank_kinds")
            .fetch_all(db)
            .await?;
    let search = rows
        .into_iter()
        .map(|(kind, weight)| (kind, weight as f32))
        .collect();
    *WEIGHTS.write().unwrap() = Some(Arc::new(Weights { search }));
    Ok(())
}
