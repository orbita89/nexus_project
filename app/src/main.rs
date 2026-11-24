//! Точка входа: конфиг, логирование, БД, миграции, фоновые задачи, HTTP-сервер.
//! `nexus media fill|check` — разовая команда вместо сервера: постеры и трейлеры каталога.
//! `nexus archive dump|build|ids|keep-cold` — горячий слой каталога и холодный архив
//! (`catalog::archive`).
//! `nexus books import` — метаданные русских книг из SQL-дампа LibGen (`catalog::books`).
//! `nexus books cassandra DIR` — книги библиотеки Cassandra (`catalog::books::cassandra`).
//! `nexus books index` — перестроить книжный индекс Tantivy из БД (`catalog::books::sync`);
//! `nexus books search QUERY` — запрос к нему из терминала.
//! `nexus books ranks FILE` — ручной порядок книг из списка `Название — Автор` (место — номер
//! строки) в `catalog_manual_ranks`; `nexus books sidecar` — пересобрать слой Б (фасеты и
//! сортировки в RAM) и его снимок без записи в Tantivy.
//! `nexus ai list|add|set|test|rm` — провайдеры ИИ (`catalog::ai`), ключи шифруются в БД.
//! `nexus search reindex` — перестроить индексы Tantivy (как `POST /admin/search/reindex`);
//! `nexus search bench [ROUNDS]` — замер поиска: книги и фильмы (Tantivy).

