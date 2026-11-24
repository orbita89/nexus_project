//! Поиск работ на Tantivy (`catalog::works`): выдача идёт из индекса в `SEARCH_INDEX_DIR`.

use catalog::jobs::Reporter;
use serde_json::Value;
use shared::{AppState, Config};
use sqlx::PgPool;
use std::path::PathBuf;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state(pool: PgPool) -> (AppState, Dir) {
    let dir = Dir(std::env::temp_dir().join(format!("nexus-works-{}", uuid::Uuid::new_v4())));
    let mut config = Config::from_env();
    config.search_index_dir = Some(dir.0.to_string_lossy().into_owned());
    (test_utils::state_with_config(pool, config).0, dir)
}

async fn get(state: &AppState, uri: &str) -> Value {
    let response = test_utils::get(
        nexus::build_app(state.clone()),
        &format!("/api/v1/catalog{uri}"),
    )
    .await;
    assert_eq!(response.status, 200, "{uri}");
    response.json()
}

fn slugs(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["slug"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn works_search_sorts_neighbors_and_similar(pool: PgPool) {
    let (state, _dir) = state(pool);
    let built = catalog::works::sync::rebuild(&state, &Reporter::silent())
        .await
        .unwrap();
    assert!(built >= 3, "{built}");

    let page = get(&state, "/search?kind=movie&q=%D0%B4%D1%8E%D0%BD%D0%B0").await;
    assert!(slugs(&page).contains(&"dune-2021"), "{page}");
    assert!(!slugs(&page).contains(&"dune-novel"), "книги — не в works");
    let games = get(&state, "/search?kind=game").await;
    assert_eq!(slugs(&games), ["witcher-3"]);
    let page = get(&state, "/search?kind=movie&sort=top_rated").await;
    assert_eq!(page["total"], 2);
    let page = get(&state, "/search?kind=movie&sort=title&q=").await;
    assert_eq!(slugs(&page).len(), 2);

    let neighbors = get(
        &state,
        "/entities/dune-2021/neighbors?kind=movie&sort=title",
    )
    .await;
    let other = if neighbors["next"].is_null() {
        &neighbors["prev"]
    } else {
        &neighbors["next"]
    };
    assert_eq!(other["slug"], "dune-part-two-2024", "{neighbors}");
    let studios = get(&state, "/search/studios?q=war").await;
    assert!(studios.is_array());
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn search_without_index_dir_is_503(pool: PgPool) {
    let (state, _) = test_utils::state_with_config(pool, Config::from_env());
    let mut config = (*state.config).clone();
    config.search_index_dir = None;
    let mut state = state;
    state.config = std::sync::Arc::new(config);
    let response =
        test_utils::get(nexus::build_app(state), "/api/v1/catalog/search?kind=movie").await;
    assert_eq!(response.status, 503);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn reindex_switches_generation(pool: PgPool) {
    let (state, dir) = state(pool);
    let reporter = Reporter::silent();
    for _ in 0..3 {
        catalog::works::sync::rebuild(&state, &reporter)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let gens = std::fs::read_dir(&dir.0)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("works.v")
        })
        .count();
    assert_eq!(gens, 2, "остаются текущее и прежнее поколения");
    assert!(dir.0.join("works.current").exists());
    // Защита: пустая БД не переключает индекс на пустой каталог.
    sqlx::query("DELETE FROM entities")
        .execute(&state.db)
        .await
        .unwrap();
    assert!(catalog::works::sync::rebuild(&state, &reporter)
        .await
        .is_err());
    assert_eq!(get(&state, "/search?kind=movie").await["total"], 2);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn painting_and_game_search_with_facets(pool: PgPool) {
    sqlx::raw_sql(
        "INSERT INTO entities (kind, slug, title, metadata) VALUES
           ('painting', 'mona-lisa', 'Мона Лиза', '{\"museum\": \"Лувр\"}'),
           ('painting', 'night-watch', 'Ночной дозор', '{\"museum\": \"Рейксмюсеум\"}'),
           ('game', 'portal-2', 'Portal 2', '{\"publisher\": \"Valve\"}');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (state, _dir) = state(pool.clone());
    catalog::works::sync::rebuild(&state, &Reporter::silent())
        .await
        .unwrap();
    let page = get(
        &state,
        "/search?kind=painting&museum=%D0%9B%D1%83%D0%B2%D1%80",
    )
    .await;
    assert_eq!(slugs(&page), ["mona-lisa"]);
    let museums = get(&state, "/search/museums?kind=painting").await;
    assert_eq!(museums.as_array().unwrap().len(), 2);
    let publishers = get(&state, "/search/publishers?kind=game&q=val").await;
    assert_eq!(publishers[0]["name"], "Valve");
    assert_eq!(
        slugs(&get(&state, "/search?kind=game&q=portal").await),
        ["portal-2"]
    );
}
