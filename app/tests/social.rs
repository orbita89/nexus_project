//! Социальный модуль: рецензии и оценки, подписки, коллекции, модерация. Форум — `forum.rs`.
//!
//! Сущности — `fixtures/catalog.sql`, пользователи создаются через dev login.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::directory::{async_trait, EntityDirectory, EntityRef};
use shared::{AppResult, AppState, Config};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use test_utils::{request, TestResponse};
use uuid::Uuid;

const SOCIAL: &str = "/api/v1/social";

struct Ctx {
    state: AppState,
}

/// Пользователь, вошедший через dev login.
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

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        user: Option<&User>,
    ) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        let token = user.map(|user| user.token.as_str());
        request(app, method, &format!("{SOCIAL}{path}"), body, token).await
    }

    /// Запрос, ожидающий `status`: тело ответа (`null` — тела нет). Любая ошибка — `{"error": "..."}`.
    async fn expect(
        &self,
        status: StatusCode,
        method: Method,
        path: &str,
        body: Option<Value>,
        user: Option<&User>,
    ) -> Value {
        let response = self.send(method.clone(), path, body, user).await;
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

    async fn get_ok(&self, path: &str, user: Option<&User>) -> Value {
        self.expect(StatusCode::OK, Method::GET, path, None, user)
            .await
    }

    async fn login(&self, email: &str, role: &str) -> User {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            Method::POST,
            "/api/v1/auth/dev/login",
            Some(json!({ "login": email, "role": role })),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        let body = response.json();
        User {
            token: body["access_token"].as_str().unwrap().to_string(),
            username: body["user"]["username"].as_str().unwrap().to_string(),
        }
    }

    async fn user(&self, name: &str) -> User {
        self.login(&format!("{name}@example.com"), "user").await
    }

    async fn review(&self, user: &User, slug: &str, body: Value) -> Value {
        let response = self
            .send(
                Method::PUT,
                &format!("/entities/{slug}/review"),
                Some(body),
                Some(user),
            )
            .await;
        assert!(
            response.status == StatusCode::CREATED || response.status == StatusCode::OK,
            "{:?}",
            response.json()
        );
        response.json()
    }

    async fn collection(&self, user: &User, body: Value) -> String {
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

    async fn add_item(&self, user: &User, collection: &str, slug: &str) {
        self.expect(
            StatusCode::CREATED,
            Method::PUT,
            &format!("/collections/{collection}/items/{slug}"),
            Some(json!({})),
            Some(user),
        )
        .await;
    }
}

fn items(page: &Value) -> &Vec<Value> {
    page["items"].as_array().unwrap()
}

/// Значения поля `field` (путь через `/`) у элементов массива.
fn pluck<'a>(list: &'a [Value], field: &str) -> Vec<&'a str> {
    list.iter()
        .map(|item| {
            field
                .split('/')
                .fold(item, |value, key| &value[key])
                .as_str()
                .unwrap_or_else(|| panic!("{field} in {item}"))
        })
        .collect()
}

