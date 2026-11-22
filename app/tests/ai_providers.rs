//! Провайдеры ИИ (`catalog::ai`, `/admin/ai/providers`): только админ, ключи в БД зашифрованы
//! и наружу не отдаются, первый провайдер — по умолчанию, без `AI_SECRET_KEY` — 503.

use axum::http::{Method, StatusCode};
use shared::{AppState, Config};
use sqlx::PgPool;

const BASE: &str = "/api/v1/catalog/admin/ai/providers";

async fn token(state: &AppState, login: &str, role: &str) -> String {
    let response = test_utils::request(
        nexus::build_app(state.clone()),
        Method::POST,
        "/api/v1/auth/dev/login",
        Some(serde_json::json!({ "login": login, "role": role })),
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
    response.json()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn state(pool: PgPool, secret: Option<&str>) -> AppState {
    let mut config = Config::from_env();
    config.ai_secret_key = secret.map(str::to_string);
    test_utils::state_with_config(pool, config).0
}

async fn send(
    state: &AppState,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
    token: &str,
) -> test_utils::TestResponse {
    test_utils::request(
        nexus::build_app(state.clone()),
        method,
        uri,
        body,
        Some(token),
    )
    .await
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_manages_ai_providers_with_encrypted_keys(pool: PgPool) {
    let state = state(pool.clone(), Some("test-secret"));
    let admin = token(&state, "admin@example.com", "admin").await;
    let user = token(&state, "user@example.com", "user").await;
    let body = serde_json::json!({
        "slug": "deepseek", "kind": "openai", "base_url": "https://api.deepseek.com/",
        "model": "deepseek-chat", "api_keys": ["sk-first-key-0001", "sk-second-key-0002"],
    });

    let forbidden = send(&state, Method::GET, BASE, None, &user).await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let bad = send(
        &state,
        Method::POST,
        BASE,
        Some(serde_json::json!({ "slug": "x", "kind": "openai", "model": "m", "api_keys": [] })),
        &admin,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let created = send(&state, Method::POST, BASE, Some(body.clone()), &admin).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.json());
    let json = created.json();
    assert_eq!(json["is_default"], true, "первый провайдер — по умолчанию");
    assert_eq!(json["base_url"], "https://api.deepseek.com");
    assert_eq!(json["keys_count"], 2);
    assert_eq!(json["keys_hint"], "sk-…0001");
    assert!(!String::from_utf8_lossy(&created.body).contains("sk-first-key-0001"));
    let again = send(&state, Method::POST, BASE, Some(body), &admin).await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // В БД ключи зашифрованы, расшифровываются тем же секретом.
    let raw: Vec<u8> = sqlx::query_scalar("SELECT api_keys FROM catalog_ai_providers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("sk-first"));
    let keys: String = sqlx::query_scalar(
        "SELECT pgp_sym_decrypt(api_keys, 'test-secret') FROM catalog_ai_providers",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(keys, "sk-first-key-0001,sk-second-key-0002");

    // Второй провайдер по умолчанию снимает флаг с первого.
    let second = send(
        &state,
        Method::POST,
        BASE,
        Some(serde_json::json!({
            "slug": "claude", "kind": "anthropic", "model": "claude-haiku-5-5",
            "api_keys": ["sk-ant-key-000003"], "is_default": true,
        })),
        &admin,
    )
    .await;
    assert_eq!(second.status, StatusCode::CREATED);
    let list = send(&state, Method::GET, BASE, None, &admin).await.json();
    let slugs: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["slug"].as_str().unwrap().to_string(),
                p["is_default"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(slugs, [("claude".into(), true), ("deepseek".into(), false)]);

    let patched = send(
        &state,
        Method::PATCH,
        &format!("{BASE}/deepseek"),
        Some(serde_json::json!({ "workers": 4, "base_url": "" })),
        &admin,
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK);
    assert_eq!(patched.json()["workers"], 4);
    assert!(patched.json()["base_url"].is_null());
    assert_eq!(patched.json()["keys_count"], 2, "ключи не тронуты");
    let out_of_range = send(
        &state,
        Method::PATCH,
        &format!("{BASE}/deepseek"),
        Some(serde_json::json!({ "per_request": 0 })),
        &admin,
    )
    .await;
    assert_eq!(out_of_range.status, StatusCode::BAD_REQUEST);
    let missing = send(
        &state,
        Method::POST,
        &format!("{BASE}/nope/test"),
        None,
        &admin,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let deleted = send(
        &state,
        Method::DELETE,
        &format!("{BASE}/deepseek"),
        None,
        &admin,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = send(
        &state,
        Method::DELETE,
        &format!("{BASE}/deepseek"),
        None,
        &admin,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn without_secret_providers_cannot_be_created(pool: PgPool) {
    let state = state(pool, None);
    let admin = token(&state, "admin@example.com", "admin").await;
    let response = send(
        &state,
        Method::POST,
        BASE,
        Some(serde_json::json!({
            "slug": "x", "kind": "openai", "model": "m", "api_keys": ["sk-aaaaaaaaaaaa"],
        })),
        &admin,
    )
    .await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
}
