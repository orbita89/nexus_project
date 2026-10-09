//! Пул соединений к PostgreSQL и помощники для запросов.

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

/// Шаблон `ILIKE '%...%'` с экранированием `%`, `_` и `\`. Пустой запрос — без фильтра.
pub fn like_pattern(q: &str) -> Option<String> {
    let q = q.trim();
    if q.is_empty() {
        return None;
    }
    let mut pattern = String::with_capacity(q.len() + 2);
    pattern.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    Some(pattern)
}

#[cfg(test)]
mod tests {
    use super::like_pattern;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("  "), None);
        assert_eq!(like_pattern("дюна").as_deref(), Some("%дюна%"));
        assert_eq!(like_pattern("100%_\\").as_deref(), Some("%100\\%\\_\\\\%"));
    }
}