// ---------------------------------------------------------------- рецензии

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn put_review_creates_then_replaces(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;

    let created = ctx
        .expect(
            StatusCode::CREATED,
            Method::PUT,
            "/entities/dune-2021/review",
            Some(json!({ "rating": 9, "body": "  Красиво.  " })),
            Some(&alice),
        )
        .await;
    assert_eq!(created["rating"], 9);
    assert_eq!(created["body"], "Красиво.");
    assert_eq!(created["author"]["username"], alice.username.as_str());
    assert_eq!(created["entity"]["slug"], "dune-2021");
    assert_eq!(created["entity"]["title"], "Дюна");
    assert_eq!(created["entity"]["kind"], "movie");

    // Повторный PUT заменяет целиком: текст убран, оценка изменена, id тот же.
    let replaced = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            "/entities/dune-2021/review",
            Some(json!({ "rating": 7 })),
            Some(&alice),
        )
        .await;
    assert_eq!(replaced["id"], created["id"]);
    assert_eq!(replaced["rating"], 7);
    assert_eq!(replaced["body"], Value::Null);

    let own = ctx.get_ok("/entities/dune-2021/review", Some(&alice)).await;
    assert_eq!(own["id"], created["id"]);
    assert_eq!(own["rating"], 7);

    // Чужой рецензии на другую сущность у alice нет.
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/witcher-3/review",
        None,
        Some(&alice),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn put_review_validates(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let path = "/entities/dune-2021/review";

    for body in [
        json!({ "rating": 0 }),
        json!({ "rating": 11 }),
        json!({}),
        json!({ "body": "   " }),
        json!({ "body": "x".repeat(10_001) }),
        json!({ "rating": 5, "extra": 1 }),
        json!({ "rating": "five" }),
    ] {
        let response = ctx
            .send(Method::PUT, path, Some(body.clone()), Some(&alice))
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{body}");
        assert!(response.json()["error"].is_string());
    }
    // Ровно 10 000 символов — можно.
    ctx.review(&alice, "dune-2021", json!({ "body": "ы".repeat(10_000) }))
        .await;

    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PUT,
        "/entities/no-such-entity/review",
        Some(json!({ "rating": 5 })),
        Some(&alice),
    )
    .await;
    for method in [Method::GET, Method::PUT, Method::DELETE] {
        ctx.expect(
            StatusCode::UNAUTHORIZED,
            method,
            path,
            Some(json!({ "rating": 5 })),
            None,
        )
        .await;
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn delete_own_review(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    ctx.review(&alice, "dune-2021", json!({ "rating": 8 }))
        .await;
    ctx.review(&bob, "dune-2021", json!({ "rating": 3 })).await;

    let path = "/entities/dune-2021/review";
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        path,
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        path,
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(StatusCode::NOT_FOUND, Method::GET, path, None, Some(&alice))
        .await;

    // Рецензия bob на месте.
    let summary = ctx.get_ok("/entities/dune-2021/rating", None).await;
    assert_eq!(summary["count"], 1);
    assert_eq!(summary["average"], 3.0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn entity_reviews_list(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let carol = ctx.user("carol").await;
    let dave = ctx.user("dave").await;
    // Порядок создания: alice, bob, carol, dave (dave — самая свежая).
    ctx.review(
        &alice,
        "dune-2021",
        json!({ "rating": 6, "body": "Неплохо" }),
    )
    .await;
    ctx.review(&bob, "dune-2021", json!({ "body": "Без оценки" }))
        .await;
    ctx.review(&carol, "dune-2021", json!({ "rating": 10 }))
        .await;
    ctx.review(
        &dave,
        "dune-2021",
        json!({ "rating": 9, "body": "Отлично" }),
    )
    .await;
    ctx.review(
        &alice,
        "witcher-3",
        json!({ "rating": 10, "body": "Другая сущность" }),
    )
    .await;

    // По умолчанию: только с текстом, новые сверху.
    let page = ctx.get_ok("/entities/dune-2021/reviews", None).await;
    assert_eq!(page["total"], 3);
    assert_eq!(
        pluck(items(&page), "author/username"),
        [&dave.username, &bob.username, &alice.username]
    );
    assert_eq!(pluck(items(&page), "entity/slug"), ["dune-2021"; 3]);

    // all=true — вместе с оценками без текста.
    let page = ctx
        .get_ok("/entities/dune-2021/reviews?all=true", None)
        .await;
    assert_eq!(page["total"], 4);
    assert_eq!(
        items(&page)[1]["author"]["username"],
        carol.username.as_str()
    );
    assert_eq!(items(&page)[1]["body"], Value::Null);

    // Сортировки по оценке: без оценки — в конце.
    let page = ctx
        .get_ok(
            "/entities/dune-2021/reviews?all=true&sort=rating_desc",
            None,
        )
        .await;
    let ratings: Vec<&Value> = items(&page).iter().map(|r| &r["rating"]).collect();
    assert_eq!(ratings, [&json!(10), &json!(9), &json!(6), &Value::Null]);
    let page = ctx
        .get_ok("/entities/dune-2021/reviews?all=true&sort=rating_asc", None)
        .await;
    let ratings: Vec<&Value> = items(&page).iter().map(|r| &r["rating"]).collect();
    assert_eq!(ratings, [&json!(6), &json!(9), &json!(10), &Value::Null]);

    // Пагинация.
    let page = ctx
        .get_ok("/entities/dune-2021/reviews?limit=1&offset=1", None)
        .await;
    assert_eq!(page["total"], 3);
    assert_eq!(page["limit"], 1);
    assert_eq!(page["offset"], 1);
    assert_eq!(pluck(items(&page), "author/username"), [&bob.username]);

    let page = ctx.get_ok("/entities/dune-novel/reviews", None).await;
    assert_eq!(page["total"], 0);

    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/entities/dune-2021/reviews?sort=best",
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/no-such-entity/reviews",
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn rating_summary(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let empty = ctx.get_ok("/entities/dune-2021/rating", None).await;
    assert_eq!(
        empty,
        json!({ "average": null, "count": 0, "distribution": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0] })
    );

    for (name, review) in [
        ("alice", json!({ "rating": 10 })),
        ("bob", json!({ "rating": 8, "body": "Хорошо" })),
        ("carol", json!({ "rating": 8 })),
        ("dave", json!({ "body": "Без оценки не считается" })),
    ] {
        let user = ctx.user(name).await;
        ctx.review(&user, "dune-2021", review).await;
    }

    // (10 + 8 + 8) / 3 = 8.666... → 8.7
    let summary = ctx.get_ok("/entities/dune-2021/rating", None).await;
    assert_eq!(
        summary,
        json!({ "average": 8.7, "count": 3, "distribution": [0, 0, 0, 0, 0, 0, 0, 2, 0, 1] })
    );

    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/no-such-entity/rating",
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn user_reviews_list(pool: PgPool) {
    let ctx = Ctx::new(pool.clone());
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    ctx.review(
        &alice,
        "dune-novel",
        json!({ "rating": 9, "body": "Книга" }),
    )
    .await;
    ctx.review(&alice, "dune-2021", json!({ "rating": 8 }))
        .await;
    ctx.review(&bob, "witcher-3", json!({ "rating": 10 })).await;

    // Все рецензии пользователя, включая оценки без текста; новые сверху.
    let page = ctx
        .get_ok(&format!("/users/{}/reviews", alice.username), None)
        .await;
    assert_eq!(page["total"], 2);
    assert_eq!(
        pluck(items(&page), "entity/slug"),
        ["dune-2021", "dune-novel"]
    );
    assert_eq!(items(&page)[1]["entity"]["kind"], "book");

    // username без учёта регистра.
    let page = ctx
        .get_ok(
            &format!("/users/{}/reviews?limit=1", alice.username.to_uppercase()),
            None,
        )
        .await;
    assert_eq!(page["total"], 2);
    assert_eq!(items(&page).len(), 1);

    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/users/nobody_here/reviews",
        None,
        None,
    )
    .await;

    // Заблокированного пользователя для профиля нет, но его рецензии в списках остаются.
    sqlx::query("UPDATE users SET is_active = false WHERE username = $1::citext")
        .bind(&alice.username)
        .execute(&pool)
        .await
        .unwrap();
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        &format!("/users/{}/reviews", alice.username),
        None,
        None,
    )
    .await;
    let page = ctx.get_ok("/entities/dune-novel/reviews", None).await;
    assert_eq!(pluck(items(&page), "author/username"), [&alice.username]);
}

// ---------------------------------------------------------------- подписки

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn follow_and_unfollow(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let carol = ctx.user("carol").await;
    let follow = |user: &User| format!("/users/{}/follow", user.username);

    // Повторная подписка — тоже 204.
    for _ in 0..2 {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::PUT,
            &follow(&bob),
            None,
            Some(&alice),
        )
        .await;
    }
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        &follow(&bob),
        None,
        Some(&carol),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        &follow(&carol),
        None,
        Some(&alice),
    )
    .await;

    let followers = ctx
        .get_ok(&format!("/users/{}/followers", bob.username), None)
        .await;
    assert_eq!(followers["total"], 2);
    assert_eq!(
        pluck(items(&followers), "user/username"),
        [&carol.username, &alice.username]
    );
    assert!(items(&followers)[0]["since"].is_string());

    let following = ctx
        .get_ok(
            &format!("/users/{}/following?limit=1", alice.username),
            None,
        )
        .await;
    assert_eq!(following["total"], 2);
    assert_eq!(pluck(items(&following), "user/username"), [&carol.username]);

    // Отписка идемпотентна.
    for _ in 0..2 {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::DELETE,
            &follow(&bob),
            None,
            Some(&alice),
        )
        .await;
    }
    let followers = ctx
        .get_ok(&format!("/users/{}/followers", bob.username), None)
        .await;
    assert_eq!(pluck(items(&followers), "user/username"), [&carol.username]);

    let empty = ctx
        .get_ok(&format!("/users/{}/following", bob.username), None)
        .await;
    assert_eq!(empty["total"], 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("../../seeds/dev.sql"))]
