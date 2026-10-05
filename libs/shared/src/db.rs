//! Пул соединений к PostgreSQL.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// Создаёт пул. Соединение устанавливается лениво: сервис поднимается, даже если
/// Postgres на пару секунд отстал со стартом, и переживает его перезапуск.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(database_url)
        .map(Ok)?
}
