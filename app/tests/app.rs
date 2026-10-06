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

    let response = test_utils::get(app, "/api/v1/auth/does-not-exist").await;

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

/// Одного и того же режиссёра (роль без персонажа) нельзя добавить к фильму дважды.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn entity_credits_rejects_duplicate_role_without_character(pool: PgPool) {
    sqlx::query(
        "WITH e AS (INSERT INTO entities (kind, slug, title) VALUES ('movie', 'dune-2021', 'Дюна') RETURNING id),
              p AS (INSERT INTO people (slug, full_name) VALUES ('denis-villeneuve', 'Дени Вильнёв') RETURNING id)
         INSERT INTO entity_credits (entity_id, person_id, role) SELECT e.id, p.id, 'director' FROM e, p",
    )
    .execute(&pool)
    .await
    .unwrap();

    let duplicate = sqlx::query(
        "INSERT INTO entity_credits (entity_id, person_id, role)
         SELECT entity_id, person_id, role FROM entity_credits",
    )
    .execute(&pool)
    .await;

    let error = duplicate.expect_err("duplicate credit must be rejected");
    let db_error = error.as_database_error().expect("database error");
    assert!(db_error.is_unique_violation(), "{db_error}");
}

#[tokio::test]
async fn swagger_ui_and_openapi_spec_are_served() {
    let app = nexus::build_app(test_utils::state_without_db());

    let response = test_utils::get(app.clone(), "/api-docs/openapi.json").await;
    assert_eq!(response.status, StatusCode::OK);
    let main = response.json();
    assert!(main["paths"]["/api/v1/auth/login"].is_object());
    // Каталог — в своей вкладке, в основной схеме его нет.
    assert!(main["paths"]["/api/v1/catalog/entities"].is_null());

    let response = test_utils::get(app.clone(), "/api-docs/catalog.json").await;
    assert_eq!(response.status, StatusCode::OK);
    let catalog = response.json();
    assert!(catalog["paths"]["/api/v1/catalog/entities"].is_object());
    assert!(catalog["paths"]["/api/v1/auth/login"].is_null());
    assert!(catalog["components"]["securitySchemes"]["bearer"].is_object());

    let response = test_utils::get(app.clone(), "/api-docs/social.json").await;
    assert_eq!(response.status, StatusCode::OK);
    let social = response.json();
    assert!(social["paths"]["/api/v1/social/entities/{slug}/reviews"].is_object());
    assert!(main["paths"]["/api/v1/social/collections"].is_null());
    assert!(social["components"]["securitySchemes"]["bearer"].is_object());

    let response = test_utils::get(app, "/docs/").await;
    assert_eq!(response.status, StatusCode::OK);
}

/// `documents/api/*.json` — контракт API в репозитории (по файлу на вкладку Swagger UI):
/// любое изменение API видно в PR.
/// Обновить после изменения эндпоинтов: `UPDATE_OPENAPI=1 cargo test -p nexus openapi`.
#[test]
fn openapi_spec_files_are_up_to_date() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../documents/api");
    for doc in nexus::openapi_docs() {
        let path = dir.join(doc.file);
        let actual = doc.spec.to_pretty_json().unwrap() + "\n";

        if std::env::var("UPDATE_OPENAPI").is_ok() {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &actual).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            expected == actual,
            "documents/api/{} is outdated, run: UPDATE_OPENAPI=1 cargo test -p nexus openapi",
            doc.file
        );
    }
}
