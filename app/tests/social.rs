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
            assert!(body["code"].is_string(), "{method} {path}: {body}");
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
    assert_eq!(created["entity"]["release_date"], "2021-10-22");
    assert_eq!(created["is_spoiler"], false);

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
async fn review_spoiler_flag(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;

    let review = ctx
        .review(
            &alice,
            "dune-2021",
            json!({ "rating": 8, "body": "Пол выживает.", "is_spoiler": true }),
        )
        .await;
    assert_eq!(review["is_spoiler"], true);
    let page = ctx.get_ok("/entities/dune-2021/reviews", None).await;
    assert_eq!(items(&page)[0]["is_spoiler"], true);

    // Замена целиком: старый клиент без поля снимает отметку.
    let review = ctx
        .review(
            &alice,
            "dune-2021",
            json!({ "rating": 8, "body": "Красиво." }),
        )
        .await;
    assert_eq!(review["is_spoiler"], false);

    let response = ctx
        .send(
            Method::PUT,
            "/entities/dune-2021/review",
            Some(json!({ "rating": 8, "is_spoiler": "yes" })),
            Some(&alice),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

/// Сущность с датой выхода через `days` дней от сегодняшней (UTC) даты БД.
async fn entity_released_in(pool: &PgPool, slug: &str, days: i32) {
    sqlx::query(
        "INSERT INTO entities (kind, slug, title, release_date)
         VALUES ('movie', $1, 'Скоро', (now() AT TIME ZONE 'UTC')::date + $2)",
    )
    .bind(slug)
    .bind(days)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn reviews_open_after_release(pool: PgPool) {
    entity_released_in(&pool, "next-year", 365).await;
    entity_released_in(&pool, "today", 0).await;
    let ctx = Ctx::new(pool.clone());
    let alice = ctx.user("alice").await;

    for body in [json!({ "rating": 9 }), json!({ "body": "Жду!" })] {
        let error = ctx
            .expect(
                StatusCode::CONFLICT,
                Method::PUT,
                "/entities/next-year/review",
                Some(body),
                Some(&alice),
            )
            .await;
        assert_eq!(error["code"], "not_released");
    }
    // Вышла сегодня или дата неизвестна — можно.
    ctx.review(&alice, "today", json!({ "rating": 9 })).await;
    ctx.review(&alice, "no-date", json!({ "rating": 9 })).await;

    // Рецензию, оставленную до переноса премьеры, удалить можно всегда.
    sqlx::query(
        "INSERT INTO reviews (user_id, entity_id, rating)
         SELECT id, (SELECT id FROM entities WHERE slug = 'next-year'), 5
         FROM users WHERE username = $1",
    )
    .bind(&alice.username)
    .execute(&pool)
    .await
    .unwrap();
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/entities/next-year/review",
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
        json!({
            "average": null, "count": 0, "distribution": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            "reactions": { "fire": 0, "sad": 0, "think": 0, "bored": 0 },
        })
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
        json!({
            "average": 8.7, "count": 3, "distribution": [0, 0, 0, 0, 0, 0, 0, 2, 0, 1],
            "reactions": { "fire": 0, "sad": 0, "think": 0, "bored": 0 },
        })
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

// ---------------------------------------------------------------- оценки в каталоге

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn ratings_in_batch(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let (alice, bob) = (ctx.user("alice").await, ctx.user("bob").await);
    ctx.review(&alice, "dune-2021", json!({ "rating": 9 }))
        .await;
    ctx.review(&bob, "dune-2021", json!({ "rating": 8 })).await;
    ctx.review(&alice, "witcher-3", json!({ "body": "Без оценки" }))
        .await;

    let ratings = ctx
        .get_ok(
            "/ratings?slugs=dune-2021,witcher-3,%20dune-2021,no-such",
            None,
        )
        .await;
    assert_eq!(
        ratings,
        json!({
            "dune-2021": { "average": 8.5, "count": 2 },
            "witcher-3": { "average": null, "count": 0 },
        })
    );

    let many = (0..101)
        .map(|i| format!("s{i}"))
        .collect::<Vec<_>>()
        .join(",");
    for query in [
        "/ratings?slugs=",
        "/ratings?slugs=,,",
        "/ratings",
        &format!("/ratings?slugs={many}"),
    ] {
        ctx.expect(StatusCode::BAD_REQUEST, Method::GET, query, None, None)
            .await;
    }
}

/// slug'и страницы каталога `GET /catalog/entities?...`.
async fn catalog_slugs(ctx: &Ctx, query: &str) -> Vec<String> {
    let app = nexus::build_app(ctx.state.clone());
    let response = test_utils::get(app, &format!("/api/v1/catalog/entities?{query}")).await;
    assert_eq!(response.status, StatusCode::OK, "{query}");
    response.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slug"].as_str().unwrap().to_string())
        .collect()
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn catalog_sorts_by_ratings_from_social(pool: PgPool) {
    let ctx = Ctx::new(pool.clone());
    let users = [
        ctx.user("alice").await,
        ctx.user("bob").await,
        ctx.user("carol").await,
    ];
    for (user, slug, rating) in [
        (0, "dune-novel", 6),
        (1, "dune-novel", 6),
        (2, "dune-novel", 7),
        (0, "dune-2021", 9),
        (1, "dune-2021", 7),
        (0, "witcher-3", 10),
    ] {
        ctx.review(&users[user], slug, json!({ "rating": rating }))
            .await;
    }
    // Текст без оценки не считается.
    ctx.review(&users[2], "no-date", json!({ "body": "Ещё не смотрел" }))
        .await;

    assert_eq!(
        catalog_slugs(&ctx, "sort=popular").await,
        [
            "dune-novel",
            "dune-2021",
            "witcher-3",
            "dune-part-two-2024",
            "no-date"
        ]
    );
    assert_eq!(
        catalog_slugs(&ctx, "sort=rating").await,
        [
            "witcher-3",
            "dune-2021",
            "dune-novel",
            "dune-part-two-2024",
            "no-date"
        ]
    );
    assert_eq!(
        catalog_slugs(&ctx, "sort=title&kind=movie").await,
        ["dune-2021", "dune-part-two-2024"]
    );
    assert_eq!(
        catalog_slugs(&ctx, "sort=new&limit=2").await,
        ["dune-part-two-2024", "dune-2021"]
    );

    // Удалена единственная оценка — сущность без оценок, в конце.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/entities/witcher-3/review",
        None,
        Some(&users[0]),
    )
    .await;
    assert_eq!(
        catalog_slugs(&ctx, "sort=rating&limit=3").await,
        ["dune-2021", "dune-novel", "dune-part-two-2024"]
    );

    // Записанное в обход API догоняет полная синхронизация при старте.
    sqlx::query(
        "INSERT INTO reviews (user_id, entity_id, rating)
         SELECT u.id, e.id, 10 FROM users u, entities e
         WHERE u.username = $1 AND e.slug = 'dune-part-two-2024'",
    )
    .bind(&users[2].username)
    .execute(&pool)
    .await
    .unwrap();
    social::ratings::sync_all(&ctx.state).await.unwrap();
    assert_eq!(
        catalog_slugs(&ctx, "sort=rating&limit=1").await,
        ["dune-part-two-2024"]
    );

    let app = nexus::build_app(ctx.state.clone());
    let response = test_utils::get(app, "/api/v1/catalog/entities?sort=best").await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------- голоса за рецензии

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn review_votes_count_and_change(pool: PgPool) {
    let ctx = Ctx::new(pool.clone());
    let (alice, bob, carol) = (
        ctx.user("alice").await,
        ctx.user("bob").await,
        ctx.user("carol").await,
    );
    let review = ctx
        .review(&alice, "dune-2021", json!({ "rating": 9, "body": "Да" }))
        .await;
    assert_eq!(review["likes_count"], 0);
    assert_eq!(review["dislikes_count"], 0);
    let id = review["id"].as_str().unwrap();
    let vote = format!("/reviews/{id}/vote");
    let put = |user, value: i32| {
        ctx.expect(
            StatusCode::OK,
            Method::PUT,
            &vote,
            Some(json!({ "value": value })),
            Some(user),
        )
    };

    let result = put(&bob, 1).await;
    assert_eq!(
        result,
        json!({ "likes_count": 1, "dislikes_count": 0, "value": 1 })
    );
    // Повтор того же голоса ничего не меняет, другой — переносит голос.
    assert_eq!(put(&bob, 1).await["likes_count"], 1);
    let result = put(&bob, -1).await;
    assert_eq!(
        result,
        json!({ "likes_count": 0, "dislikes_count": 1, "value": -1 })
    );
    let result = put(&carol, 1).await;
    assert_eq!(
        result,
        json!({ "likes_count": 1, "dislikes_count": 1, "value": 1 })
    );

    // Счётчики — в публичном списке, свой голос — отдельно.
    let page = ctx.get_ok("/entities/dune-2021/reviews", None).await;
    assert_eq!(items(&page)[0]["likes_count"], 1);
    assert_eq!(items(&page)[0]["dislikes_count"], 1);
    assert!(items(&page)[0].get("my_vote").is_none());
    let mine = ctx
        .get_ok("/entities/dune-2021/votes/mine", Some(&bob))
        .await;
    assert_eq!(mine, json!({ id: -1 }));
    let mine = ctx
        .get_ok("/entities/witcher-3/votes/mine", Some(&bob))
        .await;
    assert_eq!(mine, json!({}));

    // Голоса не делают рецензию «изменённой».
    let own = ctx.get_ok("/entities/dune-2021/review", Some(&alice)).await;
    assert_eq!(own["updated_at"], review["updated_at"]);

    let result = ctx
        .expect(StatusCode::OK, Method::DELETE, &vote, None, Some(&bob))
        .await;
    assert_eq!(
        result,
        json!({ "likes_count": 1, "dislikes_count": 0, "value": null })
    );
    // Голоса нет — тоже 200.
    let again = ctx
        .expect(StatusCode::OK, Method::DELETE, &vote, None, Some(&bob))
        .await;
    assert_eq!(again, result);

    // Удалена рецензия — голоса уходят вместе с ней.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/entities/dune-2021/review",
        None,
        Some(&alice),
    )
    .await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM review_votes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn review_votes_errors(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let (alice, bob) = (ctx.user("alice").await, ctx.user("bob").await);
    let review = ctx
        .review(&alice, "dune-2021", json!({ "rating": 9 }))
        .await;
    let vote = format!("/reviews/{}/vote", review["id"].as_str().unwrap());

    let error = ctx
        .expect(
            StatusCode::FORBIDDEN,
            Method::PUT,
            &vote,
            Some(json!({ "value": 1 })),
            Some(&alice),
        )
        .await;
    assert_eq!(error["code"], "own_review");
    for body in [
        json!({ "value": 0 }),
        json!({ "value": 2 }),
        json!({}),
        json!({ "value": "up" }),
    ] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            &vote,
            Some(body),
            Some(&bob),
        )
        .await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PUT,
        &vote,
        Some(json!({ "value": 1 })),
        None,
    )
    .await;
    ctx.expect(StatusCode::UNAUTHORIZED, Method::DELETE, &vote, None, None)
        .await;
    let missing = format!("/reviews/{}/vote", Uuid::new_v4());
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PUT,
        &missing,
        Some(json!({ "value": 1 })),
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &missing,
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::GET,
        "/entities/dune-2021/votes/mine",
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/no-such/votes/mine",
        None,
        Some(&bob),
    )
    .await;
}

// ---------------------------------------------------------------- комментарии к рецензиям

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn review_comments_flow(pool: PgPool) {
    use shared::events::Channel;

    let ctx = Ctx::new(pool.clone());
    let (alice, bob, carol) = (
        ctx.user("alice").await,
        ctx.user("bob").await,
        ctx.user("carol").await,
    );
    let admin = ctx.login("admin@example.com", "admin").await;
    let review = ctx
        .review(
            &alice,
            "dune-2021",
            json!({ "rating": 9, "body": "Красиво" }),
        )
        .await;
    assert_eq!(review["comments_count"], 0);
    let path = format!("/reviews/{}/comments", review["id"].as_str().unwrap());
    let mut events = ctx.state.events.subscribe();

    let first = ctx
        .expect(
            StatusCode::CREATED,
            Method::POST,
            &path,
            Some(json!({ "body": "  Согласен  " })),
            Some(&bob),
        )
        .await;
    assert_eq!(first["body"], "Согласен");
    assert_eq!(first["author"]["username"], bob.username.as_str());
    assert_eq!(first["review_id"], review["id"]);
    let event = events.try_recv().expect("review.commented");
    assert_eq!(event.kind, "review.commented");
    assert!(matches!(event.channels[0], Channel::Entity(_)));
    assert_eq!(event.data["comment_id"], first["id"]);
    assert_eq!(event.data["review_id"], review["id"]);

    // Автор рецензии тоже может ответить.
    for (user, text) in [(&carol, "А мне скучно"), (&alice, "Спасибо")] {
        ctx.expect(
            StatusCode::CREATED,
            Method::POST,
            &path,
            Some(json!({ "body": text })),
            Some(user),
        )
        .await;
    }

    // Публично, старые сверху; счётчик — в рецензии.
    let page = ctx.get_ok(&path, None).await;
    assert_eq!(page["total"], 3);
    assert_eq!(
        pluck(items(&page), "body"),
        ["Согласен", "А мне скучно", "Спасибо"]
    );
    let page = ctx.get_ok(&format!("{path}?limit=1&offset=1"), None).await;
    assert_eq!(pluck(items(&page), "body"), ["А мне скучно"]);
    let reviews = ctx.get_ok("/entities/dune-2021/reviews", None).await;
    assert_eq!(items(&reviews)[0]["comments_count"], 3);
    // Комментарии не делают рецензию «изменённой».
    assert_eq!(items(&reviews)[0]["updated_at"], review["updated_at"]);

    // Удалить свой может автор, чужой — нет; admin — любой.
    let delete = |id: &Value| format!("/review-comments/{}", id.as_str().unwrap());
    let carols = &items(&ctx.get_ok(&path, None).await)[1].clone();
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &delete(&carols["id"]),
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &delete(&first["id"]),
        None,
        Some(&bob),
    )
    .await;
    while let Ok(event) = events.try_recv() {
        if event.kind == "review.comment_deleted" {
            assert_eq!(event.data["comment_id"], first["id"]);
        }
    }
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &delete(&first["id"]),
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &format!("/admin/review-comments/{}", carols["id"].as_str().unwrap()),
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/admin/review-comments/{}", carols["id"].as_str().unwrap()),
        None,
        Some(&admin),
    )
    .await;
    let page = ctx.get_ok(&path, None).await;
    assert_eq!(page["total"], 1);
    assert_eq!(pluck(items(&page), "body"), ["Спасибо"]);

    // Удалена рецензия — комментарии вместе с ней.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/entities/dune-2021/review",
        None,
        Some(&alice),
    )
    .await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM review_comments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    ctx.expect(StatusCode::NOT_FOUND, Method::GET, &path, None, None)
        .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn review_comments_errors(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let (alice, bob) = (ctx.user("alice").await, ctx.user("bob").await);
    let review = ctx
        .review(&alice, "dune-2021", json!({ "rating": 9, "body": "Да" }))
        .await;
    let path = format!("/reviews/{}/comments", review["id"].as_str().unwrap());

    for body in [
        json!({ "body": "   " }),
        json!({ "body": "ы".repeat(2001) }),
        json!({}),
        json!({ "body": "x", "extra": 1 }),
    ] {
        let error = ctx
            .expect(
                StatusCode::BAD_REQUEST,
                Method::POST,
                &path,
                Some(body),
                Some(&bob),
            )
            .await;
        assert_eq!(error["code"], "validation");
    }
    ctx.expect(
        StatusCode::CREATED,
        Method::POST,
        &path,
        Some(json!({ "body": "ы".repeat(2000) })),
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        &path,
        Some(json!({ "body": "x" })),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::DELETE,
        &format!("/review-comments/{}", Uuid::new_v4()),
        None,
        None,
    )
    .await;
    let missing = format!("/reviews/{}/comments", Uuid::new_v4());
    ctx.expect(StatusCode::NOT_FOUND, Method::GET, &missing, None, None)
        .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::POST,
        &missing,
        Some(json!({ "body": "x" })),
        Some(&bob),
    )
    .await;

    // Оценку без текста не комментируют.
    let rating = ctx.review(&bob, "witcher-3", json!({ "rating": 3 })).await;
    let error = ctx
        .expect(
            StatusCode::CONFLICT,
            Method::POST,
            &format!("/reviews/{}/comments", rating["id"].as_str().unwrap()),
            Some(json!({ "body": "Почему 3?" })),
            Some(&alice),
        )
        .await;
    assert_eq!(error["code"], "review_without_text");
}

