//! Поиск людей на Tantivy (`catalog::people_idx`): выдача `GET /search/people` идёт из индекса в `SEARCH_INDEX_DIR`.

use catalog::jobs::Reporter;
use serde_json::{json, Value};
use shared::Config;
use sqlx::PgPool;
use std::path::PathBuf;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn slugs(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["slug"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn people_search_is_answered_by_tantivy(pool: PgPool) {
    let dir = Dir(std::env::temp_dir().join(format!("nexus-people-{}", uuid::Uuid::new_v4())));
    let mut config = Config::from_env();
    config.search_index_dir = Some(dir.0.to_string_lossy().into_owned());
    let (state, _) = test_utils::state_with_config(pool.clone(), config);

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

    let built = catalog::people_idx::sync::rebuild(&state, &Reporter::silent())
        .await
        .unwrap();
    assert_eq!(built, 4, "«nobody» без работ в индекс не попадает");

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
