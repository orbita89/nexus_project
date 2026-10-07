//! Карточки сущностей и людей для чтения: L1 (память) → L2 (Redis) → L3 (Meilisearch).
//!
//! В PostgreSQL чтение карточки не ходит. Готовая карточка (`card`) лежит в документе
//! поискового индекса, его собирает и обновляет [`crate::search`]. Нет в Meilisearch — 404,
//! Meilisearch недоступен — 503; то, что уже в L1 и L2, отдаётся и без него.

use crate::search::{self, INDEX as ENTITIES_INDEX, PEOPLE_INDEX};
use crate::validate;
use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::{header, Method};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use shared::cache::{Cache, Miss};
use shared::search::{Search, SearchError};
use shared::{AppError, AppResult};
use std::sync::Arc;

/// Префикс ключей карточек в кэше: `catalog:entity:{slug}`, `catalog:person:{slug}`.
pub const KEY_PREFIX: &str = "catalog:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Entity,
    Person,
}

impl CardKind {
    fn index(self) -> &'static str {
        match self {
            Self::Entity => ENTITIES_INDEX,
            Self::Person => PEOPLE_INDEX,
        }
    }

    /// Ключ карточки в кэше (без общего префикса кэша).
    pub fn key(self, slug: &str) -> String {
        match self {
            Self::Entity => format!("{KEY_PREFIX}entity:{slug}"),
            Self::Person => format!("{KEY_PREFIX}person:{slug}"),
        }
    }
}

/// L3 — откуда брать карточку при промахе кэша. Трейт, чтобы в тестах подменять Meilisearch.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait CardSource: Send + Sync {
    /// JSON карточки или `None`, если её нет.
    async fn card(&self, kind: CardKind, slug: &str) -> Result<Option<String>, SearchError>;
}

/// Карточки из документов Meilisearch (поле `card`).
pub struct MeiliCards(pub Search);

#[async_trait]
impl CardSource for MeiliCards {
    async fn card(&self, kind: CardKind, slug: &str) -> Result<Option<String>, SearchError> {
        let path = format!("/indexes/{}/documents/fetch", self.0.index(kind.index()));
        // slug уже проверен validate::slug: кавычек и пробелов в нём нет.
        let body =
            json!({ "filter": format!("slug = \"{slug}\""), "limit": 1, "fields": ["card"] });
        let response = match self.0.call(Method::POST, &path, Some(&body)).await {
            Ok(response) => response,
            // Индекс ещё не построен — карточки нет.
            Err(error) if error.code() == Some("index_not_found") => return Ok(None),
            Err(error) => return Err(error),
        };
        match response["results"].get(0).map(|doc| &doc["card"]) {
            None => Ok(None),
            Some(card) if card.is_object() => Ok(Some(card.to_string())),
            Some(_) => {
                tracing::warn!(slug, ?kind, "search document has no card, run reindex");
                Ok(None)
            }
        }
    }
}

/// Карточка по slug через кэш. JSON отдаётся как есть, без разбора.
pub async fn read(
    cache: &Cache,
    source: &dyn CardSource,
    kind: CardKind,
    slug: &str,
) -> AppResult<Arc<str>> {
    // Такого slug не может быть в каталоге, а в фильтр Meilisearch он не должен попасть.
    if validate::slug("slug", slug).is_err() {
        return Err(AppError::NotFound);
    }
    let loaded = cache
        .get_or_load(&kind.key(slug), || source.card(kind, slug))
        .await;
    loaded.map_err(|miss| match &*miss {
        Miss::NotFound => AppError::NotFound,
        Miss::Failed(SearchError::Disabled) => search::unavailable(),
        Miss::Failed(error) => {
            tracing::warn!(%error, slug, ?kind, "card lookup in search failed");
            search::unavailable()
        }
    })
}