mod ai_cli;
mod bench;
mod books_load;

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

    if args.first().map(String::as_str) == Some("ai") {
        return ai_cli::run(&state, &args[1..]).await;
    }

    if args.first().map(String::as_str) == Some("books") {
        match books_command(&args[1..]).map_err(anyhow::Error::msg)? {
            BooksCommand::Import(options) => run_books(&state, &options).await?,
            BooksCommand::Cassandra(options) => run_cassandra(&state, &options).await?,
            BooksCommand::Index => {
                let started = std::time::Instant::now();
                let books = catalog::books::sync::rebuild(&state)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("books indexed: {books} in {:.1?}", started.elapsed());
            }
            BooksCommand::Search(query) => run_books_search(&state, query)?,
            BooksCommand::Ranks(path, unmatched) => {
                run_books_ranks(&state, &path, unmatched.as_deref()).await?
            }
            BooksCommand::Sidecar => {
                let started = std::time::Instant::now();
                let books = catalog::books::sync::reload_sidecar(&state)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("books sidecar: {books} books in {:.1?}", started.elapsed());
            }
            BooksCommand::Load(options) => {
                let dir = state
                    .config
                    .books_index_dir
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("BOOKS_INDEX_DIR is not set"))?;
                tokio::task::spawn_blocking(move || books_load::run(&dir, &options)).await??;
            }
        }
        state.cache.wait_pending().await;
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("search") {
        match args.get(1).map(String::as_str) {
            Some("reindex") => {
                let options = catalog::search::ReindexOptions {
                    force: false,
                    revalidate: false,
                };
                let reporter = catalog::jobs::Reporter::silent();
                let result = catalog::search::reindex(&state, options, &reporter)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("search reindexed: {result:?}");
            }
            Some("rebuild") => {
                let what = args.get(2).map(String::as_str).unwrap_or("all");
                if !["works", "people", "all"].contains(&what) {
                    anyhow::bail!("usage: nexus search rebuild [works|people|all]");
                }
                let reporter = catalog::jobs::Reporter::silent();
                let started = std::time::Instant::now();
                // Последовательно: память на слабом железе.
                if what != "people" {
                    let n = catalog::works::sync::rebuild(&state, &reporter)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    println!("works indexed: {n} in {:.1?}", started.elapsed());
                }
                if what != "works" {
                    let n = catalog::people_idx::sync::rebuild(&state, &reporter)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    println!("people indexed: {n} in {:.1?}", started.elapsed());
                }
            }
            Some("query") => {
                let (Some(index), Some(q)) = (args.get(2), args.get(3)) else {
                    anyhow::bail!("usage: nexus search query <works|people> <q>");
                };
                let hits = match index.as_str() {
                    "works" => {
                        let index = catalog::works::search::index(&state)?;
                        let query = catalog::works::index::WorksQuery {
                            q: q.clone(),
                            ..Default::default()
                        };
                        index.search(&query, 10, 0)?
                    }
                    "people" => match catalog::people_idx::sync::open(&state) {
                        Some(index) => {
                            index?
                                .search(
                                    q,
                                    None,
                                    catalog::people_idx::index::PeopleSort::Relevance,
                                    10,
                                    0,
                                )?
                                .0
                        }
                        None => anyhow::bail!("SEARCH_INDEX_DIR is not set"),
                    },
                    _ => anyhow::bail!("usage: nexus search query <works|people> <q>"),
                };
                for hit in hits {
                    println!("{}", hit);
                }
            }
            Some("bench") => {
                let rounds = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(5);
                bench::run(&state, rounds).await?;
            }
            _ => anyhow::bail!("usage: nexus search reindex\n       nexus search rebuild [works|people|all]\n       nexus search query <works|people> <q>\n       nexus search bench [ROUNDS]"),
        }
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
    // Индексы работ и людей автоматически не перестраиваются (ни при старте, ни по расписанию):
    // только вручную — страница «Индексы» админки, `nexus search rebuild`.
    catalog::search::spawn_ratings_sync(state.clone());
    // Книжный индекс Tantivy: пуст (первый запуск, новая схема) — строится в фоне из БД.
    catalog::books::sync::spawn_initial(state.clone());
    social::ratings::spawn_sync_all(state.clone());
    // Индекс коллекций: строится при старте, дальше его правят сами записи.
    social::collection_search::spawn_fill(state.clone());
    // Рейтинг пользователей: полный пересчёт раз в сутки, первый — сразу после старта.
    social::reputation::spawn(state.clone());
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

const BOOKS_USAGE: &str = "usage: nexus books import --dump FILE.sql[.gz] [--table updated|fiction] [--batch-size N] [--limit N]\n       \
     nexus books cassandra DIR [--batch-size N] [--limit N]\n       \
     nexus books index\n       \
     nexus books search QUERY\n       \
     nexus books ranks FILE [--unmatched OUT]\n       \
     nexus books sidecar\n       \
     nexus books load [--seconds N] [--threads 1,4,16] [--limit N]";

enum BooksCommand {
    /// Нагрузка на индекс Tantivy с отчётом в консоль.
    Load(books_load::Options),
    /// Дамп LibGen.
    Import(catalog::books::ImportOptions),
    /// Библиотека Cassandra: каталог с `metadata/` и `covers/`.
    Cassandra(catalog::books::cassandra::Options),
    /// Перестроить книжный индекс Tantivy из БД (`BOOKS_INDEX_DIR`).
    Index,
    /// Запрос к книжному индексу из терминала: проверить выдачу и время без HTTP.
    Search(String),
    /// Ручной порядок книг из файла списка; второй путь — куда записать строки без места.
    Ranks(std::path::PathBuf, Option<std::path::PathBuf>),
    /// Пересобрать слой Б из БД.
    Sidecar,
}

/// Аргументы `nexus books ...`.
fn books_command(args: &[String]) -> Result<BooksCommand, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if args == ["index"] {
        return Ok(BooksCommand::Index);
    }
    if let ["load", rest @ ..] = &args[..] {
        return books_load::parse(rest).map(BooksCommand::Load);
    }
    if let ["search", query] = &args[..] {
        return Ok(BooksCommand::Search((*query).to_string()));
    }
    if let ["ranks", file] = &args[..] {
        return Ok(BooksCommand::Ranks(file.into(), None));
    }
    if let ["ranks", file, "--unmatched", out] = &args[..] {
        return Ok(BooksCommand::Ranks(file.into(), Some(out.into())));
    }
    if args == ["sidecar"] {
        return Ok(BooksCommand::Sidecar);
    }
    if let ["cassandra", dir, rest @ ..] = &args[..] {
        let mut options = catalog::books::cassandra::Options::new(dir.into());
        let mut rest = rest.iter();
        let number = |value: Option<&&str>| -> Result<usize, String> {
            value
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| BOOKS_USAGE.to_string())
        };
        while let Some(arg) = rest.next() {
            match *arg {
                "--batch-size" => options.batch_size = number(rest.next())?,
                "--limit" => options.limit = Some(number(rest.next())? as u64),
                _ => return Err(BOOKS_USAGE.into()),
            }
        }
        return Ok(BooksCommand::Cassandra(options));
    }
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
    Ok(BooksCommand::Import(options))
}