async fn follow_rejects_bad_targets(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;

    let error = ctx
        .expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            &format!("/users/{}/follow", alice.username),
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(error["error"], "cannot follow yourself");
    // Нет такого и заблокированный (`blocked` из seeds/dev.sql).
    for username in ["nobody_here", "blocked"] {
        ctx.expect(
            StatusCode::NOT_FOUND,
            Method::PUT,
            &format!("/users/{username}/follow"),
            None,
            Some(&alice),
        )
        .await;
        ctx.expect(
            StatusCode::NOT_FOUND,
            Method::GET,
            &format!("/users/{username}/followers"),
            None,
            None,
        )
        .await;
    }
    for method in [Method::PUT, Method::DELETE] {
        ctx.expect(
            StatusCode::UNAUTHORIZED,
            method,
            "/users/user/follow",
            None,
            None,
        )
        .await;
    }
}

// ---------------------------------------------------------------- профиль

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn social_profile(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let carol = ctx.user("carol").await;
    for (follower, followee) in [(&bob, &alice), (&carol, &alice), (&alice, &bob)] {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::PUT,
            &format!("/users/{}/follow", followee.username),
            None,
            Some(follower),
        )
        .await;
    }
    ctx.review(&alice, "dune-2021", json!({ "rating": 8 }))
        .await;
    ctx.review(&alice, "dune-novel", json!({ "body": "Книга" }))
        .await;
    ctx.collection(&alice, json!({ "title": "Публичная" }))
        .await;
    ctx.collection(&alice, json!({ "title": "Приватная", "is_public": false }))
        .await;
    let path = format!("/users/{}", alice.username);

    // Гость: счётчики без приватного, отношения нет.
    let profile = ctx.get_ok(&path, None).await;
    assert_eq!(profile["user"]["username"], alice.username.as_str());
    assert_eq!(profile["followers_count"], 2);
    assert_eq!(profile["following_count"], 1);
    assert_eq!(profile["reviews_count"], 2);
    assert_eq!(profile["collections_count"], 1);
    assert_eq!(profile["relation"], Value::Null);

    // Владелец: приватные коллекции в счётчике, отношения к себе нет.
    let own = ctx.get_ok(&path, Some(&alice)).await;
    assert_eq!(own["collections_count"], 2);
    assert_eq!(own["relation"], Value::Null);

    // bob подписан на alice, и alice на bob.
    let seen_by_bob = ctx.get_ok(&path, Some(&bob)).await;
    assert_eq!(seen_by_bob["collections_count"], 1);
    assert_eq!(
        seen_by_bob["relation"],
        json!({ "following": true, "followed_by": true })
    );
    // carol подписана на alice, alice на carol — нет.
    let seen_by_carol = ctx.get_ok(&path, Some(&carol)).await;
    assert_eq!(
        seen_by_carol["relation"],
        json!({ "following": true, "followed_by": false })
    );
    // alice глазами carol: следует carol за alice? нет; alice за carol? нет — наоборот.
    let carol_seen_by_alice = ctx
        .get_ok(&format!("/users/{}", carol.username), Some(&alice))
        .await;
    assert_eq!(
        carol_seen_by_alice["relation"],
        json!({ "following": false, "followed_by": true })
    );

    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/users/nobody_here",
        None,
        None,
    )
    .await;
}