// ---------------------------------------------------------------- реакции

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn reactions_count_one_per_viewer(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let (alice, bob, carol) = (
        ctx.user("alice").await,
        ctx.user("bob").await,
        ctx.user("carol").await,
    );
    let path = "/entities/dune-2021/reaction";
    let react = |user, kind: &str| {
        ctx.expect(
            StatusCode::OK,
            Method::PUT,
            path,
            Some(json!({ "kind": kind })),
            Some(user),
        )
    };

    ctx.expect(StatusCode::NOT_FOUND, Method::GET, path, None, Some(&alice))
        .await;
    assert_eq!(react(&alice, "fire").await, json!({ "kind": "fire" }));
    react(&bob, "fire").await;
    react(&carol, "sad").await;
    // Другая реакция заменяет прежнюю: у зрителя одна.
    react(&carol, "think").await;
    assert_eq!(
        ctx.get_ok(path, Some(&carol)).await,
        json!({ "kind": "think" })
    );
    let summary = ctx.get_ok("/entities/dune-2021/rating", None).await;
    assert_eq!(
        summary["reactions"],
        json!({ "fire": 2, "sad": 0, "think": 1, "bored": 0 })
    );

    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        path,
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        path,
        None,
        Some(&bob),
    )
    .await;
    let summary = ctx.get_ok("/entities/dune-2021/rating", None).await;
    assert_eq!(summary["reactions"]["fire"], 1);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn reactions_errors(pool: PgPool) {
    entity_released_in(&pool, "next-year", 365).await;
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let path = "/entities/dune-2021/reaction";

    for body in [json!({ "kind": "love" }), json!({}), json!({ "kind": 1 })] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            path,
            Some(body),
            Some(&alice),
        )
        .await;
    }
    for method in [Method::GET, Method::PUT, Method::DELETE] {
        let body = (method == Method::PUT).then(|| json!({ "kind": "fire" }));
        ctx.expect(
            StatusCode::UNAUTHORIZED,
            method.clone(),
            path,
            body.clone(),
            None,
        )
        .await;
        ctx.expect(
            StatusCode::NOT_FOUND,
            method,
            "/entities/no-such/reaction",
            body,
            Some(&alice),
        )
        .await;
    }
    let error = ctx
        .expect(
            StatusCode::CONFLICT,
            Method::PUT,
            "/entities/next-year/reaction",
            Some(json!({ "kind": "fire" })),
            Some(&alice),
        )
        .await;
    assert_eq!(error["code"], "not_released");
}

