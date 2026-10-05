//! Smoke-тесты собранного приложения.

use axum::http::StatusCode;
use sqlx::PgPool;

#[tokio::test]
async fn health_returns_ok() {
    let app = nexus::build_app(test_utils::state_without_db());

    let response = test_utils::get(app, "/health").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["status"], "ok");
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = nexus::build_app(test_utils::state_without_db());

    let response = test_utils::get(app, "/api/auth/does-not-exist").await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

/// `#[sqlx::test]` создаёт отдельную чистую БД и накатывает миграции —
/// если SQL в `migrations/` сломан, тест упадёт.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn migrations_create_schema(pool: PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'
         ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    for expected in ["users", "entities", "reviews", "collections"] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}: {tables:?}"
        );
    }
}
