//! Лента модуля social: ярусы (подписки, интересы, популярное), причины показа, курсор.
//!
//! Сущности — `fixtures/catalog.sql`, пользователи создаются через dev login.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::Config;
use sqlx::PgPool;
use test_utils::request;

const SOCIAL: &str = "/api/v1/social";

struct Ctx {
    state: shared::AppState,
}

struct User {
    token: String,
    username: String,
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
        body: Option<Value>,
        user: Option<&User>,
    ) -> Value {
        let app = nexus::build_app(self.state.clone());
        let token = user.map(|user| user.token.as_str());
        let response = request(app, method.clone(), &format!("{SOCIAL}{path}"), body, token).await;
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

    async fn login(&self, name: &str, role: &str) -> User {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            Method::POST,
            "/api/v1/auth/dev/login",
            Some(json!({ "login": format!("{name}@example.com"), "role": role })),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let body = response.json();
        User {
            token: body["access_token"].as_str().unwrap().to_string(),
            username: body["user"]["username"].as_str().unwrap().to_string(),
        }
    }

    async fn feed(&self, query: &str, user: &User) -> Value {
        self.expect(
            StatusCode::OK,
            Method::GET,
            &format!("/feed{query}"),
            None,
            Some(user),
        )
        .await
    }

    async fn thread(&self, author: &User, title: &str, entities: &[&str]) -> String {
        let body = json!({ "title": title, "body": "Текст", "entities": entities });
        let created = self
            .expect(
                StatusCode::CREATED,
                Method::POST,
                "/threads",
                Some(body),
                Some(author),
            )
            .await;
        created["id"].as_str().unwrap().to_string()
    }

    async fn reply(&self, user: &User, thread: &str) {
        let body = json!({ "body": "Ответ" });
        let path = format!("/threads/{thread}/posts");
        self.expect(
            StatusCode::CREATED,
            Method::POST,
            &path,
            Some(body),
            Some(user),
        )
        .await;
    }

    async fn review(&self, user: &User, slug: &str, body: Value) -> String {
        let app = nexus::build_app(self.state.clone());
        let path = format!("{SOCIAL}/entities/{slug}/review");
        let response = request(app, Method::PUT, &path, Some(body), Some(&user.token)).await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json()["id"].as_str().unwrap().to_string()
    }

    async fn collection(&self, user: &User, title: &str, public: bool) -> String {
        let body = json!({ "title": title, "is_public": public });
        let created = self
            .expect(
                StatusCode::CREATED,
                Method::POST,
                "/collections",
                Some(body),
                Some(user),
            )
            .await;
        created["id"].as_str().unwrap().to_string()
    }
}

/// `тип:id` записей страницы.
fn keys(page: &Value) -> Vec<String> {
    page["items"].as_array().unwrap().iter().map(key).collect()
}

fn key(item: &Value) -> String {
    let kind = item["type"].as_str().unwrap();
    format!("{kind}:{}", item[kind]["id"].as_str().unwrap())
}

/// Причины записи: `follow`, `interest:<slug>`, `popular`.
fn reasons(item: &Value) -> Vec<String> {
    item["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reason| match reason["type"].as_str().unwrap() {
            "follow" => "follow".to_string(),
            "interest" => format!("interest:{}", reason["entity"]["slug"].as_str().unwrap()),
            other => other.to_string(),
        })
        .collect()
}

/// Данные для сценария: me подписан на bob и следит за «Дюной» 2021.
struct World {
    me: User,
    expected: Vec<String>,
}