// ---------------------------------------------------------------- коллекции

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn create_and_get_collection(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;

    let created = ctx
        .expect(
            StatusCode::CREATED,
            Method::POST,
            "/collections",
            Some(json!({ "title": "  Дюна везде ", "description": "  " })),
            Some(&alice),
        )
        .await;
    assert_eq!(created["title"], "Дюна везде");
    assert_eq!(created["description"], Value::Null);
    assert_eq!(created["is_public"], true);
    assert_eq!(created["items_count"], 0);
    assert_eq!(created["items"], json!([]));
    assert_eq!(created["owner"]["username"], alice.username.as_str());

    let id = created["id"].as_str().unwrap();
    let got = ctx.get_ok(&format!("/collections/{id}"), None).await;
    assert_eq!(got, created);

    for body in [
        json!({ "title": "" }),
        json!({ "title": "x".repeat(201) }),
        json!({ "title": "ok", "description": "x".repeat(2001) }),
        json!({ "title": "ok", "owner": "x" }),
        json!({}),
    ] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::POST,
            "/collections",
            Some(body),
            Some(&alice),
        )
        .await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        "/collections",
        Some(json!({ "title": "ok" })),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        &format!("/collections/{}", Uuid::new_v4()),
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/collections/not-a-uuid",
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn private_collections_are_visible_only_to_owner(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let public = ctx
        .collection(&alice, json!({ "title": "Публичная" }))
        .await;
    let private = ctx
        .collection(&alice, json!({ "title": "Приватная", "is_public": false }))
        .await;
    ctx.add_item(&alice, &public, "dune-2021").await;
    ctx.add_item(&alice, &private, "dune-2021").await;

    // Карточка приватной: владелец — да, гость и чужой — 404.
    let path = format!("/collections/{private}");
    ctx.get_ok(&path, Some(&alice)).await;
    ctx.expect(StatusCode::NOT_FOUND, Method::GET, &path, None, None)
        .await;
    ctx.expect(StatusCode::NOT_FOUND, Method::GET, &path, None, Some(&bob))
        .await;

    // Коллекции пользователя: владелец видит обе, остальные — только публичную.
    let path = format!("/users/{}/collections", alice.username);
    let own = ctx.get_ok(&path, Some(&alice)).await;
    assert_eq!(own["total"], 2);
    assert_eq!(pluck(items(&own), "title"), ["Приватная", "Публичная"]);
    assert_eq!(items(&own)[0]["items_count"], 1);
    for viewer in [None, Some(&bob)] {
        let page = ctx.get_ok(&path, viewer).await;
        assert_eq!(pluck(items(&page), "title"), ["Публичная"]);
    }

    // Лента публичных и «в каких коллекциях есть сущность» — без приватных, даже для владельца.
    let page = ctx.get_ok("/collections", Some(&alice)).await;
    assert_eq!(pluck(items(&page), "title"), ["Публичная"]);
    let page = ctx
        .get_ok("/entities/dune-2021/collections", Some(&alice))
        .await;
    assert_eq!(page["total"], 1);
    assert_eq!(pluck(items(&page), "title"), ["Публичная"]);

    // Недействительный токен — не гость, а 401.
    let broken = User {
        token: "broken".into(),
        username: String::new(),
    };
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::GET,
        &path,
        None,
        Some(&broken),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/users/nobody_here/collections",
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn public_collections_and_entity_collections(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let first = ctx.collection(&alice, json!({ "title": "Первая" })).await;
    let second = ctx.collection(&bob, json!({ "title": "Вторая" })).await;
    ctx.collection(&bob, json!({ "title": "Третья" })).await;
    ctx.add_item(&alice, &first, "dune-novel").await;
    ctx.add_item(&bob, &second, "dune-novel").await;
    ctx.add_item(&bob, &second, "witcher-3").await;

    let page = ctx.get_ok("/collections", None).await;
    assert_eq!(page["total"], 3);
    assert_eq!(pluck(items(&page), "title"), ["Третья", "Вторая", "Первая"]);
    let page = ctx.get_ok("/collections?limit=1&offset=1", None).await;
    assert_eq!(pluck(items(&page), "title"), ["Вторая"]);
    assert_eq!(items(&page)[0]["items_count"], 2);
    assert_eq!(items(&page)[0]["owner"]["username"], bob.username.as_str());

    let page = ctx.get_ok("/entities/dune-novel/collections", None).await;
    assert_eq!(pluck(items(&page), "title"), ["Вторая", "Первая"]);
    let page = ctx.get_ok("/entities/dune-2021/collections", None).await;
    assert_eq!(page["total"], 0);
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/no-such-entity/collections",
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn update_collection(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let id = ctx
        .collection(
            &alice,
            json!({ "title": "Старое", "description": "Описание" }),
        )
        .await;
    let path = format!("/collections/{id}");

    // Не переданные поля не меняются.
    let updated = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(json!({ "title": "Новое" })),
            Some(&alice),
        )
        .await;
    assert_eq!(updated["title"], "Новое");
    assert_eq!(updated["description"], "Описание");
    assert_eq!(updated["is_public"], true);

    // null очищает описание.
    let updated = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(json!({ "description": null, "is_public": false })),
            Some(&alice),
        )
        .await;
    assert_eq!(updated["title"], "Новое");
    assert_eq!(updated["description"], Value::Null);
    assert_eq!(updated["is_public"], false);

    // Чужая приватная — 404, чужая публичная — 403.
    let change = Some(json!({ "title": "Взлом" }));
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        &path,
        change.clone(),
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::OK,
        Method::PATCH,
        &path,
        Some(json!({ "is_public": true })),
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::PATCH,
        &path,
        change.clone(),
        Some(&bob),
    )
    .await;
    ctx.expect(StatusCode::UNAUTHORIZED, Method::PATCH, &path, change, None)
        .await;

    for body in [json!({ "title": " " }), json!({ "is_public": "yes" })] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::PATCH,
            &path,
            Some(body),
            Some(&alice),
        )
        .await;
    }
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        &format!("/collections/{}", Uuid::new_v4()),
        Some(json!({ "title": "x" })),
        Some(&alice),
    )
    .await;
    assert_eq!(ctx.get_ok(&path, None).await["title"], "Новое");
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn delete_collection(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let id = ctx.collection(&alice, json!({ "title": "Удалить" })).await;
    ctx.add_item(&alice, &id, "dune-2021").await;
    let path = format!("/collections/{id}");

    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &path,
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(StatusCode::UNAUTHORIZED, Method::DELETE, &path, None, None)
        .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &path,
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        &path,
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &path,
        None,
        Some(&alice),
    )
    .await;
    let page = ctx.get_ok("/entities/dune-2021/collections", None).await;
    assert_eq!(page["total"], 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn collection_items(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let id = ctx.collection(&alice, json!({ "title": "Дюна" })).await;
    let item = |slug: &str| format!("/collections/{id}/items/{slug}");

    // Без position — в конец: 0, 1, 2. Тело можно не передавать.
    let first = ctx
        .expect(
            StatusCode::CREATED,
            Method::PUT,
            &item("dune-novel"),
            Some(json!({ "note": " Первоисточник " })),
            Some(&alice),
        )
        .await;
    assert_eq!(first["position"], 0);
    assert_eq!(first["note"], "Первоисточник");
    assert_eq!(first["entity"]["slug"], "dune-novel");
    let second = ctx
        .expect(
            StatusCode::CREATED,
            Method::PUT,
            &item("dune-2021"),
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(second["position"], 1);
    assert_eq!(second["note"], Value::Null);
    ctx.add_item(&alice, &id, "dune-part-two-2024").await;

    let collection = ctx.get_ok(&format!("/collections/{id}"), None).await;
    assert_eq!(collection["items_count"], 3);
    assert_eq!(
        pluck(collection["items"].as_array().unwrap(), "entity/slug"),
        ["dune-novel", "dune-2021", "dune-part-two-2024"]
    );

    // Изменение: позиция меняется, заметка без поля не трогается, null — очищает.
    let changed = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &item("dune-novel"),
            Some(json!({ "position": 10 })),
            Some(&alice),
        )
        .await;
    assert_eq!(changed["position"], 10);
    assert_eq!(changed["note"], "Первоисточник");
    let changed = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &item("dune-novel"),
            Some(json!({ "note": null })),
            Some(&alice),
        )
        .await;
    assert_eq!(changed["position"], 10);
    assert_eq!(changed["note"], Value::Null);
    let collection = ctx.get_ok(&format!("/collections/{id}"), None).await;
    assert_eq!(
        pluck(collection["items"].as_array().unwrap(), "entity/slug"),
        ["dune-2021", "dune-part-two-2024", "dune-novel"]
    );

    // Удаление пункта.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &item("dune-2021"),
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &item("dune-2021"),
        None,
        Some(&alice),
    )
    .await;
    let collection = ctx.get_ok(&format!("/collections/{id}"), None).await;
    assert_eq!(collection["items_count"], 2);

    // Ошибки.
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PUT,
        &item("witcher-3"),
        Some(json!({ "note": "x".repeat(1001) })),
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PUT,
        &item("no-such-entity"),
        Some(json!({})),
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::PUT,
        &item("witcher-3"),
        Some(json!({})),
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &item("dune-novel"),
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PUT,
        &item("witcher-3"),
        Some(json!({})),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PUT,
        &format!("/collections/{}/items/witcher-3", Uuid::new_v4()),
        Some(json!({})),
        Some(&alice),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn collection_items_limit(pool: PgPool) {
    let ctx = Ctx::new(pool.clone());
    let alice = ctx.user("alice").await;
    let id = ctx.collection(&alice, json!({ "title": "Полная" })).await;

    // 500 пунктов вставляем напрямую: в фикстуре всего 5 сущностей.
    sqlx::query(
        "INSERT INTO entities (kind, slug, title)
         SELECT 'book', 'filler-' || n, 'Книга ' || n FROM generate_series(1, 500) n;
         ",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collection_items (collection_id, entity_id, position)
         SELECT $1, id, 0 FROM entities WHERE slug LIKE 'filler-%'",
    )
    .bind(Uuid::parse_str(&id).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let error = ctx
        .expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            &format!("/collections/{id}/items/dune-2021"),
            Some(json!({})),
            Some(&alice),
        )
        .await;
    assert_eq!(error["error"], "collection already has 500 items");
    // Изменить существующий пункт полной коллекции можно.
    ctx.expect(
        StatusCode::OK,
        Method::PUT,
        &format!("/collections/{id}/items/filler-1"),
        Some(json!({ "note": "ok" })),
        Some(&alice),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn reorder_collection_items(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let bob = ctx.user("bob").await;
    let id = ctx.collection(&alice, json!({ "title": "Порядок" })).await;
    for slug in ["dune-novel", "dune-2021", "dune-part-two-2024"] {
        ctx.add_item(&alice, &id, slug).await;
    }
    let path = format!("/collections/{id}/items");

    let reordered = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &path,
            Some(json!({ "entities": ["dune-part-two-2024", "dune-novel", "dune-2021"] })),
            Some(&alice),
        )
        .await;
    let list = reordered["items"].as_array().unwrap();
    assert_eq!(
        pluck(list, "entity/slug"),
        ["dune-part-two-2024", "dune-novel", "dune-2021"]
    );
    let positions: Vec<&Value> = list.iter().map(|item| &item["position"]).collect();
    assert_eq!(positions, [&json!(0), &json!(1), &json!(2)]);

    for entities in [
        json!(["dune-novel", "dune-2021"]),
        json!(["dune-novel", "dune-2021", "dune-2021"]),
        json!(["dune-novel", "dune-2021", "witcher-3"]),
        json!(["dune-novel", "dune-2021", "dune-part-two-2024", "witcher-3"]),
    ] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            &path,
            Some(json!({ "entities": entities })),
            Some(&alice),
        )
        .await;
    }
    let body = Some(json!({ "entities": ["dune-novel", "dune-2021", "dune-part-two-2024"] }));
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::PUT,
        &path,
        body.clone(),
        Some(&bob),
    )
    .await;
    ctx.expect(StatusCode::UNAUTHORIZED, Method::PUT, &path, body, None)
        .await;
}

