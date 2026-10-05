//! Точка входа: конфиг, логирование, БД, миграции, HTTP-сервер.

use shared::{db, telemetry, AppState, Config};

/// Используется, если `RUST_LOG` не задан.
const DEFAULT_LOG_FILTER: &str =
    "info,nexus=debug,shared=debug,auth=debug,catalog=debug,social=debug,realtime=debug";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init(DEFAULT_LOG_FILTER);
    let config = Config::from_env();
    let pool = db::connect(&config.database_url).await?;

    nexus::MIGRATOR.run(&pool).await?;
    tracing::info!("migrations applied");

    let app = nexus::build_app(AppState::new(config.clone(), pool));

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("nexus listening on {}", config.bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}
