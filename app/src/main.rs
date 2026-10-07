//! Точка входа: конфиг, логирование, БД, миграции, фоновые задачи, HTTP-сервер.
//! `nexus media fill|check` — разовая команда вместо сервера: постеры и трейлеры каталога.

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

    let directories = nexus::directories(pool.clone());
    let state = AppState::new(config.clone(), pool, mailer, directories);

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("media") {
        let options = media_options(&args[1..]).map_err(anyhow::Error::msg)?;
        let report = catalog::media::run(&state, options)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        print!("{report}");
        // Повторное удаление карточек из Redis (см. shared::cache) — до выхода процесса.
        state.cache.wait_pending().await;
        return Ok(());
    }

    auth::cleanup::spawn(state.db.clone());
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

const MEDIA_USAGE: &str = "usage: nexus media <fill|check> [--dry-run] [--slug SLUG] [--limit N]";

/// Аргументы `nexus media ...`.
fn media_options(args: &[String]) -> Result<catalog::media::Options, String> {
    use catalog::media::{Mode, Options};
    let mode = match args.first().map(String::as_str) {
        Some("fill") => Mode::Fill,
        Some("check") => Mode::Check,
        _ => return Err(MEDIA_USAGE.into()),
    };
    let mut options = Options {
        mode,
        dry_run: false,
        slug: None,
        limit: None,
    };
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--dry-run" => options.dry_run = true,
            "--slug" => options.slug = Some(rest.next().ok_or(MEDIA_USAGE)?.clone()),
            "--limit" => {
                let limit = rest.next().ok_or(MEDIA_USAGE)?;
                options.limit = Some(limit.parse().map_err(|_| MEDIA_USAGE)?);
            }
            _ => return Err(MEDIA_USAGE.into()),
        }
    }
    Ok(options)
}
