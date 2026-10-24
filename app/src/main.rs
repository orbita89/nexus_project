//! Точка входа: конфиг, логирование, БД, миграции, фоновые задачи, HTTP-сервер.
//! `nexus media fill|check` — разовая команда вместо сервера: постеры и трейлеры каталога.
//! `nexus archive dump|build|ids|keep-cold` — горячий слой каталога и холодный архив
//! (`catalog::archive`).
//! `nexus books import` — метаданные русских книг из SQL-дампа LibGen (`catalog::books`).

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

    if args.first().map(String::as_str) == Some("books") {
        let options = books_options(&args[1..]).map_err(anyhow::Error::msg)?;
        run_books(&state, &options).await?;
        state.cache.wait_pending().await;
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("archive") {
        let command = archive_command(&args[1..]).map_err(anyhow::Error::msg)?;
        run_archive(&state, command).await?;
        state.cache.wait_pending().await;
        return Ok(());
    }

    auth::cleanup::spawn(state.db.clone());
    catalog::search::spawn_reindex(state.clone());
    catalog::search::spawn_ratings_sync(state.clone());
    social::ratings::spawn_sync_all(state.clone());
    // Вложения, которые загрузили и не отправили: висят на диске без сообщения.
    match social::attachments::cleanup_orphans(&state).await {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "cleaned up orphan chat attachments"),
        Err(e) => tracing::warn!(error = %e, "orphan attachment cleanup failed"),
    }
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

const ARCHIVE_USAGE: &str = "usage: nexus archive dump FILE.tsv\n       \
     nexus archive build FILE.tsv [--fresh] [--defer-delete] [--ru N] [--foreign N]\n       \
     nexus archive ids [--manual]\n       \
     nexus archive keep-cold TMDB_ID...";

enum ArchiveCommand {
    Dump(std::path::PathBuf),
    Build(std::path::PathBuf, catalog::archive::pipeline::BuildOptions),
    /// TMDB id документов архива (с `--manual` — перенесённых вручную), по одному в строке.
    Ids {
        manual: bool,
    },
    /// Пометить документы перенесёнными вручную.
    KeepCold(Vec<i32>),
}

/// Аргументы `nexus archive ...`.
fn archive_command(args: &[String]) -> Result<ArchiveCommand, String> {
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["ids"] => return Ok(ArchiveCommand::Ids { manual: false }),
        ["ids", "--manual"] => return Ok(ArchiveCommand::Ids { manual: true }),
        ["keep-cold", ref ids @ ..] if !ids.is_empty() => {
            let ids = ids
                .iter()
                .map(|id| id.parse().map_err(|_| ARCHIVE_USAGE.to_string()))
                .collect::<Result<_, _>>()?;
            return Ok(ArchiveCommand::KeepCold(ids));
        }
        _ => {}
    }
    let (command, file) = match args {
        [command, file, ..] => (command.as_str(), std::path::PathBuf::from(file)),
        _ => return Err(ARCHIVE_USAGE.into()),
    };
    let mut options = catalog::archive::pipeline::BuildOptions::default();
    let mut rest = args[2..].iter();
    let number = |value: Option<&String>| -> Result<usize, String> {
        value
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| ARCHIVE_USAGE.to_string())
    };
    match command {
        "dump" if args.len() == 2 => Ok(ArchiveCommand::Dump(file)),
        "build" => {
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--fresh" => options.fresh = true,
                    "--defer-delete" => options.defer_delete = true,
                    "--ru" => options.ru = number(rest.next())?,
                    "--foreign" => options.foreign = number(rest.next())?,
                    _ => return Err(ARCHIVE_USAGE.into()),
                }
            }
            Ok(ArchiveCommand::Build(file, options))
        }
        _ => Err(ARCHIVE_USAGE.into()),
    }
}

const BOOKS_USAGE: &str = "usage: nexus books import --dump FILE.sql[.gz] [--table updated|fiction] [--batch-size N] [--limit N]";

