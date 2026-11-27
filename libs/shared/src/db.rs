//! Пул соединений к PostgreSQL и помощники для запросов.

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// Соединений в пуле по умолчанию (`DB_POOL_MAX`). Каждое — отдельный процесс Postgres
/// (~5–10 МБ): на слабом сервере пять хватает, чат пишет пачками.
const DEFAULT_POOL_MAX: u32 = 5;

/// Создаёт пул. Соединение устанавливается лениво: сервис поднимается, даже если
/// Postgres на пару секунд отстал со стартом, и переживает его перезапуск.
/// Размер — `DB_POOL_MAX`; простаивающие соединения закрываются (процессы PG не держат память).
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let max = pool_max(std::env::var("DB_POOL_MAX").ok().as_deref());
    PgPoolOptions::new()
        .max_connections(max)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(300))
        .max_lifetime(Duration::from_secs(1800))
        .connect_lazy(database_url)
        .map(Ok)?
}

fn pool_max(value: Option<&str>) -> u32 {
    value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_POOL_MAX)
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
    use super::{like_pattern, pool_max, DEFAULT_POOL_MAX};

    #[test]
    fn pool_max_defaults_and_parses() {
        assert_eq!(pool_max(None), DEFAULT_POOL_MAX);
        assert_eq!(pool_max(Some("0")), DEFAULT_POOL_MAX);
        assert_eq!(pool_max(Some("x")), DEFAULT_POOL_MAX);
        assert_eq!(pool_max(Some(" 8 ")), 8);
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("  "), None);
        assert_eq!(like_pattern("дюна").as_deref(), Some("%дюна%"));
        assert_eq!(like_pattern("100%_\\").as_deref(), Some("%100\\%\\_\\\\%"));
    }
}