// ---------------------------------------------------------------- модерация

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_deletes_reviews_and_collections(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login("admin@example.com", "admin").await;
    let author = ctx.login("author@example.com", "author").await;
    let alice = ctx.user("alice").await;
    let review = ctx
        .review(&alice, "dune-2021", json!({ "body": "Спам" }))
        .await;
    let review = format!("/admin/reviews/{}", review["id"].as_str().unwrap());
    let collection = ctx
        .collection(&alice, json!({ "title": "Спам", "is_public": false }))
        .await;
    let collection = format!("/admin/collections/{collection}");

    for path in [&review, &collection] {
        ctx.expect(StatusCode::UNAUTHORIZED, Method::DELETE, path, None, None)
            .await;
        for user in [&alice, &author] {
            ctx.expect(
                StatusCode::FORBIDDEN,
                Method::DELETE,
                path,
                None,
                Some(user),
            )
            .await;
        }
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::DELETE,
            path,
            None,
            Some(&admin),
        )
        .await;
        ctx.expect(
            StatusCode::NOT_FOUND,
            Method::DELETE,
            path,
            None,
            Some(&admin),
        )
        .await;
    }

    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/dune-2021/review",
        None,
        Some(&alice),
    )
    .await;
    let page = ctx
        .get_ok(
            &format!("/users/{}/collections", alice.username),
            Some(&alice),
        )
        .await;
    assert_eq!(page["total"], 0);
}

