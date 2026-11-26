//! `nexus search bench [ROUNDS]` — замер поиска: книги против фильмов (оба — Tantivy).
//!
//! Запросы идут в настоящий роутер приложения в этом же процессе (без сети и nginx): тот же
//! путь, что у запроса с сайта, — разбор параметров, поиск, JSON ответа. Для фильмов — индекс
//! `works`, для книг — индекс `books`, оба в процессе. Одинаковые по смыслу
//! запросы идут парами; книги меряются дважды — без кэша ответов (цена самого запроса) и с ним.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use shared::AppState;
use tower::ServiceExt;

/// Группа одинаковых по смыслу запросов: для фильмов и для книг.
struct Group {
    title: &'static str,
    /// Параметры после `/api/v1/catalog/` без `kind`: `search?q=...`.
    movies: Vec<String>,
    books: Vec<String>,
}

fn search(q: &str) -> String {
    let q: String = form_urlencoded::byte_serialize(q.as_bytes()).collect();
    format!("search?limit=24&q={q}")
}

fn groups() -> Vec<Group> {
    let each = |queries: &[&str]| queries.iter().map(|q| search(q)).collect::<Vec<_>>();
    let one = |uri: &str| vec![uri.to_string()];
    vec![
        Group {
            title: "Название целиком",
            movies: each(&[
                "интерстеллар",
                "крёстный отец",
                "властелин колец",
                "звёздные войны",
                "гарри поттер",
                "терминатор",
                "назад в будущее",
                "брат",
            ]),
            books: each(&[
                "собачье сердце",
                "мастер и маргарита",
                "война и мир",
                "преступление и наказание",
                "гарри поттер",
                "анна каренина",
                "три мушкетёра",
                "основание",
            ]),
        },
        Group {
            title: "Недописанное слово",
            movies: each(&[
                "интерст",
                "власт",
                "термин",
                "звёздные во",
                "назад в буд",
                "крёстн",
            ]),
            books: each(&[
                "собач",
                "прест",
                "мастер и марг",
                "война и ми",
                "три мушк",
                "каренин",
            ]),
        },
        Group {
            title: "Опечатка",
            movies: each(&["интерстелар", "терменатор", "властилин колец", "гари потер"]),
            books: each(&[
                "приступление",
                "собачие сердце",
                "мастер и маргорита",
                "гари потер",
            ]),
        },
        Group {
            title: "Имя человека / автора",
            movies: each(&["нолан", "тарантино", "спилберг", "балабанов"]),
            books: each(&["булгаков", "азимов", "достоевский", "стругацкие"]),
        },
        Group {
            title: "Раздел без текста (рекомендуемые, 24)",
            movies: one("search?limit=24&sort=recommended"),
            books: one("search?limit=24&sort=recommended"),
        },
        Group {
            title: "Раздел: новинки",
            movies: one("search?limit=24&sort=new"),
            books: one("search?limit=24&sort=new"),
        },
        Group {
            title: "Фильтр по годам 2000–2010",
            movies: one("search?limit=24&sort=recommended&year_from=2000&year_to=2010"),
            books: one("search?limit=24&sort=recommended&year_from=2000&year_to=2010"),
        },
        Group {
            title: "Фильтр по жанру",
            movies: one("search?limit=24&sort=recommended&tag=drama"),
            books: one("search?limit=24&sort=recommended&tag=fantasy"),
        },
        Group {
            title: "Глубокая страница (offset 2400)",
            movies: one("search?limit=24&offset=2400&sort=recommended"),
            books: one("search?limit=24&offset=2400&sort=recommended"),
        },
        Group {
            title: "Жанры раздела (подсказки фильтра)",
            movies: one("search/tags?limit=50"),
            books: one("search/tags?limit=50"),
        },
    ]
}

/// Один запрос к роутеру: время до полного ответа и `total` выдачи (если есть).
async fn ask(app: &axum::Router, uri: &str) -> anyhow::Result<(Duration, Option<i64>)> {
    let request = Request::builder().uri(uri).body(Body::empty())?;
    let started = Instant::now();
    let response = app.clone().oneshot(request).await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let elapsed = started.elapsed();
    if status != StatusCode::OK {
        anyhow::bail!("{uri}: {status} {}", String::from_utf8_lossy(&body));
    }
    let total = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json["total"].as_i64());
    Ok((elapsed, total))
}

/// Все запросы группы `rounds` раз: времена и сумма `total` одного круга.
async fn measure(
    app: &axum::Router,
    kind: &str,
    uris: &[String],
    rounds: usize,
) -> anyhow::Result<(Vec<Duration>, i64)> {
    let mut times = Vec::with_capacity(uris.len() * rounds);
    let mut found = 0;
    for round in 0..rounds {
        for uri in uris {
            let (elapsed, total) = ask(app, &format!("/api/v1/catalog/{uri}&kind={kind}")).await?;
            times.push(elapsed);
            if round == 0 {
                found += total.unwrap_or(0);
            }
        }
    }
    times.sort_unstable();
    Ok((times, found))
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Медиана и 95-й перцентиль отсортированных времён, мс.
fn stats(times: &[Duration]) -> (f64, f64) {
    let at = |share: f64| {
        let index = ((times.len() as f64 - 1.0) * share).round() as usize;
        ms(times[index])
    };
    (at(0.5), at(0.95))
}

pub async fn run(state: &AppState, rounds: usize) -> anyhow::Result<()> {
    let rounds = rounds.max(1);
    let index = catalog::index::sync::open(state)
        .ok_or_else(|| anyhow::anyhow!("SEARCH_INDEX_DIR is not set"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let app = nexus::build_app(state.clone());
    println!(
        "build: {}; rounds: {rounds}; books in index: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        index.len()
    );
    println!("время — мс: медиана / 95-й перцентиль; «найдено» — сумма total по запросам группы\n");
    println!(
        "{:<40} {:>18} {:>18} {:>18}   найдено: фильмы / книги",
        "группа", "фильмы", "книги", "книги из кэша"
    );
    let mut all: [Vec<Duration>; 3] = Default::default();
    for group in groups() {
        // Прогрев: первый запрос открывает индексы и их страницы.
        measure(&app, "movie", &group.movies, 1).await?;
        index.set_cached(false);
        measure(&app, "book", &group.books, 1).await?;

        let (movies, movies_found) = measure(&app, "movie", &group.movies, rounds).await?;
        let (books, books_found) = measure(&app, "book", &group.books, rounds).await?;
        index.set_cached(true);
        measure(&app, "book", &group.books, 1).await?;
        let (cached, _) = measure(&app, "book", &group.books, rounds).await?;

        let cell = |times: &[Duration]| {
            let (p50, p95) = stats(times);
            format!("{p50:.2} / {p95:.2}")
        };
        println!(
            "{:<40} {:>18} {:>18} {:>18}   {movies_found} / {books_found}",
            group.title,
            cell(&movies),
            cell(&books),
            cell(&cached)
        );
        all[0].extend(movies);
        all[1].extend(books);
        all[2].extend(cached);
    }
    for times in &mut all {
        times.sort_unstable();
    }
    let cell = |times: &[Duration]| {
        let (p50, p95) = stats(times);
        format!("{p50:.2} / {p95:.2}")
    };
    println!(
        "{:<40} {:>18} {:>18} {:>18}",
        "ВСЕГО",
        cell(&all[0]),
        cell(&all[1]),
        cell(&all[2])
    );
    Ok(())
}
