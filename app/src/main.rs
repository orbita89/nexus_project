//! Точка входа: конфиг, логирование, БД, миграции, фоновые задачи, HTTP-сервер.

use shared::mail::Mailer;
use shared::{db, telemetry, AppState, Config};

/// Используется, если `RUST_LOG` не задан.
const DEFAULT_LOG_FILTER: &str =
    "info,nexus=debug,shared=debug,auth=debug,catalog=debug,social=debug,realtime=debug";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init(DEFAULT_LOG_FILTER);
    let config = Config::from_env();
    if config.uses_dev_jwt_secret() {
        tracing::warn!("JWT_SECRET is the development default, set your own in production");
    }
    let mailer = Mailer::from_config(&config).map_err(anyhow::Error::msg)?;
    if matches!(mailer, Mailer::Log) {
        tracing::warn!("SMTP_URL is not set, emails will only be logged");
    }

    let pool = db::connect(&config.database_url).await?;
    nexus::MIGRATOR.run(&pool).await?;
    tracing::info!("migrations applied");

    auth::cleanup::spawn(pool.clone());

    let state = AppState::new(config.clone(), pool, mailer);
    catalog::search::spawn_reindex(state.clone());
    let app = nexus::build_app(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("nexus listening on {}", config.bind_addr);
    if config.api_docs {
        tracing::info!("API docs: /docs");
    }
    axum::serve(listener, app).await?;
    Ok(())
}
