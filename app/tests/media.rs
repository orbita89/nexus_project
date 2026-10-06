//! `nexus media fill | check`: что заполняется, что заменяется, что остаётся как есть.
//! Источники — подделка без сети ([`FakeFinder`]); выбор по ответам настоящих API проверяют
//! unit-тесты `catalog::media::pick`.

use catalog::media::{run_with, Finder, Liveness, Mode, Options, Target};
use catalog::metadata::{TrailerProvider, TrailerSource};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashSet;

const YT: &str = "n9xhJrPXop4";
const YT_NEW: &str = "Way9Dexny3w";
const RT: &str = "0ab1fc1e47f2e9b89e9e59d9db36f2b4";
const RT_NEW: &str = "264355f929e2781efa3153905b5d5ea3";

/// Находит всё для любой сущности; «мёртвыми» и «неизвестными» считает то, что перечислено.
#[derive(Default)]
struct FakeFinder {
    dead: HashSet<String>,
    unknown: HashSet<String>,
    nothing_found: bool,
}

impl FakeFinder {
    fn liveness(&self, key: &str) -> Liveness {
        if self.dead.contains(key) {
            Liveness::Dead
        } else if self.unknown.contains(key) {
            Liveness::Unknown
        } else {
            Liveness::Alive
        }
    }
}

impl Finder for FakeFinder {
    async fn cover(&self, target: &Target) -> Option<String> {
        (!self.nothing_found).then(|| format!("https://img.test/{}.jpg", target.slug))
    }
    async fn youtube_trailer(&self, _: &Target) -> Option<String> {
        (!self.nothing_found).then(|| YT_NEW.to_string())
    }
    async fn rutube_trailer(&self, _: &Target) -> Option<String> {
        (!self.nothing_found).then(|| RT_NEW.to_string())
    }
    async fn image_alive(&self, url: &str) -> Liveness {
        self.liveness(url)
    }
    async fn video_alive(&self, source: &TrailerSource) -> Liveness {
        self.liveness(&source.id)
    }
}

async fn insert(pool: &PgPool, kind: &str, slug: &str, cover: Option<&str>, metadata: Value) {
    sqlx::query(
        "INSERT INTO entities (kind, slug, title, release_date, cover_url, metadata)
         VALUES ($1::entity_kind, $2, $2, '2021-10-22', $3, $4)",
    )
    .bind(kind)
    .bind(slug)
    .bind(cover)
    .bind(metadata)
    .execute(pool)
    .await
    .unwrap();
}

