//! Единый индекс каталога (`catalog::index`): книги, работы и люди в одном индексе
//! `SEARCH_INDEX_DIR/catalog`; выдача, фильтры, соседи, подсказки, перестройка и люди.

use catalog::jobs::Reporter;
use serde_json::{json, Value};
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
    let built = catalog::index::sync::rebuild(
        &state,
        catalog::index::sync::Target::All,
        &Reporter::silent(),
        false,
    )
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
async fn rebuild_is_one_commit_and_the_size_guard_keeps_the_old_index(pool: PgPool) {
    let (mut state, dir) = state(pool);
    let mut config = (*state.config).clone();
    config.search_reindex_min_percent = 80;
    state.config = std::sync::Arc::new(config);
    let reporter = Reporter::silent();
    for _ in 0..2 {
        catalog::index::sync::rebuild(&state, catalog::index::sync::Target::All, &reporter, false)
            .await
            .unwrap();
    }
    // Один каталог без поколений.
    assert!(dir.0.join("catalog").is_dir());
    assert!(std::fs::read_dir(&dir.0).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".v")));
    // Защита: пустая БД не заменяет индекс пустым — коммита нет, поиск читает прежнее.
    sqlx::query("DELETE FROM entities")
        .execute(&state.db)
        .await
        .unwrap();
    assert!(catalog::index::sync::rebuild(
        &state,
        catalog::index::sync::Target::All,
        &reporter,
        false,
    )
    .await
    .is_err());
    assert_eq!(get(&state, "/search?kind=movie").await["total"], 2);
}

/// Вид перестраивается отдельно: остальные документы остаются.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn one_kind_can_be_rebuilt_alone(pool: PgPool) {
    let (state, _dir) = state(pool);
    let reporter = Reporter::silent();
    catalog::index::sync::rebuild(&state, catalog::index::sync::Target::All, &reporter, false)
        .await
        .unwrap();
    let index = catalog::index::sync::open(&state).unwrap().unwrap();
    let movies = index.kind_docs("movie");
    let people = index.kind_docs("person");
    assert!(movies >= 2 && people > 0);
    catalog::index::sync::rebuild(
        &state,
        catalog::index::sync::Target::Kinds(vec![catalog::models::EntityKind::Game]),
        &Reporter::silent(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(index.kind_docs("movie"), movies);
    assert_eq!(index.kind_docs("person"), people);
    assert_eq!(get(&state, "/search?kind=game").await["total"], 1);
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
    catalog::index::sync::rebuild(
        &state,
        catalog::index::sync::Target::All,
        &Reporter::silent(),
        false,
    )
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

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn people_search_is_answered_by_the_catalog_index(pool: PgPool) {
    let (state, _dir) = state(pool.clone());

    sqlx::raw_sql(
        "INSERT INTO entity_stats (entity_id, tmdb_votes)
         SELECT id, v.votes FROM entities e
         JOIN (VALUES ('dune-2021', 1000), ('dune-part-two-2024', 500))
             AS v(slug, votes) ON v.slug = e.slug;
         UPDATE people SET original_name = 'Denis Villeneuve' WHERE slug = 'denis-villeneuve';
         INSERT INTO people (slug, full_name) VALUES ('denis-extra', 'Дени Статист');
         INSERT INTO entity_credits (entity_id, person_id, role, position)
         SELECT e.id, p.id, 'actor', 5 FROM entities e, people p
         WHERE e.slug = 'witcher-3' AND p.slug = 'denis-extra';",
    )
    .execute(&pool)
    .await
    .unwrap();

    catalog::index::sync::rebuild(
        &state,
        catalog::index::sync::Target::People,
        &Reporter::silent(),
        false,
    )
    .await
    .unwrap();
    let index = catalog::index::sync::open(&state).unwrap().unwrap();
    assert_eq!(
        index.kind_docs("person"),
        4,
        "«nobody» без работ в индекс не попадает"
    );
    // Слой Б людей собран тем же проходом: люди ищутся, не дожидаясь пересборки.

    let get = |uri: String| {
        let app = nexus::build_app(state.clone());
        async move {
            let response = test_utils::get(app, &format!("/api/v1/catalog{uri}")).await;
            assert_eq!(response.status, 200);
            response.json()
        }
    };

    // Одинаково подходящие «Дени» — по известности; латиница и опечатка находят человека.
    let page = get("/search/people?q=%D0%B4%D0%B5%D0%BD%D0%B8".into()).await;
    assert_eq!(slugs(&page), ["denis-villeneuve", "denis-extra"]);
    assert_eq!(page["items"][0]["original_name"], "Denis Villeneuve");
    assert!(page["items"][0].get("fame").is_none(), "{page}");
    assert_eq!(
        slugs(&get("/search/people?q=villenuve".into()).await),
        ["denis-villeneuve"]
    );

    // Без запроса — чередование ролей; счётчики ролей.
    let page = get("/search/people?limit=3".into()).await;
    assert_eq!(
        slugs(&page),
        ["timothee-chalamet", "denis-villeneuve", "frank-herbert"]
    );
    assert_eq!(page["total"], 4);
    assert_eq!(
        page["facets"]["known_for"],
        json!({ "actor": 2, "author": 1, "director": 1 })
    );
    let rest = get("/search/people?limit=3&offset=3".into()).await;
    assert_eq!(slugs(&rest), ["denis-extra"]);
    let actors = get("/search/people?known_for=actor".into()).await;
    assert_eq!(actors["total"], actors["facets"]["known_for"]["actor"]);
}
