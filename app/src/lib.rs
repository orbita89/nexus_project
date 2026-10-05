//! nexus — модульный монолит. Здесь собирается HTTP-приложение из роутеров модулей;
//! `main.rs` только поднимает ресурсы и запускает сервер. Разделение нужно, чтобы тесты
//! могли собрать то же приложение без сети.
//! Модули друг от друга не зависят: всё общее приходит через `shared::AppState`.

use axum::{routing::get, Json, Router};
use shared::AppState;

/// Миграции из `migrations/`, встроенные в бинарник.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .nest("/api/auth", auth::router())
        .nest("/api/catalog", catalog::router())
        .nest("/api/social", social::router())
        .merge(realtime::router())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "service": "nexus", "status": "ok" }))
}
