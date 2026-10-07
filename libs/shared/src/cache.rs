//! Кэш готовых JSON-ответов в два уровня: L1 — память процесса (moka), L2 — Redis.
//!
//! Что кэшировать и откуда брать при промахе обоих уровней, решает модуль (сейчас `catalog`:
//! карточки из Meilisearch). Здесь только уровни и правила:
//! - L1 → L2 → загрузчик; попадание в L2 кладётся в L1, результат загрузчика — в L1 сразу
//!   и в L2 в фоне (ответ не ждёт Redis);
//! - одновременные промахи по одному ключу дают одну загрузку;
//! - «не найдено» не кэшируется;
//! - ошибка, таймаут Redis или битый JSON — warning в лог и переход к загрузчику, а не ошибка запроса.

use crate::Config;
use async_trait::async_trait;
use deadpool_redis::redis;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::task::TaskTracker;

/// Сколько ждать Redis на одну операцию чтения или записи ключа.
const L2_TIMEOUT: Duration = Duration::from_millis(100);
/// Сколько ждать массовые операции (удаление по префиксу).
const L2_BULK_TIMEOUT: Duration = Duration::from_secs(10);
/// Через сколько повторить удаление при инвалидации.
const REDELETE_AFTER: Duration = Duration::from_secs(2);
/// Ключи сканируются пачками такого размера.
const SCAN_COUNT: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("redis: {0}")]
    Redis(String),
    #[error("redis timeout")]
    Timeout,
}

/// L2-хранилище. Трейт, чтобы в тестах подменять Redis моком (`MockL2Store`, feature `mock`).
#[cfg_attr(any(test, feature = "mock"), mockall::automock)]
#[async_trait]
pub trait L2Store: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>;
    async fn set_ex(&self, key: &str, value: &str, ttl: Duration) -> Result<(), CacheError>;
    async fn del(&self, keys: &[String]) -> Result<(), CacheError>;
    /// Удаляет все ключи с префиксом. Возвращает, сколько удалено.
    async fn del_prefix(&self, prefix: &str) -> Result<u64, CacheError>;
    /// `SET key NX EX ttl`: `true`, если ключ поставлен (блокировка взята).
    async fn try_lock(&self, key: &str, ttl: Duration) -> Result<bool, CacheError>;
}

/// Redis через пул соединений deadpool. Соединения открываются при первом запросе,
/// поэтому недоступный Redis не мешает старту приложения.
pub struct RedisStore {
    pool: deadpool_redis::Pool,
}

impl RedisStore {
    pub fn new(url: &str) -> Result<Self, CacheError> {
        let pool = deadpool_redis::Config::from_url(url)
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .map_err(|e| CacheError::Redis(e.to_string()))?;
        Ok(Self { pool })
    }

    async fn conn(&self) -> Result<deadpool_redis::Connection, CacheError> {
        self.pool
            .get()
            .await
            .map_err(|e| CacheError::Redis(e.to_string()))
    }
}

fn redis_error(error: redis::RedisError) -> CacheError {
    CacheError::Redis(error.to_string())
}

#[async_trait]
impl L2Store for RedisStore {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let mut conn = self.conn().await?;
        redis::cmd("GET")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    async fn set_ex(&self, key: &str, value: &str, ttl: Duration) -> Result<(), CacheError> {
        let mut conn = self.conn().await?;
        redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("EX")
            .arg(ttl.as_secs().max(1))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    async fn del(&self, keys: &[String]) -> Result<(), CacheError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn().await?;
        redis::cmd("DEL")
            .arg(keys)
            .query_async(&mut conn)
            .await
            .map_err(redis_error)
    }