async fn world(ctx: &Ctx) -> World {
    let me = ctx.login("me", "user").await;
    let bob = ctx.login("bob", "author").await;
    let carol = ctx.login("carol", "author").await;
    let dave = ctx.login("dave", "user").await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        &format!("/users/{}/follow", bob.username),
        None,
        Some(&me),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/entities/dune-2021/interest",
        None,
        Some(&me),
    )
    .await;

    // Популярное: темы не про интересы и не от подписок.
    let witcher = ctx.thread(&carol, "Ведьмак", &["witcher-3"]).await;
    ctx.reply(&dave, &witcher).await;
    ctx.reply(&dave, &witcher).await;
    let quiet = ctx.thread(&carol, "Тихая тема", &["no-date"]).await;
    // Подписки: любая рецензия (и без текста), публичная коллекция; приватной нет.
    let bob_review = ctx.review(&bob, "witcher-3", json!({ "rating": 9 })).await;
    let bob_collection = ctx.collection(&bob, "Подборка", true).await;
    ctx.collection(&bob, "Личное", false).await;
    // Интересы: рецензия с текстом и тема; голая оценка — нет.
    let carol_review = ctx
        .review(
            &carol,
            "dune-2021",
            json!({ "rating": 7, "body": "Красиво" }),
        )
        .await;
    ctx.review(&dave, "dune-2021", json!({ "rating": 5 })).await;
    let carol_thread = ctx.thread(&carol, "Про Дюну", &["dune-2021"]).await;
    // Тема bob про «Дюну»: и подписка, и интерес — один раз, в первом ярусе.
    let bob_thread = ctx
        .thread(&bob, "Дюна: книга и фильм", &["dune-novel", "dune-2021"])
        .await;
    // Своё в ленту не попадает.
    ctx.review(&me, "dune-2021", json!({ "rating": 10, "body": "Моё" }))
        .await;

    let expected = [
        format!("thread:{bob_thread}"),
        format!("collection:{bob_collection}"),
        format!("review:{bob_review}"),
        format!("thread:{carol_thread}"),
        format!("review:{carol_review}"),
        format!("thread:{witcher}"),
        format!("thread:{quiet}"),
    ]
    .to_vec();
    World { me, expected }
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn feed_goes_follows_then_interests_then_popular(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let World { me, expected } = world(&ctx).await;

    let page = ctx.feed("", &me).await;
    assert_eq!(keys(&page), expected);
    assert_eq!(page["next_cursor"], Value::Null);
    assert_eq!(page["limit"], 20);

    let items = page["items"].as_array().unwrap();
    let all: Vec<Vec<String>> = items.iter().map(reasons).collect();
    assert_eq!(all[0], ["follow", "interest:dune-2021"]);
    assert_eq!(all[1], ["follow"]);
    assert_eq!(all[2], ["follow"]);
    assert_eq!(all[3], ["interest:dune-2021"]);
    assert_eq!(all[4], ["interest:dune-2021"]);
    assert_eq!(all[5], ["popular"]);
    assert_eq!(all[6], ["popular"]);
    let follow = &items[0]["reasons"][0]["user"]["username"];
    assert!(follow.as_str().unwrap().starts_with("bob"), "{follow}");
    // Запись — тот же DTO, что в остальном API.
    assert_eq!(items[0]["thread"]["entities"].as_array().unwrap().len(), 2);
    assert_eq!(items[2]["review"]["rating"], 9);
    assert_eq!(items[1]["collection"]["is_public"], true);
    assert!(items[0].get("review").is_none());
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn cursor_walks_through_tiers_without_duplicates(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let World { me, expected } = world(&ctx).await;

    for limit in [1, 2, 3, 6] {
        let mut seen = Vec::new();
        let mut query = format!("?limit={limit}");
        loop {
            let page = ctx.feed(&query, &me).await;
            let page_keys = keys(&page);
            assert!(page_keys.len() <= limit);
            seen.extend(page_keys);
            match page["next_cursor"].as_str() {
                Some(cursor) => query = format!("?limit={limit}&cursor={cursor}"),
                None => break,
            }
        }
        assert_eq!(seen, expected, "limit={limit}");
    }

    // Новая запись сверху не сдвигает следующую страницу.
    let first = ctx.feed("?limit=2", &me).await;
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let bob = ctx.login("bob", "author").await;
    ctx.thread(&bob, "Совсем новая", &["witcher-3"]).await;
    let second = ctx.feed(&format!("?limit=2&cursor={cursor}"), &me).await;
    assert_eq!(keys(&second), expected[2..4]);

    for bad in ["?cursor=garbage", "?cursor=", "?type=post", "?limit=x"] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::GET,
            &format!("/feed{bad}"),
            None,
            Some(&me),
        )
        .await;
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn feed_filters_by_type(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let World { me, expected } = world(&ctx).await;
    let only = |prefix: &str| -> Vec<String> {
        expected
            .iter()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect()
    };

    assert_eq!(keys(&ctx.feed("?type=thread", &me).await), only("thread:"));
    assert_eq!(keys(&ctx.feed("?type=review", &me).await), only("review:"));
    assert_eq!(
        keys(&ctx.feed("?type=collection", &me).await),
        only("collection:")
    );
    let paged = ctx.feed("?type=thread&limit=3", &me).await;
    let cursor = paged["next_cursor"].as_str().unwrap();
    let rest = ctx
        .feed(&format!("?type=thread&limit=3&cursor={cursor}"), &me)
        .await;
    assert_eq!(keys(&rest), only("thread:")[3..]);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn newcomer_sees_popular_threads(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let World { expected, .. } = world(&ctx).await;
    let newcomer = ctx.login("newcomer", "user").await;

    // Ни подписок, ни интересов: только популярное, все темы — больше сообщений сверху.
    let page = ctx.feed("", &newcomer).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|item| reasons(item) == ["popular"]));
    assert_eq!(key(&items[0]), expected[5]);
    assert_eq!(items[0]["thread"]["posts_count"], 2);

    ctx.expect(StatusCode::UNAUTHORIZED, Method::GET, "/feed", None, None)
        .await;
}
