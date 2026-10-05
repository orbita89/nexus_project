//! Инициализация логирования. Уровень задаётся через `RUST_LOG`.

/// `default_filter` применяется, если `RUST_LOG` не задан.
pub fn init(default_filter: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
