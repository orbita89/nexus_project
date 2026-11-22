//! Скрипты админки (`catalog::scripts`, `/admin/scripts`): только админ, реестр с флагами,
//! флаги проверяются, без `SCRIPTS_PYTHON` запуск выключен, отчёт и задачи — 404, если их нет.
//! Сами скрипты здесь не запускаются (им нужны БД каталога и ключи ИИ).

use axum::http::{Method, StatusCode};
use shared::{AppState, Config};
use sqlx::PgPool;

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

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_sees_registry_and_flags_are_checked(pool: PgPool) {
    let mut config = Config::from_env();
    config.scripts_python = None;
    let state = test_utils::state_with_config(pool, config).0;
    let admin = token(&state, "admin@example.com", "admin").await;
    let user = token(&state, "user@example.com", "user").await;
    let send = |method: Method, uri: &str, body: Option<serde_json::Value>, token: &str| {
        let (app, uri, token) = (
            nexus::build_app(state.clone()),
            uri.to_string(),
            token.to_string(),
        );
        async move { test_utils::request(app, method, &uri, body, Some(&token)).await }
    };

    let forbidden = send(Method::GET, "/api/v1/catalog/admin/scripts", None, &user).await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let list = send(Method::GET, "/api/v1/catalog/admin/scripts", None, &admin).await;
    assert_eq!(list.status, StatusCode::OK);
    let run = list
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "books-llm")
        .cloned()
        .expect("books-llm in registry");
    let limit = run["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "limit")
        .unwrap();
    assert_eq!(limit["type"], "int");
    assert!(limit["description"].as_str().unwrap().len() > 10);

    let bad = send(
        Method::POST,
        "/api/v1/catalog/admin/scripts/books-llm/run",
        Some(serde_json::json!({ "flags": { "ai": "x; rm -rf /" } })),
        &admin,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let unknown = send(
        Method::POST,
        "/api/v1/catalog/admin/scripts/books-llm/run",
        Some(serde_json::json!({ "flags": { "evil": true } })),
        &admin,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    let off = send(
        Method::POST,
        "/api/v1/catalog/admin/scripts/books-llm/run",
        Some(serde_json::json!({ "flags": { "limit": 10 } })),
        &admin,
    )
    .await;
    assert_eq!(off.status, StatusCode::SERVICE_UNAVAILABLE);
    let missing = send(
        Method::POST,
        "/api/v1/catalog/admin/scripts/nope/run",
        Some(serde_json::json!({})),
        &admin,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let jobs = send(
        Method::GET,
        "/api/v1/catalog/admin/scripts/jobs",
        None,
        &admin,
    )
    .await;
    assert_eq!(jobs.status, StatusCode::OK);
    let no_job = send(
        Method::GET,
        "/api/v1/catalog/admin/scripts/jobs/999999",
        None,
        &admin,
    )
    .await;
    assert_eq!(no_job.status, StatusCode::NOT_FOUND);
    let no_cancel = send(
        Method::POST,
        "/api/v1/catalog/admin/scripts/jobs/999999/cancel",
        None,
        &admin,
    )
    .await;
    assert_eq!(no_cancel.status, StatusCode::NOT_FOUND);
    let no_report = send(
        Method::GET,
        "/api/v1/catalog/admin/scripts/nope/report",
        None,
        &admin,
    )
    .await;
    assert_eq!(no_report.status, StatusCode::NOT_FOUND);
}