    async fn del_prefix(&self, prefix: &str) -> Result<u64, CacheError> {
        let mut conn = self.conn().await?;
        let pattern = format!("{prefix}*");
        let mut cursor: u64 = 0;
        let mut deleted = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(SCAN_COUNT)
                .query_async(&mut conn)
                .await
                .map_err(redis_error)?;
            if !keys.is_empty() {
                let count: u64 = redis::cmd("DEL")
                    .arg(&keys)
                    .query_async(&mut conn)
                    .await
                    .map_err(redis_error)?;
                deleted += count;
            }
            if next == 0 {
                return Ok(deleted);
            }
            cursor = next;
        }
    }

    async fn try_lock(&self, key: &str, ttl: Duration) -> Result<bool, CacheError> {
        let mut conn = self.conn().await?;
        let reply: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(ttl.as_secs().max(1))
            .query_async(&mut conn)
            .await
            .map_err(redis_error)?;
        Ok(reply.is_some())
    }
}

/// Настройки кэша. Из env — [`CacheSettings::from_config`].
#[derive(Debug, Clone)]
pub struct CacheSettings {
    /// Префикс всех ключей (`nexus:v1:`); у каждого теста свой.
    pub prefix: String,
    pub l1_ttl: Duration,
    pub l1_capacity: u64,
    pub l2_ttl: Duration,
    /// Таймаут одной операции с L2.
    pub l2_timeout: Duration,
    /// Через сколько повторить удаление при инвалидации.
    pub redelete_after: Duration,
}

impl CacheSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            prefix: "nexus:v1:".into(),
            l1_ttl: Duration::from_secs(config.cache_l1_ttl_secs),
            l1_capacity: config.cache_l1_capacity,
            l2_ttl: Duration::from_secs(config.cache_l2_ttl_secs),
            l2_timeout: L2_TIMEOUT,
            redelete_after: REDELETE_AFTER,
        }
    }
}

/// Почему значения нет: не найдено в источнике (не кэшируется) или источник ответил ошибкой.
#[derive(Debug)]
pub enum Miss<E> {
    NotFound,
    Failed(E),
}

/// Хэндл кэша: дешёво клонируется. Выключенный (`disabled`) каждый раз зовёт загрузчик.
#[derive(Clone)]
pub struct Cache {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    l1: moka::future::Cache<String, Arc<str>>,
    l2: Arc<dyn L2Store>,
    settings: CacheSettings,
    /// Фоновые записи в L2 и повторные удаления: в тестах их можно дождаться.
    tasks: TaskTracker,
}

impl Cache {
    pub fn new(settings: CacheSettings, l2: Arc<dyn L2Store>) -> Self {
        let l1 = moka::future::Cache::builder()
            .max_capacity(settings.l1_capacity)
            .time_to_live(settings.l1_ttl)
            .build();
        Self {
            inner: Some(Arc::new(Inner {
                l1,
                l2,
                settings,
                tasks: TaskTracker::new(),
            })),
        }
    }