// ---------------------------------------------------------------- границы модулей

/// Справочник сущностей, отдающий для `dune-2021` своё название: видно, что social берёт
/// данные сущности из справочника, а не из таблицы `entities`.
struct FakeEntities {
    dune: EntityRef,
}

#[async_trait]
impl EntityDirectory for FakeEntities {
    async fn by_slug(&self, slug: &str) -> AppResult<Option<EntityRef>> {
        Ok((slug == "renamed").then(|| self.dune.clone()))
    }

    async fn by_ids(&self, ids: &[Uuid]) -> AppResult<HashMap<Uuid, EntityRef>> {
        Ok(ids
            .iter()
            .filter(|id| **id == self.dune.id)
            .map(|id| (*id, self.dune.clone()))
            .collect())
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn social_reads_entities_through_directory(pool: PgPool) {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM entities WHERE slug = 'dune-2021'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut ctx = Ctx::new(pool);
    ctx.state.entities = Arc::new(FakeEntities {
        dune: EntityRef {
            id,
            kind: "movie".into(),
            slug: "renamed".into(),
            title: "Из справочника".into(),
            cover_url: None,
        },
    });
    let alice = ctx.user("alice").await;

    let review = ctx
        .review(&alice, "renamed", json!({ "rating": 9, "body": "Да" }))
        .await;
    assert_eq!(review["entity"]["title"], "Из справочника");
    let page = ctx.get_ok("/entities/renamed/reviews", None).await;
    assert_eq!(pluck(items(&page), "entity/title"), ["Из справочника"]);

    // Slug из каталога справочник не знает — для social такой сущности нет.
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/dune-2021/reviews",
        None,
        None,
    )
    .await;
}

// ---------------------------------------------------------------- события для realtime

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn reviews_publish_events(pool: PgPool) {
    use shared::events::Channel;

    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let admin = ctx.login("admin@example.com", "admin").await;
    let mut events = ctx.state.events.subscribe();
    let mut next = || {
        let event = events.try_recv().expect("event");
        (event.kind, event.channels.clone(), event.data.clone())
    };

    let created = ctx
        .review(&alice, "dune-2021", json!({ "rating": 9 }))
        .await;
    let (kind, channels, data) = next();
    assert_eq!(kind, "review.created");
    let Channel::Entity(entity) = channels[0] else {
        panic!("{channels:?}")
    };
    assert_eq!(data["review_id"], created["id"]);
    assert_eq!(data["entity_id"], entity.to_string());
    assert_eq!(data["author_id"], created["author"]["id"]);

    ctx.review(&alice, "dune-2021", json!({ "rating": 7 }))
        .await;
    assert_eq!(next().0, "review.updated");
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/entities/dune-2021/review",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(next().0, "review.deleted");

    let created = ctx
        .review(&alice, "witcher-3", json!({ "rating": 3 }))
        .await;
    next();
    let id = created["id"].as_str().unwrap();
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/admin/reviews/{id}"),
        None,
        Some(&admin),
    )
    .await;
    let (kind, _, data) = next();
    assert_eq!(kind, "review.deleted");
    assert_eq!(data["review_id"], id);
}