// ---------------------------------------------------------------- отметки

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn marks_are_exclusive_and_private(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let (alice, bob) = (ctx.user("alice").await, ctx.user("bob").await);
    let ctx = &ctx;
    let mark = |user, slug: &str, kind: &str| {
        let path = format!("/entities/{slug}/mark");
        let body = json!({ "kind": kind });
        async move {
            ctx.expect(StatusCode::OK, Method::PUT, &path, Some(body), Some(user))
                .await
        }
    };

    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/entities/dune-2021/mark",
        None,
        Some(&alice),
    )
    .await;
    assert_eq!(
        mark(&alice, "dune-2021", "later").await,
        json!({ "kind": "later" })
    );
    mark(&alice, "dune-novel", "later").await;
    mark(&alice, "witcher-3", "done").await;
    mark(&bob, "dune-2021", "done").await;

    let later = ctx.get_ok("/users/me/marks?kind=later", Some(&alice)).await;
    assert_eq!(later["total"], 2);
    assert_eq!(
        pluck(items(&later), "entity/slug"),
        ["dune-novel", "dune-2021"]
    );
    assert_eq!(items(&later)[0]["kind"], "later");
    assert_eq!(items(&later)[0]["entity"]["release_date"], "1965-08-01");

    // «Просмотрено» убирает из «позже» и встаёт сверху своего списка.
    mark(&alice, "dune-2021", "done").await;
    assert_eq!(
        ctx.get_ok("/entities/dune-2021/mark", Some(&alice)).await,
        json!({ "kind": "done" })
    );
    let later = ctx.get_ok("/users/me/marks?kind=later", Some(&alice)).await;
    assert_eq!(pluck(items(&later), "entity/slug"), ["dune-novel"]);
    let done = ctx.get_ok("/users/me/marks?kind=done", Some(&alice)).await;
    assert_eq!(
        pluck(items(&done), "entity/slug"),
        ["dune-2021", "witcher-3"]
    );
    let all = ctx.get_ok("/users/me/marks?limit=2", Some(&alice)).await;
    assert_eq!(all["total"], 3);
    assert_eq!(items(&all).len(), 2);

    // У bob свои отметки, alice их не видит.
    let bobs = ctx.get_ok("/users/me/marks", Some(&bob)).await;
    assert_eq!(pluck(items(&bobs), "entity/slug"), ["dune-2021"]);

    for _ in 0..2 {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::DELETE,
            "/entities/dune-2021/mark",
            None,
            Some(&alice),
        )
        .await;
    }
    let done = ctx.get_ok("/users/me/marks?kind=done", Some(&alice)).await;
    assert_eq!(pluck(items(&done), "entity/slug"), ["witcher-3"]);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn marks_errors(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let path = "/entities/dune-2021/mark";

    for body in [json!({ "kind": "watching" }), json!({})] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            path,
            Some(body),
            Some(&alice),
        )
        .await;
    }
    for method in [Method::GET, Method::PUT, Method::DELETE] {
        let body = (method == Method::PUT).then(|| json!({ "kind": "later" }));
        ctx.expect(
            StatusCode::UNAUTHORIZED,
            method.clone(),
            path,
            body.clone(),
            None,
        )
        .await;
        ctx.expect(
            StatusCode::NOT_FOUND,
            method,
            "/entities/no-such/mark",
            body,
            Some(&alice),
        )
        .await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::GET,
        "/users/me/marks",
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/users/me/marks?kind=soon",
        None,
        Some(&alice),
    )
    .await;
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
    assert_eq!(first["entity"]["release_date"], "1965-08-01");
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

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn collections_have_preview_and_contains(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let alice = ctx.user("alice").await;
    let full = ctx
        .collection(&alice, json!({ "title": "Всё", "is_public": false }))
        .await;
    for slug in [
        "dune-novel",
        "dune-2021",
        "witcher-3",
        "no-date",
        "dune-part-two-2024",
    ] {
        ctx.add_item(&alice, &full, slug).await;
    }
    let empty = ctx.collection(&alice, json!({ "title": "Пусто" })).await;

    // Первые 4 по порядку — в списках и карточке.
    let list_path = format!("/users/{}/collections", alice.username);
    let page = ctx.get_ok(&list_path, Some(&alice)).await;
    let find = |page: &Value, id: &str| {
        items(page)
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {page}"))
            .clone()
    };
    let collection = find(&page, &full);
    assert_eq!(
        pluck(collection["preview"].as_array().unwrap(), "slug"),
        ["dune-novel", "dune-2021", "witcher-3", "no-date"]
    );
    assert_eq!(collection["preview"][0]["release_date"], "1965-08-01");
    assert!(collection.get("contains").is_none());
    assert_eq!(find(&page, &empty)["preview"], json!([]));

    ctx.expect(
        StatusCode::OK,
        Method::PUT,
        &format!("/collections/{full}/items"),
        Some(json!({ "entities": ["dune-part-two-2024", "dune-novel", "dune-2021", "witcher-3", "no-date"] })),
        Some(&alice),
    )
    .await;
    let detail = ctx
        .get_ok(&format!("/collections/{full}"), Some(&alice))
        .await;
    assert_eq!(detail["preview"][0]["slug"], "dune-part-two-2024");

    // ?contains= — отметки для кнопки «В коллекцию», в том числе у приватной (для владельца).
    let page = ctx
        .get_ok(&format!("{list_path}?contains=witcher-3"), Some(&alice))
        .await;
    assert_eq!(find(&page, &full)["contains"], true);
    assert_eq!(find(&page, &empty)["contains"], false);
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        &format!("{list_path}?contains=no-such"),
        None,
        Some(&alice),
    )
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

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_features_collections(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login("admin@example.com", "admin").await;
    let alice = ctx.user("alice").await;
    let top = ctx.collection(&admin, json!({ "title": "Топ-10" })).await;
    let best = ctx.collection(&alice, json!({ "title": "Лучшее" })).await;
    let private = ctx
        .collection(&alice, json!({ "title": "Позже", "is_public": false }))
        .await;
    let featured = |id: &str| format!("/admin/collections/{id}/featured");

    assert_eq!(ctx.get_ok("/collections/featured", None).await, json!([]));

    // Только admin.
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PUT,
        &featured(&top),
        None,
        None,
    )
    .await;
    for method in [Method::PUT, Method::DELETE] {
        ctx.expect(
            StatusCode::FORBIDDEN,
            method,
            &featured(&top),
            None,
            Some(&alice),
        )
        .await;
    }
    // Приватная не может быть особой; несуществующей — 404.
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PUT,
        &featured(&private),
        None,
        Some(&admin),
    )
    .await;
    let missing = featured("00000000-0000-0000-0000-000000000000");
    for method in [Method::PUT, Method::DELETE] {
        ctx.expect(StatusCode::NOT_FOUND, method, &missing, None, Some(&admin))
            .await;
    }

    // Без тела — в конец; `position` задаёт порядок.
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured(&best),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(got["is_featured"], true);
    assert_eq!(got["featured_position"], 0);
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured(&top),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(got["featured_position"], 1);
    ctx.expect(
        StatusCode::OK,
        Method::PUT,
        &featured(&top),
        Some(json!({ "position": -1 })),
        Some(&admin),
    )
    .await;
    let list = ctx.get_ok("/collections/featured", None).await;
    assert_eq!(
        pluck(list.as_array().unwrap(), "title"),
        ["Топ-10", "Лучшее"]
    );
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["is_featured"], true);

    // Владелец не может спрятать особую.
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PATCH,
        &format!("/collections/{best}"),
        Some(json!({ "is_public": false })),
        Some(&alice),
    )
    .await;

    // Снять отметку; повторно — тоже 204.
    for _ in 0..2 {
        ctx.expect(
            StatusCode::NO_CONTENT,
            Method::DELETE,
            &featured(&top),
            None,
            Some(&admin),
        )
        .await;
    }
    let list = ctx.get_ok("/collections/featured", None).await;
    assert_eq!(pluck(list.as_array().unwrap(), "title"), ["Лучшее"]);
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["is_featured"], false);
    assert_eq!(got["featured_position"], Value::Null);
}