    /// Кэш с L2 в Redis. Неверный `url` — кэш выключен, ошибка в лог.
    pub fn redis(settings: CacheSettings, url: &str) -> Self {
        match RedisStore::new(url) {
            Ok(store) => Self::new(settings, Arc::new(store)),
            Err(error) => {
                tracing::error!(%error, "cache is disabled: bad REDIS_URL");
                Self::disabled()
            }
        }
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Значение по ключу: L1 → L2 → `load`. `load` возвращает JSON или `None` (не найдено).
    pub async fn get_or_load<F, Fut, E>(&self, key: &str, load: F) -> Result<Arc<str>, Arc<Miss<E>>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<String>, E>>,
        E: Send + Sync + 'static,
    {
        let Some(inner) = &self.inner else {
            return match load().await {
                Ok(Some(json)) => Ok(Arc::from(json)),
                Ok(None) => Err(Arc::new(Miss::NotFound)),
                Err(error) => Err(Arc::new(Miss::Failed(error))),
            };
        };
        let key = inner.key(key);
        // Одновременные промахи по ключу ждут одну загрузку; ошибка не кэшируется.
        inner
            .l1
            .try_get_with(key.clone(), Inner::fill(inner.clone(), key, load))
            .await
    }

    /// Удаляет ключи из L1 и L2 и через `redelete_after` ещё раз: фоновая запись промаха,
    /// начатого до изменения данных, могла вернуть старое значение.
    pub async fn invalidate(&self, keys: &[String]) {
        let Some(inner) = &self.inner else { return };
        if keys.is_empty() {
            return;
        }
        let keys: Vec<String> = keys.iter().map(|key| inner.key(key)).collect();
        inner.forget(&keys).await;
        let inner = inner.clone();
        inner.tasks.clone().spawn(async move {
            tokio::time::sleep(inner.settings.redelete_after).await;
            inner.forget(&keys).await;
        });
    }

    /// Удаляет из L2 все ключи с префиксом, L1 очищает целиком.
    pub async fn clear(&self, prefix: &str) {
        let Some(inner) = &self.inner else { return };
        inner.l1.invalidate_all();
        let prefix = inner.key(prefix);
        match tokio::time::timeout(L2_BULK_TIMEOUT, inner.l2.del_prefix(&prefix)).await {
            Ok(Ok(deleted)) => tracing::debug!(%prefix, deleted, "cache cleared"),
            Ok(Err(error)) => tracing::warn!(%error, %prefix, "cache clear failed"),
            Err(_) => tracing::warn!(%prefix, "cache clear timed out"),
        }
    }

    /// Блокировка в L2 на `ttl` (одна задача на все инстансы). Выключенный кэш — всегда `true`.
    pub async fn try_lock(&self, key: &str, ttl: Duration) -> Result<bool, CacheError> {
        let Some(inner) = &self.inner else {
            return Ok(true);
        };
        tokio::time::timeout(
            inner.settings.l2_timeout,
            inner.l2.try_lock(&inner.key(key), ttl),
        )
        .await
        .map_err(|_| CacheError::Timeout)?
    }

    /// Значение в L1, без похода в L2 и загрузчик. Для тестов и диагностики.
    pub async fn peek_l1(&self, key: &str) -> Option<Arc<str>> {
        let inner = self.inner.as_ref()?;
        inner.l1.get(&inner.key(key)).await
    }

    /// Полный ключ в Redis (с префиксом).
    pub fn full_key(&self, key: &str) -> String {
        match &self.inner {
            Some(inner) => inner.key(key),
            None => key.to_string(),
        }
    }

    /// Ждёт фоновые записи и повторные удаления. Для тестов.
    pub async fn wait_pending(&self) {
        let Some(inner) = &self.inner else { return };
        inner.tasks.close();
        inner.tasks.wait().await;
        inner.tasks.reopen();
    }
}

impl Inner {
    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.settings.prefix)
    }

    /// Промах L1: L2, затем загрузчик.
    async fn fill<F, Fut, E>(self: Arc<Self>, key: String, load: F) -> Result<Arc<str>, Miss<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<String>, E>>,
    {
        match tokio::time::timeout(self.settings.l2_timeout, self.l2.get(&key)).await {
            Ok(Ok(Some(json))) if is_json(&json) => return Ok(Arc::from(json)),
            Ok(Ok(Some(_))) => {
                tracing::warn!(%key, "cache L2 value is not JSON, dropping it");
                self.l2_del(std::slice::from_ref(&key)).await;
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => tracing::warn!(%error, %key, "cache L2 read failed"),
            Err(_) => tracing::warn!(%key, "cache L2 read timed out"),
        }

        let json = load().await.map_err(Miss::Failed)?.ok_or(Miss::NotFound)?;
        let value: Arc<str> = Arc::from(json);
        let inner = self.clone();
        let stored = value.clone();
        self.tasks.spawn(async move {
            let set = inner.l2.set_ex(&key, &stored, inner.settings.l2_ttl);
            match tokio::time::timeout(inner.settings.l2_timeout, set).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, %key, "cache L2 write failed"),
                Err(_) => tracing::warn!(%key, "cache L2 write timed out"),
            }
        });
        Ok(value)
    }

    async fn forget(&self, keys: &[String]) {
        for key in keys {
            self.l1.invalidate(key).await;
        }
        self.l2_del(keys).await;
    }

    async fn l2_del(&self, keys: &[String]) {
        match tokio::time::timeout(self.settings.l2_timeout, self.l2.del(keys)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, ?keys, "cache L2 delete failed"),
            Err(_) => tracing::warn!(?keys, "cache L2 delete timed out"),
        }
    }
}