// ---------------------------------------------------------------- дамп

/// `seeds/social.sql` загружается поверх `seeds/dev.sql` и `seeds/catalog.sql` и читается через API.
#[sqlx::test(
    migrator = "nexus::MIGRATOR",
    fixtures(
        "../../seeds/dev.sql",
        "../../seeds/catalog.sql",
        "../../seeds/social.sql"
    )
)]
async fn seed_social_is_valid(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let reviews = ctx.get_ok("/users/user/reviews", None).await;
    assert!(reviews["total"].as_i64().unwrap() >= 5, "{reviews}");
    let summary = ctx.get_ok("/entities/dune-2021/rating", None).await;
    assert!(summary["count"].as_i64().unwrap() >= 2, "{summary}");
    let page = ctx
        .get_ok("/entities/dune-2021/reviews?all=true", None)
        .await;
    assert!(page["total"].as_i64().unwrap() >= 2);

    let followers = ctx.get_ok("/users/author/followers", None).await;
    assert!(followers["total"].as_i64().unwrap() >= 2, "{followers}");

    let collections = ctx.get_ok("/collections", None).await;
    assert!(collections["total"].as_i64().unwrap() >= 2, "{collections}");
    for collection in items(&collections) {
        let id = collection["id"].as_str().unwrap();
        let detail = ctx.get_ok(&format!("/collections/{id}"), None).await;
        assert!(!detail["items"].as_array().unwrap().is_empty(), "{detail}");
    }

    // У user есть приватная коллекция: гостю не видна, владельцу — видна.
    let user = ctx.login("user", "user").await;
    let own = ctx.get_ok("/users/user/collections", Some(&user)).await;
    let public = ctx.get_ok("/users/user/collections", None).await;
    assert!(own["total"].as_i64().unwrap() > public["total"].as_i64().unwrap());

    // Форум: тема про «Дюну» видна у книги и у фильма, у каждой темы есть сущности.
    let threads = ctx.get_ok("/threads", None).await;
    assert!(threads["total"].as_i64().unwrap() >= 4, "{threads}");
    for thread in items(&threads) {
        assert!(
            !thread["entities"].as_array().unwrap().is_empty(),
            "{thread}"
        );
    }
    let book = ctx.get_ok("/entities/dune-novel/threads", None).await;
    let movie = ctx.get_ok("/entities/dune-2021/threads", None).await;
    let dune = &items(&book)[0];
    assert_eq!(dune["id"], items(&movie)[0]["id"]);
    assert_eq!(dune["posts_count"], 5);

    // Ветки и заглушка удалённого сообщения, на которое ответили.
    let id = dune["id"].as_str().unwrap();
    let detail = ctx.get_ok(&format!("/threads/{id}"), None).await;
    let posts = items(&detail["posts"]);
    assert_eq!(detail["posts"]["total"], 6);
    assert!(posts
        .iter()
        .any(|p| p["deleted"] == true && p["author"].is_null()));
    assert!(posts.iter().any(|p| p["reply_to"]["username"] == "author"));
    assert_eq!(detail["last_post_at"], posts.last().unwrap()["created_at"]);
    assert!(items(&threads).iter().any(|t| t["is_locked"] == true));

    let profile = ctx.get_ok("/users/author", None).await;
    assert_eq!(profile["threads_count"], 3);
    assert_eq!(profile["posts_count"], 2);

    // Интересы user.
    let interests = ctx.get_ok("/interests", Some(&user)).await;
    assert_eq!(interests["total"], 3, "{interests}");
}