/// Читаемый адрес особой коллекции: транслит названия при первой пометке, `-2` при коллизии,
/// свой slug (400 на формат, UUID, служебное слово; 409 — занят), повторный PUT не меняет
/// адрес, смена и снятие из особых — в историю, `by-slug` отдаёт коллекцию или куда
/// перенаправить, удалённая коллекция держит адрес навсегда.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn featured_collection_slugs(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login("admin@example.com", "admin").await;
    let alice = ctx.user("alice").await;
    let top = ctx
        .collection(&admin, json!({ "title": "Русский топ 2021" }))
        .await;
    let twin = ctx
        .collection(&alice, json!({ "title": "Русский  топ 2021!" }))
        .await;
    let other = ctx.collection(&alice, json!({ "title": "Другая" })).await;
    let featured = |id: &str| format!("/admin/collections/{id}/featured");
    let put = |id: String, body: Option<Value>| {
        let ctx = &ctx;
        let admin = &admin;
        async move {
            ctx.expect(
                StatusCode::OK,
                Method::PUT,
                &featured(&id),
                body,
                Some(admin),
            )
            .await
        }
    };

    // Обычная коллекция без адреса.
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["slug"], Value::Null);

    // Транслит; коллизия — `-2`.
    assert_eq!(put(top.clone(), None).await["slug"], "russkiy-top-2021");
    assert_eq!(put(twin.clone(), None).await["slug"], "russkiy-top-2021-2");
    // Повторный PUT (порядок) адрес не меняет.
    let got = put(top.clone(), Some(json!({ "position": 5 }))).await;
    assert_eq!(got["slug"], "russkiy-top-2021");
    assert_eq!(got["featured_position"], 5);

    // Неверный slug — 400; занятый — 409.
    for bad in [
        "ab",
        "Top",
        "a--b",
        "featured",
        "by-slug",
        "0f8fad5b-d9cb-469f-a165-70867728950e",
    ] {
        let error = ctx
            .expect(
                StatusCode::BAD_REQUEST,
                Method::PUT,
                &featured(&other),
                Some(json!({ "slug": bad })),
                Some(&admin),
            )
            .await;
        assert_eq!(error["code"], "validation", "{bad}");
    }
    let error = ctx
        .expect(
            StatusCode::CONFLICT,
            Method::PUT,
            &featured(&other),
            Some(json!({ "slug": "russkiy-top-2021" })),
            Some(&admin),
        )
        .await;
    assert_eq!(error["code"], "conflict");

    // По адресу — коллекция целиком.
    let by_slug = ctx
        .get_ok("/collections/by-slug/russkiy-top-2021", None)
        .await;
    assert_eq!(by_slug["id"], top.as_str());
    assert!(by_slug["items"].is_array());
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/collections/by-slug/nope",
        None,
        None,
    )
    .await;

    // Смена адреса: старый — в историю, ведёт на новый; чужой коллекции не достаётся.
    let got = put(top.clone(), Some(json!({ "slug": "russian-top-2021" }))).await;
    assert_eq!(got["slug"], "russian-top-2021");
    assert_eq!(
        ctx.get_ok("/collections/by-slug/russkiy-top-2021", None)
            .await,
        json!({ "redirect_slug": "russian-top-2021" })
    );
    ctx.expect(
        StatusCode::CONFLICT,
        Method::PUT,
        &featured(&other),
        Some(json!({ "slug": "russkiy-top-2021" })),
        Some(&admin),
    )
    .await;
    // Свой бывший адрес вернуть можно.
    let got = put(top.clone(), Some(json!({ "slug": "russkiy-top-2021" }))).await;
    assert_eq!(got["slug"], "russkiy-top-2021");
    assert_eq!(
        ctx.get_ok("/collections/by-slug/russian-top-2021", None)
            .await,
        json!({ "redirect_slug": "russkiy-top-2021" })
    );

    // Снята из особых: адрес — в историю, ведёт на uuid-адрес.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &featured(&top),
        None,
        Some(&admin),
    )
    .await;
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["slug"], Value::Null);
    assert_eq!(
        ctx.get_ok("/collections/by-slug/russkiy-top-2021", None)
            .await,
        json!({ "redirect_id": top })
    );
    ctx.expect(
        StatusCode::CONFLICT,
        Method::PUT,
        &featured(&other),
        Some(json!({ "slug": "russkiy-top-2021" })),
        Some(&admin),
    )
    .await;
    // Снова особая без slug — транслит, свой бывший адрес свободен для неё.
    assert_eq!(put(top.clone(), None).await["slug"], "russkiy-top-2021");

    // Удалена: адрес занят навсегда, по нему 404.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/collections/{twin}"),
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/collections/by-slug/russkiy-top-2021-2",
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::CONFLICT,
        Method::PUT,
        &featured(&other),
        Some(json!({ "slug": "russkiy-top-2021-2" })),
        Some(&admin),
    )
    .await;

    // В списке особых — со slug.
    let list = ctx.get_ok("/collections/featured", None).await;
    assert_eq!(list[0]["slug"], "russkiy-top-2021");
}