fn is_json(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn settings() -> CacheSettings {
        CacheSettings {
            prefix: "t:".into(),
            l1_ttl: Duration::from_secs(30),
            l1_capacity: 100,
            l2_ttl: Duration::from_secs(60),
            l2_timeout: Duration::from_millis(50),
            redelete_after: Duration::from_millis(10),
        }
    }

    /// L2, который не отвечает дольше таймаута.
    struct HangingStore;

    #[async_trait]
    impl L2Store for HangingStore {
        async fn get(&self, _: &str) -> Result<Option<String>, CacheError> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(Some("\"stale\"".into()))
        }
        async fn set_ex(&self, _: &str, _: &str, _: Duration) -> Result<(), CacheError> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        }
        async fn del(&self, _: &[String]) -> Result<(), CacheError> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        }
        async fn del_prefix(&self, _: &str) -> Result<u64, CacheError> {
            Ok(0)
        }
        async fn try_lock(&self, _: &str, _: Duration) -> Result<bool, CacheError> {
            Ok(true)
        }
    }

    async fn load_ok(calls: &AtomicUsize) -> Result<Option<String>, CacheError> {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some("{\"v\":1}".into()))
    }

    #[tokio::test]
    async fn l2_timeout_falls_through_to_loader_quickly() {
        let cache = Cache::new(settings(), Arc::new(HangingStore));
        let calls = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let value = cache.get_or_load("k", || load_ok(&calls)).await.unwrap();
        assert_eq!(&*value, "{\"v\":1}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited for Redis"
        );
        // Фоновая запись тоже обрывается по таймауту, а не висит.
        tokio::time::timeout(Duration::from_secs(1), cache.wait_pending())
            .await
            .expect("pending L2 write is bounded by the timeout");
    }

    #[tokio::test]
    async fn invalidate_deletes_twice() {
        let mut store = MockL2Store::new();
        store
            .expect_del()
            .withf(|keys| keys == ["t:a".to_string(), "t:b".to_string()])
            .times(2)
            .returning(|_| Ok(()));
        let cache = Cache::new(settings(), Arc::new(store));
        cache.invalidate(&["a".into(), "b".into()]).await;
        cache.wait_pending().await;
    }

    #[tokio::test]
    async fn clear_drops_l1_and_l2_prefix() {
        let mut store = MockL2Store::new();
        store.expect_get().returning(|_| Ok(None));
        store.expect_set_ex().returning(|_, _, _| Ok(()));
        store
            .expect_del_prefix()
            .withf(|prefix| prefix == "t:catalog:")
            .times(1)
            .returning(|_| Ok(3));
        let cache = Cache::new(settings(), Arc::new(store));
        let calls = AtomicUsize::new(0);
        cache
            .get_or_load("catalog:x", || load_ok(&calls))
            .await
            .unwrap();
        assert!(cache.peek_l1("catalog:x").await.is_some());
        cache.clear("catalog:").await;
        assert!(cache.peek_l1("catalog:x").await.is_none());
    }

    #[tokio::test]
    async fn disabled_cache_always_loads() {
        let cache = Cache::disabled();
        let calls = AtomicUsize::new(0);
        cache.get_or_load("k", || load_ok(&calls)).await.unwrap();
        cache.get_or_load("k", || load_ok(&calls)).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(cache
            .try_lock("lock", Duration::from_secs(1))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn try_lock_uses_prefixed_key() {
        let mut store = MockL2Store::new();
        store
            .expect_try_lock()
            .withf(|key, ttl| key == "t:job" && *ttl == Duration::from_secs(60))
            .times(1)
            .returning(|_, _| Ok(false));
        let cache = Cache::new(settings(), Arc::new(store));
        assert!(!cache
            .try_lock("job", Duration::from_secs(60))
            .await
            .unwrap());
    }
}