/// `nexus books ranks FILE`: список в `catalog_manual_ranks` и отчёт — сколько строк нашлось,
/// чего нет в каталоге и где автор не совпал (по 20 примеров).
async fn run_books_ranks(
    state: &AppState,
    path: &std::path::Path,
    unmatched: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let (lines, matched) = catalog::books::ranks::import(state, path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "lines: {lines}; ranked: {}; not in catalog: {}; other author: {}; repeated: {}; beyond 65535: {}",
        matched.ranks.len(),
        matched.not_found.len(),
        matched.other_author.len(),
        matched.repeated.len(),
        matched.beyond,
    );
    for (label, list) in [
        ("not in catalog", &matched.not_found),
        ("other author", &matched.other_author),
    ] {
        for line in list.iter().take(20) {
            println!("  {label}: {line}");
        }
    }
    if let Some(out) = unmatched {
        // Строки без места целиком, по разделам: чего нет в каталоге, где автор другой, повторы.
        let mut text = String::new();
        for (title, list) in [
            ("НЕТ В КАТАЛОГЕ", &matched.not_found),
            ("НАЗВАНИЕ ЕСТЬ, АВТОР ДРУГОЙ", &matched.other_author),
            ("ПОВТОР (книга уже получила место выше)", &matched.repeated),
        ] {
            text.push_str(&format!("# {title}: {}\n", list.len()));
            for line in list {
                text.push_str(line);
                text.push('\n');
            }
            text.push('\n');
        }
        std::fs::write(out, text)?;
        println!("unmatched lines written to {}", out.display());
    }
    Ok(())
}

/// `nexus books search QUERY`: первые 10 книг по запросу, всего найдено и время — холодный
/// запрос и повтор из кэша.
fn run_books_search(state: &AppState, query: String) -> anyhow::Result<()> {
    use catalog::books::index::BookQuery;
    let index = catalog::books::sync::open(state)
        .ok_or_else(|| anyhow::anyhow!("BOOKS_INDEX_DIR is not set"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let query = BookQuery {
        q: query,
        ..BookQuery::default()
    };
    let started = std::time::Instant::now();
    let found = index
        .search(&query, 10, 0)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cold = started.elapsed();
    let started = std::time::Instant::now();
    index
        .search(&query, 10, 0)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cached = started.elapsed();
    println!(
        "books in index: {}; found: {}{}; {cold:.1?} (cached: {cached:.1?})",
        index.len(),
        found.total,
        if found.approximate {
            " (typo fixed)"
        } else {
            ""
        },
    );
    for item in found.items {
        let authors: Vec<&str> = item.authors.iter().map(|a| a.name.as_str()).collect();
        println!("  {} — {} [{}]", item.title, authors.join(", "), item.slug);
    }
    Ok(())
}

/// Импорт библиотеки Cassandra. Поисковые индексы после него строятся отдельно: книжный —
/// `nexus books index`, люди (новые авторы) — `POST /admin/search/reindex`.
async fn run_cassandra(
    state: &AppState,
    options: &catalog::books::cassandra::Options,
) -> anyhow::Result<()> {
    let report = catalog::books::cassandra::import(state, options)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    print!("{report}");
    Ok(())
}

/// Импорт книг, затем перестройка поиска (новые книги и авторы — в индексы).
async fn run_books(
    state: &AppState,
    options: &catalog::books::ImportOptions,
) -> anyhow::Result<()> {
    let report = catalog::books::import(state, options)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    print!("{report}");
    if state.config.search_index_dir.is_some() && report.books_created > 0 {
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
            if state.config.search_index_dir.is_some() && !options.defer_delete {
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