async fn media(pool: &PgPool, slug: &str) -> (Option<String>, Value) {
    sqlx::query_as("SELECT cover_url, metadata FROM entities WHERE slug = $1")
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn options(mode: Mode) -> Options {
    Options {
        mode,
        dry_run: false,
        slug: None,
        limit: None,
    }
}

fn sources(list: &[(TrailerProvider, &str)]) -> Value {
    json!(list
        .iter()
        .map(|(provider, id)| TrailerSource {
            provider: *provider,
            id: id.to_string(),
        })
        .collect::<Vec<_>>())
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn fill_adds_only_what_is_missing(pool: PgPool) {
    let (state, _) = test_utils::state(pool.clone());
    insert(&pool, "movie", "empty", None, json!({ "runtime_min": 155 })).await;
    let manual = sources(&[(TrailerProvider::Youtube, YT)]);
    insert(
        &pool,
        "movie",
        "manual",
        Some("https://img.test/hand-picked.jpg"),
        json!({ "trailers": manual }),
    )
    .await;
    insert(&pool, "book", "novel", None, json!({ "pages": 300 })).await;

    let report = run_with(&state, &FakeFinder::default(), options(Mode::Fill))
        .await
        .unwrap();
    assert_eq!(report.checked, 3);

    // Пустое заполнено: постер, YouTube первым, Rutube запасным; остальная metadata цела.
    let (cover, metadata) = media(&pool, "empty").await;
    assert_eq!(cover.as_deref(), Some("https://img.test/empty.jpg"));
    assert_eq!(metadata["runtime_min"], 155);
    assert_eq!(
        metadata["trailers"],
        sources(&[
            (TrailerProvider::Youtube, YT_NEW),
            (TrailerProvider::Rutube, RT_NEW)
        ])
    );

    // Подобранное вручную не перезаписано, добавлен только недостающий Rutube.
    let (cover, metadata) = media(&pool, "manual").await;
    assert_eq!(cover.as_deref(), Some("https://img.test/hand-picked.jpg"));
    assert_eq!(
        metadata["trailers"],
        sources(&[
            (TrailerProvider::Youtube, YT),
            (TrailerProvider::Rutube, RT_NEW)
        ])
    );

    // У книги — только обложка.
    let (cover, metadata) = media(&pool, "novel").await;
    assert!(cover.is_some());
    assert!(metadata.get("trailers").is_none());

    // Повторный запуск ничего не меняет.
    let again = run_with(&state, &FakeFinder::default(), options(Mode::Fill))
        .await
        .unwrap();
    assert_eq!(again.updated(), 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn check_replaces_dead_links_and_keeps_unknown(pool: PgPool) {
    let (state, _) = test_utils::state(pool.clone());
    let both = sources(&[
        (TrailerProvider::Youtube, YT),
        (TrailerProvider::Rutube, RT),
    ]);
    insert(
        &pool,
        "movie",
        "dead",
        Some("https://img.test/gone.jpg"),
        json!({ "trailers": both }),
    )
    .await;
    insert(
        &pool,
        "movie",
        "flaky",
        Some("https://img.test/flaky.jpg"),
        json!({ "trailers": both }),
    )
    .await;

    let finder = FakeFinder {
        dead: ["https://img.test/gone.jpg".to_string(), YT.to_string()].into(),
        unknown: ["https://img.test/flaky.jpg".to_string(), RT.to_string()].into(),
        ..FakeFinder::default()
    };
    let report = run_with(&state, &finder, options(Mode::Check))
        .await
        .unwrap();

    // Мёртвые постер и YouTube заменены; Rutube «неизвестен» — остался.
    let (cover, metadata) = media(&pool, "dead").await;
    assert_eq!(cover.as_deref(), Some("https://img.test/dead.jpg"));
    assert_eq!(
        metadata["trailers"],
        sources(&[
            (TrailerProvider::Youtube, YT_NEW),
            (TrailerProvider::Rutube, RT)
        ])
    );
    let dead = report.changes.iter().find(|c| c.slug == "dead").unwrap();
    assert!(
        dead.actions.iter().any(|a| a.contains("постер (мёртв)")),
        "{dead:?}"
    );

    // Сбой сети — не повод удалять: постер «неизвестен» и остался. YouTube у этой сущности
    // общий с первой (тот же id) и мёртв — он заменён.
    let (cover, _) = media(&pool, "flaky").await;
    assert_eq!(cover.as_deref(), Some("https://img.test/flaky.jpg"));
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn check_removes_dead_links_even_without_replacement(pool: PgPool) {
    let (state, _) = test_utils::state(pool.clone());
    let only_yt = sources(&[(TrailerProvider::Youtube, YT)]);
    insert(
        &pool,
        "game",
        "lost",
        Some("https://img.test/gone.jpg"),
        json!({ "trailers": only_yt }),
    )
    .await;

    let finder = FakeFinder {
        dead: ["https://img.test/gone.jpg".to_string(), YT.to_string()].into(),
        nothing_found: true,
        ..FakeFinder::default()
    };
    run_with(&state, &finder, options(Mode::Check))
        .await
        .unwrap();

    // Лучше заглушка и «без трейлера», чем битая картинка и пустой плеер.
    let (cover, metadata) = media(&pool, "lost").await;
    assert_eq!(cover, None);
    assert!(metadata.get("trailers").is_none());
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn dry_run_writes_nothing_and_slug_limits_scope(pool: PgPool) {
    let (state, _) = test_utils::state(pool.clone());
    insert(&pool, "movie", "one", None, json!({})).await;
    insert(&pool, "movie", "two", None, json!({})).await;

    let report = run_with(
        &state,
        &FakeFinder::default(),
        Options {
            dry_run: true,
            slug: Some("one".into()),
            ..options(Mode::Fill)
        },
    )
    .await
    .unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.updated(), 1);
    assert_eq!(media(&pool, "one").await.0, None);
    assert!(report.to_string().contains("dry run"));
}
