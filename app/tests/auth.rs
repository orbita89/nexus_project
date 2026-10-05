//! Авторизация: регистрация, вход, токены, роли.

use axum::http::{Method, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sqlx::PgPool;
use test_utils::{request, TestResponse};

const PASSWORD: &str = "password123";

fn app(pool: &PgPool) -> Router {
    nexus::build_app(test_utils::state(pool.clone()))
}

async fn post(pool: &PgPool, uri: &str, body: Value) -> TestResponse {
    request(app(pool), Method::POST, uri, Some(body), None).await
}

async fn register(pool: &PgPool, username: &str) -> Value {
    let response = post(
        pool,
        "/api/auth/register",
        json!({ "email": format!("{username}@example.com"), "username": username, "password": PASSWORD }),
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "{:?}",
        response.json()
    );
    response.json()
}

/// Регистрирует пользователя, выдаёт ему роль напрямую в БД и логинится заново,
/// чтобы роль попала в токен. Возвращает access-токен.
async fn login_as(pool: &PgPool, username: &str, role: &str) -> String {
    register(pool, username).await;
    sqlx::query("UPDATE users SET role = $2::user_role WHERE username = $1")
        .bind(username)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    let response = post(
        pool,
        "/api/auth/login",
        json!({ "login": username, "password": PASSWORD }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    response.json()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn register_returns_tokens_and_user_role(pool: PgPool) {
    let body = register(&pool, "neo").await;

    assert_eq!(body["token_type"], "Bearer");
    assert!(body["access_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(body["refresh_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty()));
    assert_eq!(body["user"]["username"], "neo");
    assert_eq!(body["user"]["role"], "user");
    assert!(body["user"].get("password_hash").is_none());
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn register_rejects_duplicates_case_insensitively(pool: PgPool) {
    register(&pool, "neo").await;

    let response = post(
        &pool,
        "/api/auth/register",
        json!({ "email": "NEO@example.com", "username": "other", "password": PASSWORD }),
    )
    .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.json()["error"], "email already registered");

    let response = post(
        &pool,
        "/api/auth/register",
        json!({ "email": "other@example.com", "username": "NEO", "password": PASSWORD }),
    )
    .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.json()["error"], "username already taken");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn register_validates_input(pool: PgPool) {
    let response = post(
        &pool,
        "/api/auth/register",
        json!({ "email": "neo@example.com", "username": "neo", "password": "short" }),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn login_by_email_or_username(pool: PgPool) {
    register(&pool, "neo").await;

    for login in ["neo", "NEO@example.com"] {
        let response = post(
            &pool,
            "/api/auth/login",
            json!({ "login": login, "password": PASSWORD }),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "login via {login}");
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn login_fails_the_same_way_for_wrong_password_unknown_user_and_blocked(pool: PgPool) {
    register(&pool, "neo").await;
    register(&pool, "blocked").await;
    sqlx::query("UPDATE users SET is_active = false WHERE username = 'blocked'")
        .execute(&pool)
        .await
        .unwrap();

    for (login, password) in [
        ("neo", "wrong-password"),
        ("nobody", PASSWORD),
        ("blocked", PASSWORD),
    ] {
        let response = post(
            &pool,
            "/api/auth/login",
            json!({ "login": login, "password": password }),
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{login}");
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn me_requires_valid_token(pool: PgPool) {
    let token = register(&pool, "neo").await["access_token"]
        .as_str()
        .unwrap()
        .to_string();

    let response = request(app(&pool), Method::GET, "/api/auth/me", None, Some(&token)).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["username"], "neo");

    let response = request(app(&pool), Method::GET, "/api/auth/me", None, None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let response = request(
        app(&pool),
        Method::GET,
        "/api/auth/me",
        None,
        Some("garbage"),
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn refresh_rotates_token(pool: PgPool) {
    let first = register(&pool, "neo").await["refresh_token"].clone();

    let response = post(
        &pool,
        "/api/auth/refresh",
        json!({ "refresh_token": first }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let second = response.json()["refresh_token"].clone();
    assert_ne!(first, second);

    let response = post(
        &pool,
        "/api/auth/refresh",
        json!({ "refresh_token": second }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn reusing_revoked_refresh_token_revokes_all_sessions(pool: PgPool) {
    let stolen = register(&pool, "neo").await["refresh_token"].clone();
    let fresh = post(
        &pool,
        "/api/auth/refresh",
        json!({ "refresh_token": stolen }),
    )
    .await
    .json()["refresh_token"]
        .clone();

    // Кто-то повторно использует старый токен — отзываем всё, включая свежий.
    let response = post(
        &pool,
        "/api/auth/refresh",
        json!({ "refresh_token": stolen }),
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let response = post(
        &pool,
        "/api/auth/refresh",
        json!({ "refresh_token": fresh }),
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn logout_revokes_refresh_token(pool: PgPool) {
    let refresh = register(&pool, "neo").await["refresh_token"].clone();

    let response = post(
        &pool,
        "/api/auth/logout",
        json!({ "refresh_token": refresh }),
    )
    .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let response = post(
        &pool,
        "/api/auth/refresh",
        json!({ "refresh_token": refresh }),
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_endpoints_require_admin_role(pool: PgPool) {
    for (username, role) in [("plain", "user"), ("writer", "author")] {
        let token = login_as(&pool, username, role).await;
        let response = request(
            app(&pool),
            Method::GET,
            "/api/auth/admin/users",
            None,
            Some(&token),
        )
        .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{role}");
    }

    let response = request(app(&pool), Method::GET, "/api/auth/admin/users", None, None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let token = login_as(&pool, "boss", "admin").await;
    let response = request(
        app(&pool),
        Method::GET,
        "/api/auth/admin/users",
        None,
        Some(&token),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json().as_array().unwrap().len(), 3);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_promotes_user_to_author(pool: PgPool) {
    let admin = login_as(&pool, "boss", "admin").await;
    let user_id = register(&pool, "neo").await["user"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let response = request(
        app(&pool),
        Method::PATCH,
        &format!("/api/auth/admin/users/{user_id}/role"),
        Some(json!({ "role": "author" })),
        Some(&admin),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["role"], "author");

    // Новая роль приходит в токене при следующем входе.
    let response = post(
        &pool,
        "/api/auth/login",
        json!({ "login": "neo", "password": PASSWORD }),
    )
    .await;
    assert_eq!(response.json()["user"]["role"], "author");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_cannot_demote_self(pool: PgPool) {
    let token = login_as(&pool, "boss", "admin").await;
    let me = request(app(&pool), Method::GET, "/api/auth/me", None, Some(&token))
        .await
        .json();

    let response = request(
        app(&pool),
        Method::PATCH,
        &format!("/api/auth/admin/users/{}/role", me["id"].as_str().unwrap()),
        Some(json!({ "role": "user" })),
        Some(&token),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}