/// Аргументы `nexus books import ...`.
fn books_options(args: &[String]) -> Result<catalog::books::ImportOptions, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ["import", rest @ ..] = &args[..] else {
        return Err(BOOKS_USAGE.into());
    };
    let mut dump = None;
    let mut options = catalog::books::ImportOptions::new(std::path::PathBuf::new());
    let mut rest = rest.iter();
    let number = |value: Option<&&str>| -> Result<usize, String> {
        value
            .and_then(|v| v.parse().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| BOOKS_USAGE.to_string())
    };
    while let Some(arg) = rest.next() {
        match *arg {
            "--dump" => dump = Some(rest.next().ok_or(BOOKS_USAGE)?.into()),
            "--table" => options.table = rest.next().ok_or(BOOKS_USAGE)?.to_string(),
            "--batch-size" => options.batch_size = number(rest.next())?,
            "--limit" => options.limit = Some(number(rest.next())? as u64),
            _ => return Err(BOOKS_USAGE.into()),
        }
    }
    options.dump = dump.ok_or(BOOKS_USAGE)?;
    Ok(options)
}

/// Импорт книг, затем перестройка поиска (новые книги и авторы — в Meilisearch).
async fn run_books(
    state: &AppState,
    options: &catalog::books::ImportOptions,
) -> anyhow::Result<()> {
    let report = catalog::books::import(state, options)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    print!("{report}");
    if state.search.is_enabled() && report.books_created > 0 {
        let options = catalog::search::ReindexOptions {
            force: false,
            revalidate: true,
        };
        let reporter = catalog::jobs::Reporter::silent();
        let result = catalog::search::reindex(state, options, &reporter)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("search reindexed: {result:?}");
    }
    Ok(())
}

/// `dump` — дамп из БД и архива; `build` — горячий слой и архив из дампа, затем перестройка
/// поиска сайта (с `force`: горячий слой мог уменьшиться) и всей статики; с `--defer-delete` —
/// без неё (сначала `scripts/archive-purge.sql`, затем `POST /admin/search/reindex?force=true`).
async fn run_archive(state: &AppState, command: ArchiveCommand) -> anyhow::Result<()> {
    use catalog::archive::{self, pipeline};
    let archive = archive::open(state).map_err(|e| anyhow::anyhow!("{e} (set ARCHIVE_DIR)"))?;
    match command {
        ArchiveCommand::Dump(file) => {
            let report = pipeline::dump(state, Some(archive), &file)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            print!("{report}");
        }
        ArchiveCommand::Build(file, options) => {
            let report = pipeline::build(state, archive, &file, options)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            print!("{report}");
            // Отложенное удаление: до archive-purge.sql холодные строки ещё в БД.
            if state.search.is_enabled() && !options.defer_delete {
                let options = catalog::search::ReindexOptions {
                    force: true,
                    revalidate: true,
                };
                let reporter = catalog::jobs::Reporter::silent();
                let result = catalog::search::reindex(state, options, &reporter)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("search reindexed: {result:?}");
            }
        }
        ArchiveCommand::Ids { manual } => {
            use std::io::Write;
            let (all, manuals) = tokio::task::spawn_blocking(move || archive.tmdb_ids())
                .await?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut ids: Vec<i32> = if manual { manuals } else { all }.into_iter().collect();
            ids.sort_unstable();
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            for id in ids {
                writeln!(out, "{id}")?;
            }
            out.flush()?;
        }
        ArchiveCommand::KeepCold(ids) => {
            if !archive.is_current() {
                anyhow::bail!("archive index has no `manual` field: run `nexus archive build FILE --fresh` first");
            }
            let marked = tokio::task::spawn_blocking(move || pipeline::mark_manual(&archive, &ids))
                .await?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("marked as kept in the archive: {marked}");
        }
    }
    Ok(())
}
