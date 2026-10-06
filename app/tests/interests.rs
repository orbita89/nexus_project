//! Интересы модуля social: свои подписки на сущности и события для realtime.
//!
//! Сущности — `fixtures/catalog.sql`, пользователи создаются через dev login.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::events::{Channel, Event};
use shared::Config;
use sqlx::PgPool;
use std::sync::Arc;
use test_utils::request;
use tokio::sync::broadcast::Receiver;
use uuid::Uuid;

const SOCIAL: &str = "/api/v1/social";

struct Ctx {
    state: shared::AppState,
}

struct User {
    id: Uuid,
    token: String,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let mut config = Config::from_env();
        config.dev_login = true;
        let (state, _) = test_utils::state_with_config(pool, config);
        Self { state }
    }

    async fn expect(
        &self,
        status: StatusCode,
        method: Method,
        path: &str,
        user: Option<&User>,
    ) -> Value {
        let app = nexus::build_app(self.state.clone());
        let token = user.map(|user| user.token.as_str());
        let response = request(app, method.clone(), &format!("{SOCIAL}{path}"), None, token).await;
        let text = String::from_utf8_lossy(&response.body).to_string();
        assert_eq!(response.status, status, "{method} {path}: {text}");
        if response.body.is_empty() {
            return Value::Null;
        }
        let body = response.json();
        if !status.is_success() {
            assert!(body["error"].is_string(), "{method} {path}: {body}");
        }
        body
    }

    async fn login(&self, name: &str) -> User {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            Method::POST,
            "/api/v1/auth/dev/login",
            Some(json!({ "login": format!("{name}@example.com"), "role": "user" })),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body = response.json();
        User {
            id: body["user"]["id"].as_str().unwrap().parse().unwrap(),
            token: body["access_token"].as_str().unwrap().to_string(),
        }
    }

    async fn entity_id(&self, slug: &str) -> Uuid {
        sqlx::query_scalar("SELECT id FROM entities WHERE slug = $1")
            .bind(slug)
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}

/// Все события, уже опубликованные в шину.
fn drain(events: &mut Receiver<Arc<Event>>) -> Vec<Event> {
    std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| (*event).clone())
        .collect()
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn add_list_and_remove_interests(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.login("alice").await;
    let bob = ctx.login("bob").await;
    let mut events = ctx.state.events.subscribe();
    let dune = ctx.entity_id("dune-2021").await;

    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PUT,
        "/entities/dune-2021/interest",
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PUT,
        "/entities/unknown/interest",
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/dune-2021/interest",
        Some(&alice),
    )
    .await;

    for slug in ["dune-2021", "witcher-3", "dune-2021"] {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::PUT,
            &format!("/entities/{slug}/interest"),
            Some(&alice),
        )
        .await;
    }
    // Повторное добавление события не публикует.
    let published = drain(&mut events);
    assert_eq!(published.len(), 2);
    assert_eq!(published[0].kind, "interest.added");
    assert_eq!(published[0].channels, [Channel::User(alice.id)]);
    assert_eq!(published[0].data, json!({ "entity_id": dune }));

    let own = ctx
        .expect(
            StatusCode::OK,
            Method::GET,
            "/entities/dune-2021/interest",
            Some(&alice),
        )
        .await;
    assert_eq!(own["entity"]["slug"], "dune-2021");
    assert!(own["since"].is_string());

    let page = ctx
        .expect(StatusCode::OK, Method::GET, "/interests", Some(&alice))
        .await;
    assert_eq!(page["total"], 2);
    let slugs: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["entity"]["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["witcher-3", "dune-2021"]);
    let paged = ctx
        .expect(
            StatusCode::OK,
            Method::GET,
            "/interests?limit=1&offset=1",
            Some(&alice),
        )
        .await;
    assert_eq!(paged["items"][0]["entity"]["slug"], "dune-2021");

    // Интересы личные: у bob своих нет, чужих он не видит.
    let empty = ctx
        .expect(StatusCode::OK, Method::GET, "/interests", Some(&bob))
        .await;
    assert_eq!(empty["total"], 0);
    ctx.expect(StatusCode::UNAUTHORIZED, Method::GET, "/interests", None)
        .await;

    for _ in 0..2 {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::DELETE,
            "/entities/dune-2021/interest",
            Some(&alice),
        )
        .await;
    }
    let published = drain(&mut events);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].kind, "interest.removed");
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/dune-2021/interest",
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        "/entities/unknown/interest",
        Some(&alice),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn interests_are_limited(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.login("alice").await;
    sqlx::query(
        "INSERT INTO entities (kind, slug, title)
         SELECT 'book', 'bulk-' || n, 'Книга ' || n FROM generate_series(1, 500) AS n",
    )
    .execute(&ctx.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_interests (user_id, entity_id)
         SELECT $1, id FROM entities WHERE slug LIKE 'bulk-%'",
    )
    .bind(alice.id)
    .execute(&ctx.state.db)
    .await
    .unwrap();

    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PUT,
        "/entities/dune-2021/interest",
        Some(&alice),
    )
    .await;
    // Уже добавленная — по-прежнему 204.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/entities/bulk-1/interest",
        Some(&alice),
    )
    .await;
}