/// Оформление особой коллекции (обои, обложка, тема): задаётся в `PUT .../featured`, отсутствующее
/// поле не меняется, `null` сбрасывает, неверное — 400 с `field`; снятие из особых сбрасывает всё;
/// приватная — `collection_private`. Описания сущностей в детали коллекции. Админ меняет состав
/// любой особой коллекции, обычной — 400 `not_featured`.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn featured_collection_style_overviews_and_admin_items(pool: PgPool) {
    let ctx = Ctx::new(pool.clone());
    let admin = ctx.login("admin@example.com", "admin").await;
    let alice = ctx.user("alice").await;
    let top = ctx.collection(&alice, json!({ "title": "Хеллоуин" })).await;
    let plain = ctx.collection(&alice, json!({ "title": "Обычная" })).await;
    let private = ctx
        .collection(&alice, json!({ "title": "Тайная", "is_public": false }))
        .await;
    let featured = format!("/admin/collections/{top}/featured");

    let got = ctx.get_ok(&format!("/collections/{plain}"), None).await;
    for field in ["wallpaper_url", "cover_url", "theme"] {
        assert_eq!(got[field], Value::Null, "{field}");
    }

    let style = json!({
        "wallpaper_url": "https://cdn.example.com/wall.jpg",
        "cover_url": "https://cdn.example.com/cover.jpg",
        "theme": "halloween",
    });
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(style),
            Some(&admin),
        )
        .await;
    assert_eq!(got["wallpaper_url"], "https://cdn.example.com/wall.jpg");
    assert_eq!(got["cover_url"], "https://cdn.example.com/cover.jpg");
    assert_eq!(got["theme"], "halloween");
    // Не передано — не меняется; null — сброс.
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({ "cover_url": null, "position": 2 })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["cover_url"], Value::Null);
    assert_eq!(got["theme"], "halloween");
    assert_eq!(got["wallpaper_url"], "https://cdn.example.com/wall.jpg");
    let list = ctx.get_ok("/collections/featured", None).await;
    assert_eq!(list[0]["theme"], "halloween");

    for (body, field) in [
        (
            json!({ "wallpaper_url": "http://cdn.example.com/a.jpg" }),
            "wallpaper_url",
        ),
        (
            json!({ "cover_url": format!("https://x/{}", "a".repeat(2048)) }),
            "cover_url",
        ),
        (json!({ "theme": "Halloween" }), "theme"),
        (json!({ "theme": "a".repeat(33) }), "theme"),
        (json!({ "slug": "ab" }), "slug"),
    ] {
        let error = ctx
            .expect(
                StatusCode::BAD_REQUEST,
                Method::PUT,
                &featured,
                Some(body),
                Some(&admin),
            )
            .await;
        assert_eq!(error["code"], "validation");
        assert_eq!(error["field"], field);
    }
    let error = ctx
        .expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            &format!("/admin/collections/{private}/featured"),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(error["code"], "collection_private");

    // Описания: одним запросом для всех элементов, без HTML; нет описания — null.
    sqlx::query(
        "UPDATE entities SET description = '<p>Пол Атрейдес  едет на Арракис.</p>'
         WHERE slug = 'dune-2021'",
    )
    .execute(&pool)
    .await
    .unwrap();
    ctx.add_item(&alice, &top, "dune-2021").await;
    ctx.add_item(&alice, &top, "dune-part-two-2024").await;
    let detail = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(detail["items"][0]["entity"]["slug"], "dune-2021");
    assert_eq!(
        detail["items"][0]["entity"]["overview"],
        "Пол Атрейдес едет на Арракис."
    );
    assert_eq!(detail["items"][1]["entity"]["overview"], Value::Null);

    // Админ правит состав особой; не admin — 403; обычной — 400 not_featured.
    let item = |id: &str, slug: &str| format!("/admin/collections/{id}/items/{slug}");
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::PUT,
        &item(&top, "dune-novel"),
        None,
        Some(&alice),
    )
    .await;
    let added = ctx
        .expect(
            StatusCode::CREATED,
            Method::PUT,
            &item(&top, "dune-novel"),
            Some(json!({ "note": "Книга", "position": 0 })),
            Some(&admin),
        )
        .await;
    assert_eq!(added["entity"]["slug"], "dune-novel");
    assert_eq!(added["note"], "Книга");
    ctx.expect(
        StatusCode::OK,
        Method::PUT,
        &item(&top, "dune-novel"),
        Some(json!({ "position": 9 })),
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &item(&top, "dune-2021"),
        None,
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &item(&top, "dune-2021"),
        None,
        Some(&admin),
    )
    .await;
    let detail = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(pluck_items(&detail), ["dune-part-two-2024", "dune-novel"]);
    let error = ctx
        .expect(
            StatusCode::BAD_REQUEST,
            Method::PUT,
            &item(&plain, "dune-novel"),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(error["code"], "not_featured");
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &item("00000000-0000-0000-0000-000000000000", "dune-novel"),
        None,
        Some(&admin),
    )
    .await;

    // Снята из особых — оформление сброшено.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &featured,
        None,
        Some(&admin),
    )
    .await;
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["wallpaper_url"], Value::Null);
    assert_eq!(got["theme"], Value::Null);
}

