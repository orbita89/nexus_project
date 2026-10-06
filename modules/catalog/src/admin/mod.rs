//! Управление каталогом: только роль `admin` (extractor `AdminUser`). Пути `/admin/...`.
//! После каждой записи затронутые сущности переотправляются в поисковый индекс.

pub mod entities;
pub mod people;
pub mod tags;

use uuid::Uuid;

/// Значение для `UPDATE ... SET col = CASE WHEN $set THEN $value ELSE col END`:
/// поле не передано — `(false, None)`, передан `null` — `(true, None)`.
pub(crate) fn nullable_param<T>(value: Option<Option<T>>) -> (bool, Option<T>) {
    match value {
        None => (false, None),
        Some(value) => (true, value),
    }
}

/// Сущности, которые надо переиндексировать после изменения тега или человека.
pub(crate) async fn affected_entities(
    db: &sqlx::PgPool,
    sql: &str,
    id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(sql).bind(id).fetch_all(db).await
}