/// Ответ `200 application/json` из готового JSON без копирования.
pub fn json_response(json: Arc<str>) -> Response {
    struct JsonBytes(Arc<str>);
    impl AsRef<[u8]> for JsonBytes {
        fn as_ref(&self) -> &[u8] {
            self.0.as_bytes()
        }
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(Bytes::from_owner(JsonBytes(json))),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    //! Цепочка L1 → L2 → L3 на моках: Redis — `MockL2Store`, Meilisearch — `MockCardSource`.
    //! Число вызовов каждого уровня проверяется через `times`.

    use super::*;
    use axum::http::StatusCode;
    use mockall::predicate::eq;
    use shared::cache::{CacheError, CacheSettings, MockL2Store};
    use std::time::Duration;

    const CARD: &str = r#"{"slug":"dune-2021","title":"Дюна"}"#;
    const KEY: &str = "t:catalog:entity:dune-2021";

    fn settings(l1_ttl: Duration) -> CacheSettings {
        CacheSettings {
            prefix: "t:".into(),
            l1_ttl,
            l1_capacity: 100,
            l2_ttl: Duration::from_secs(86_400),
            l2_timeout: Duration::from_millis(100),
            redelete_after: Duration::from_millis(10),
        }
    }

    fn cache(store: MockL2Store) -> Cache {
        Cache::new(settings(Duration::from_secs(30)), Arc::new(store))
    }

    fn source_returning(times: usize, card: Option<&'static str>) -> MockCardSource {
        let mut source = MockCardSource::new();
        source
            .expect_card()
            .with(eq(CardKind::Entity), eq("dune-2021"))
            .times(times)
            .returning(move |_, _| Ok(card.map(str::to_string)));
        source
    }

    async fn read_dune(cache: &Cache, source: &MockCardSource) -> AppResult<Arc<str>> {
        read(cache, source, CardKind::Entity, "dune-2021").await
    }

    fn status(error: AppError) -> StatusCode {
        error.into_response().status()
    }

    /// 1) Холодный старт: L2 пуст, L3 вызывается ровно 1 раз, карточка попадает в L1 и L2.
    #[tokio::test]
    async fn cold_start_loads_from_l3_once_and_fills_l1_and_l2() {
        let mut store = MockL2Store::new();
        store
            .expect_get()
            .with(eq(KEY))
            .times(1)
            .returning(|_| Ok(None));
        store
            .expect_set_ex()
            .withf(|key, value, ttl| {
                key == KEY && value == CARD && *ttl == Duration::from_secs(86_400)
            })
            .times(1)
            .returning(|_, _, _| Ok(()));
        let cache = cache(store);
        let source = source_returning(1, Some(CARD));

        assert_eq!(&*read_dune(&cache, &source).await.unwrap(), CARD);
        cache.wait_pending().await;
        assert_eq!(
            cache.peek_l1("catalog:entity:dune-2021").await.as_deref(),
            Some(CARD)
        );
    }

    /// 2) L1 hit: второй запрос не трогает ни Redis, ни Meilisearch.
    #[tokio::test]
    async fn l1_hit_skips_l2_and_l3() {
        let mut store = MockL2Store::new();
        store.expect_get().times(1).returning(|_| Ok(None));
        store.expect_set_ex().times(1).returning(|_, _, _| Ok(()));
        let cache = cache(store);
        let source = source_returning(1, Some(CARD));

        read_dune(&cache, &source).await.unwrap();
        cache.wait_pending().await;
        // times(1) выше: повторный вызов L2 или L3 уронит тест.
        assert_eq!(&*read_dune(&cache, &source).await.unwrap(), CARD);
        assert_eq!(&*read_dune(&cache, &source).await.unwrap(), CARD);
    }

    /// 3) L1 истёк, в L2 есть: запрос идёт в Redis, но не в Meilisearch, и снова кладётся в L1.
    /// Часы moka не подчиняются tokio::time::advance, поэтому TTL L1 короткий и настоящий sleep.
    #[tokio::test]
    async fn l1_expired_l2_hit_does_not_reach_l3() {
        let mut store = MockL2Store::new();
        let mut seq = mockall::Sequence::new();
        store
            .expect_get()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));
        store
            .expect_get()
            .with(eq(KEY))
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(Some(CARD.to_string())));
        store.expect_set_ex().times(1).returning(|_, _, _| Ok(()));
        let cache = Cache::new(settings(Duration::from_millis(50)), Arc::new(store));
        let source = source_returning(1, Some(CARD));

        read_dune(&cache, &source).await.unwrap();
        cache.wait_pending().await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(cache.peek_l1("catalog:entity:dune-2021").await.is_none());

        assert_eq!(&*read_dune(&cache, &source).await.unwrap(), CARD);
        assert!(cache.peek_l1("catalog:entity:dune-2021").await.is_some());
    }

    /// 4) L2 miss → L3 hit → записано в L2 и L1; следующий запрос из L1.
    #[tokio::test]
    async fn l2_miss_fills_l2_from_l3() {
        let mut store = MockL2Store::new();
        store.expect_get().times(1).returning(|_| Ok(None));
        store
            .expect_set_ex()
            .withf(|key, value, _| key == KEY && value == CARD)
            .times(1)
            .returning(|_, _, _| Ok(()));
        let cache = cache(store);
        let source = source_returning(1, Some(CARD));

        read_dune(&cache, &source).await.unwrap();
        cache.wait_pending().await;
        read_dune(&cache, &source).await.unwrap();
    }

    /// 5) В L3 нет → 404, ничего не кэшируется: следующий запрос снова идёт в L2 и L3.
    #[tokio::test]
    async fn not_found_in_l3_is_404_and_not_cached() {
        let mut store = MockL2Store::new();
        store.expect_get().times(2).returning(|_| Ok(None));
        store.expect_set_ex().never();
        let cache = cache(store);
        let source = source_returning(2, None);

        for _ in 0..2 {
            let error = read_dune(&cache, &source).await.unwrap_err();
            assert_eq!(status(error), StatusCode::NOT_FOUND);
        }
        cache.wait_pending().await;
        assert!(cache.peek_l1("catalog:entity:dune-2021").await.is_none());
    }

    /// 6) Meilisearch упал → 503; но то, что уже в L1 или L2, отдаётся.
    #[tokio::test]
    async fn l3_failure_is_503_but_cached_cards_are_served() {
        let mut store = MockL2Store::new();
        store.expect_get().with(eq(KEY)).returning(|_| Ok(None));
        store
            .expect_get()
            .with(eq("t:catalog:entity:dune-1984"))
            .times(1)
            .returning(|_| Ok(Some(CARD.to_string())));
        store.expect_set_ex().never();
        let cache = cache(store);
        let mut source = MockCardSource::new();
        source
            .expect_card()
            .with(eq(CardKind::Entity), eq("dune-2021"))
            .times(1)
            .returning(|_, _| Err(SearchError::Disabled));

        let error = read_dune(&cache, &source).await.unwrap_err();
        assert_eq!(status(error), StatusCode::SERVICE_UNAVAILABLE);
        // dune-1984 есть в L2: Meilisearch не нужен (expect_card для него не задан).
        let card = read(&cache, &source, CardKind::Entity, "dune-1984").await;
        assert_eq!(&*card.unwrap(), CARD);
        // А теперь и в L1.
        let card = read(&cache, &source, CardKind::Entity, "dune-1984").await;
        assert_eq!(&*card.unwrap(), CARD);
    }

    /// 7) Redis отвечает ошибкой → карточка из Meilisearch, 200, без паники; битый JSON → DEL.
    #[tokio::test]
    async fn l2_errors_fall_through_to_l3() {
        let mut store = MockL2Store::new();
        store
            .expect_get()
            .times(1)
            .returning(|_| Err(CacheError::Redis("connection refused".into())));
        store
            .expect_set_ex()
            .times(1)
            .returning(|_, _, _| Err(CacheError::Redis("connection refused".into())));
        let cache = cache(store);
        let source = source_returning(1, Some(CARD));
        assert_eq!(&*read_dune(&cache, &source).await.unwrap(), CARD);
        cache.wait_pending().await;

        let mut store = MockL2Store::new();
        store
            .expect_get()
            .times(1)
            .returning(|_| Ok(Some("{not json".into())));
        store
            .expect_del()
            .withf(|keys| keys == [KEY.to_string()])
            .times(1)
            .returning(|_| Ok(()));
        store.expect_set_ex().times(1).returning(|_, _, _| Ok(()));
        let cache = self::cache(store);
        let source = source_returning(1, Some(CARD));
        assert_eq!(&*read_dune(&cache, &source).await.unwrap(), CARD);
        cache.wait_pending().await;
    }

    /// 8) Одновременные промахи по одной карточке — один запрос в Redis и в Meilisearch.
    #[tokio::test]
    async fn concurrent_misses_load_once() {
        let mut store = MockL2Store::new();
        store.expect_get().times(1).returning(|_| Ok(None));
        store.expect_set_ex().times(1).returning(|_, _, _| Ok(()));
        let cache = cache(store);
        let source = source_returning(1, Some(CARD));

        let reads = (0..20).map(|_| read_dune(&cache, &source));
        for card in futures_util::future::join_all(reads).await {
            assert_eq!(&*card.unwrap(), CARD);
        }
        cache.wait_pending().await;
    }

    /// 9) После инвалидации запрос снова идёт в L2 и L3.
    #[tokio::test]
    async fn invalidate_forces_reload() {
        let mut store = MockL2Store::new();
        store.expect_get().times(2).returning(|_| Ok(None));
        store.expect_set_ex().times(2).returning(|_, _, _| Ok(()));
        // Сразу и повторно через redelete_after.
        store
            .expect_del()
            .withf(|keys| keys == [KEY.to_string()])
            .times(2)
            .returning(|_| Ok(()));
        let cache = cache(store);
        let source = source_returning(2, Some(CARD));

        read_dune(&cache, &source).await.unwrap();
        cache.wait_pending().await;
        cache.invalidate(&[CardKind::Entity.key("dune-2021")]).await;
        cache.wait_pending().await;
        read_dune(&cache, &source).await.unwrap();
        cache.wait_pending().await;
    }

    /// Невалидный slug не доходит ни до кэша, ни до фильтра Meilisearch.
    #[tokio::test]
    async fn invalid_slug_is_404_without_lookups() {
        let mut store = MockL2Store::new();
        store.expect_get().never();
        let cache = cache(store);
        let mut source = MockCardSource::new();
        source.expect_card().never();
        let error = read(&cache, &source, CardKind::Entity, "x\" OR slug = \"y")
            .await
            .unwrap_err();
        assert_eq!(status(error), StatusCode::NOT_FOUND);
    }
}