/// Оформление страницы особой коллекции: значения по умолчанию, установка, частичное изменение,
/// `null` — значение по умолчанию, ошибки с `field` (в том числе неверный тип), сброс при снятии
/// из особых.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn featured_collection_page_style(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login("admin@example.com", "admin").await;
    let alice = ctx.user("alice").await;
    let top = ctx.collection(&alice, json!({ "title": "Ужасы" })).await;
    let featured = format!("/admin/collections/{top}/featured");
    let defaults = |got: &Value| {
        assert_eq!(got["wallpaper_mode"], "header");
        assert_eq!(got["wallpaper_dim"], Value::Null);
        assert_eq!(got["wallpaper_blur"], 0);
        assert_eq!(got["effects"], true);
        assert_eq!(got["accent_color"], Value::Null);
    };
    defaults(&ctx.get_ok(&format!("/collections/{top}"), None).await);

    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({
                "theme": "horror",
                "wallpaper_mode": "page",
                "wallpaper_dim": 60,
                "wallpaper_blur": 4,
                "effects": false,
                "accent_color": "#B3001B",
            })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["wallpaper_mode"], "page");
    assert_eq!(got["wallpaper_dim"], 60);
    assert_eq!(got["wallpaper_blur"], 4);
    assert_eq!(got["effects"], false);
    assert_eq!(got["accent_color"], "#b3001b");

    // Не передано — не меняется; null — по умолчанию.
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({ "wallpaper_dim": null, "effects": null, "wallpaper_blur": 20 })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["wallpaper_dim"], Value::Null);
    assert_eq!(got["effects"], true);
    assert_eq!(got["wallpaper_blur"], 20);
    assert_eq!(got["wallpaper_mode"], "page");
    assert_eq!(got["accent_color"], "#b3001b");
    let list = ctx.get_ok("/collections/featured", None).await;
    assert_eq!(list[0]["wallpaper_mode"], "page");

    for (body, field) in [
        (json!({ "wallpaper_mode": "full" }), "wallpaper_mode"),
        (json!({ "wallpaper_mode": 1 }), "wallpaper_mode"),
        (json!({ "wallpaper_dim": 101 }), "wallpaper_dim"),
        (json!({ "wallpaper_dim": 50.5 }), "wallpaper_dim"),
        (json!({ "wallpaper_dim": "50" }), "wallpaper_dim"),
        (json!({ "wallpaper_blur": -1 }), "wallpaper_blur"),
        (json!({ "wallpaper_blur": 21 }), "wallpaper_blur"),
        (json!({ "effects": "yes" }), "effects"),
        (json!({ "accent_color": "red" }), "accent_color"),
        (json!({ "accent_color": "#b3001" }), "accent_color"),
        (json!({ "accent_color": "#gg0000" }), "accent_color"),
    ] {
        let error = ctx
            .expect(
                StatusCode::BAD_REQUEST,
                Method::PUT,
                &featured,
                Some(body),
                Some(&admin),
            )
            .await;
        assert_eq!(error["code"], "validation");
        assert_eq!(error["field"], field);
    }
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(
        got["wallpaper_blur"], 20,
        "rejected request changes nothing"
    );

    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &featured,
        None,
        Some(&admin),
    )
    .await;
    defaults(&ctx.get_ok(&format!("/collections/{top}"), None).await);
}

/// Админ меняет название и описание чужой коллекции (не admin — 403, нет — 404, неверное поле —
/// 400 с `field`, `is_public` — 400); режим шапки особой: по умолчанию `styled`, установка,
/// `null` — `styled`, неверное значение — 400, сброс при снятии из особых.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_edits_collection_and_header_mode(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login("admin@example.com", "admin").await;
    let alice = ctx.user("alice").await;
    let top = ctx
        .collection(&alice, json!({ "title": "Дюна", "description": "Старое" }))
        .await;
    let path = format!("/admin/collections/{top}");

    let body = json!({ "title": "Дюна во всех видах", "description": "Книги и экранизации" });
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::PATCH,
        &path,
        Some(body.clone()),
        Some(&alice),
    )
    .await;
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(body),
            Some(&admin),
        )
        .await;
    assert_eq!(got["title"], "Дюна во всех видах");
    assert_eq!(got["description"], "Книги и экранизации");
    assert_eq!(got["is_public"], true);
    // Не передано — не меняется; null — очищает.
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(json!({ "description": null })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["title"], "Дюна во всех видах");
    assert_eq!(got["description"], Value::Null);
    for (body, field) in [
        (json!({ "title": "  " }), "title"),
        (json!({ "description": "x".repeat(2001) }), "description"),
    ] {
        let error = ctx
            .expect(
                StatusCode::BAD_REQUEST,
                Method::PATCH,
                &path,
                Some(body),
                Some(&admin),
            )
            .await;
        assert_eq!(error["code"], "validation");
        assert_eq!(error["field"], field);
    }
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PATCH,
        &path,
        Some(json!({ "is_public": false })),
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        "/admin/collections/00000000-0000-0000-0000-000000000000",
        Some(json!({ "title": "x" })),
        Some(&admin),
    )
    .await;

    let featured = format!("{path}/featured");
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["header_mode"], "styled");
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({ "header_mode": "plain" })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["header_mode"], "plain");
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({ "theme": "horror" })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["header_mode"], "plain", "absent field keeps the value");
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({ "header_mode": "none" })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["header_mode"], "none");
    let list = ctx.get_ok("/collections/featured", None).await;
    assert_eq!(list[0]["header_mode"], "none");
    for bad in [json!("hero"), json!(1)] {
        let error = ctx
            .expect(
                StatusCode::BAD_REQUEST,
                Method::PUT,
                &featured,
                Some(json!({ "header_mode": bad })),
                Some(&admin),
            )
            .await;
        assert_eq!(error["field"], "header_mode");
    }
    let got = ctx
        .expect(
            StatusCode::OK,
            Method::PUT,
            &featured,
            Some(json!({ "header_mode": null })),
            Some(&admin),
        )
        .await;
    assert_eq!(got["header_mode"], "styled");
    ctx.expect(
        StatusCode::OK,
        Method::PUT,
        &featured,
        Some(json!({ "header_mode": "none" })),
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &featured,
        None,
        Some(&admin),
    )
    .await;
    let got = ctx.get_ok(&format!("/collections/{top}"), None).await;
    assert_eq!(got["header_mode"], "styled");

    // Админское удаление на том же пути, что и PATCH.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &path,
        None,
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        &path,
        Some(json!({ "title": "x" })),
        Some(&admin),
    )
    .await;
}

fn pluck_items(detail: &Value) -> Vec<&str> {
    detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["entity"]["slug"].as_str().unwrap())
        .collect()
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

    async fn by_slugs(&self, slugs: &[String]) -> AppResult<HashMap<String, EntityRef>> {
        Ok(slugs
            .iter()
            .filter(|slug| **slug == self.dune.slug)
            .map(|slug| (slug.clone(), self.dune.clone()))
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
            release_date: "2021-10-22".parse().ok(),
        },
    });
    let alice = ctx.user("alice").await;

    let review = ctx
        .review(&alice, "renamed", json!({ "rating": 9, "body": "Да" }))
        .await;
    assert_eq!(review["entity"]["title"], "Из справочника");
    assert_eq!(review["entity"]["release_date"], "2021-10-22");
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
