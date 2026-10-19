//! Студии: синхронизация из `metadata.studios` и сырья TMDB, страница студии (данные компании из
//! подставного TMDB при первом открытии), фильмы студии в `GET /entities`, ссылки в карточке,
//! новая студия после правки в админке. Данные — `fixtures/catalog.sql`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use test_utils::{request, TestResponse};

const CATALOG: &str = "/api/v1/catalog";

struct Ctx {
    state: AppState,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let mut config = Config::from_env();
        config.dev_login = true;
        // В dev-контейнере ключ TMDB задан: тесты в настоящий TMDB не ходят.
        config.tmdb_api_key = None;
        Self {
            state: test_utils::state_with_config(pool, config).0,
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        request(app, method, &format!("{CATALOG}{path}"), body, token).await
    }

    async fn get_ok(&self, path: &str) -> Value {
        let response = self.send(Method::GET, path, None, None).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {:?}",
            response.json()
        );
        response.json()
    }

    async fn admin(&self) -> String {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            Method::POST,
            "/api/v1/auth/dev/login",
            Some(json!({ "login": "admin@example.com", "role": "admin" })),
            None,
        )
        .await;
        response.json()["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// Подставной TMDB: `GET /company/{id}` — 923 (Legendary) с материнской 174, остальные — 404.
async fn fake_tmdb() -> (String, Arc<Mutex<Vec<i32>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let app = axum::Router::new().route(
        "/company/{id}",
        axum::routing::get(
            move |shared::extract::Path(id): shared::extract::Path<i32>| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(id);
                    match id {
                        923 => (
                            StatusCode::OK,
                            axum::Json(json!({
                                "id": 923,
                                "name": "Legendary Pictures",
                                "description": "American film production company.",
                                "headquarters": "Burbank, California, United States",
                                "homepage": "https://www.legendary.com",
                                "logo_path": "/new-leg.png",
                                "origin_country": "US",
                                "parent_company": { "id": 174, "name": "Warner Bros. Pictures" },
                            })),
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            axum::Json(json!({ "status_code": 34 })),
                        ),
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, requests)
}

fn slugs(page: &Value) -> Vec<&str> {
    let mut slugs: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["slug"].as_str().unwrap())
        .collect();
    slugs.sort();
    slugs
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn studio_pages_and_their_films(pool: PgPool) {
    let mut ctx = Ctx::new(pool);
    sqlx::raw_sql(
        r#"UPDATE entities SET metadata = metadata || '{"studios": ["Legendary Pictures", "Warner Bros. Pictures"]}'
           WHERE slug = 'dune-2021';
           UPDATE entities SET metadata = '{"studios": ["Legendary Pictures"]}' WHERE slug = 'dune-part-two-2024';
           UPDATE entities SET metadata = '{"studios": ["HBO"]}' WHERE slug = 'no-date';"#,
    )
    .execute(&ctx.state.db)
    .await
    .unwrap();

    let company = |id, name: &str, logo: &str| catalog::studios::Company {
        name: name.into(),
        tmdb_id: Some(id),
        logo_path: Some(logo.into()),
        origin_country: Some("US".into()),
    };
    let companies = [
        company(923, "Legendary Pictures", "/leg.png"),
        company(174, "Warner Bros. Pictures", "/wb.png"),
    ];
    assert_eq!(
        catalog::studios::sync(&ctx.state.db, None, &companies)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        catalog::studios::sync(&ctx.state.db, None, &[])
            .await
            .unwrap(),
        0
    );
    let rows: Vec<(String, String, Option<i32>, Option<String>)> =
        sqlx::query_as("SELECT slug, name, tmdb_id, logo_url FROM studios ORDER BY slug")
            .fetch_all(&ctx.state.db)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            ("hbo".into(), "HBO".into(), None, None),
            (
                "legendary-pictures".into(),
                "Legendary Pictures".into(),
                Some(923),
                Some("https://image.tmdb.org/t/p/w300/leg.png".into())
            ),
            (
                "warner-bros-pictures".into(),
                "Warner Bros. Pictures".into(),
                Some(174),
                Some("https://image.tmdb.org/t/p/w300/wb.png".into())
            ),
        ]
    );

    // Карточка: студии по порядку `metadata.studios`, со ссылками.
    let card = ctx.get_ok("/entities/dune-2021").await;
    assert_eq!(
        card["studios"],
        json!([
            { "slug": "legendary-pictures", "name": "Legendary Pictures",
              "logo_url": "https://image.tmdb.org/t/p/w300/leg.png" },
            { "slug": "warner-bros-pictures", "name": "Warner Bros. Pictures",
              "logo_url": "https://image.tmdb.org/t/p/w300/wb.png" },
        ])
    );

    // Фильмы студии — тот же список каталога, с сортировками.
    let page = ctx.get_ok("/entities?studio=legendary-pictures").await;
    assert_eq!(slugs(&page), ["dune-2021", "dune-part-two-2024"]);
    assert_eq!(page["total"], 2);
    let page = ctx
        .get_ok("/entities?studio=legendary-pictures&sort=new&limit=1")
        .await;
    assert_eq!(page["items"][0]["slug"], "dune-part-two-2024");
    assert_eq!(page["total"], 2);
    let page = ctx.get_ok("/entities?studio=nope").await;
    assert_eq!((page["total"].clone(), slugs(&page).len()), (json!(0), 0));

    // Без ключа TMDB — страница из того, что есть.
    let studio = ctx.get_ok("/studios/legendary-pictures").await;
    assert_eq!(studio["name"], "Legendary Pictures");
    assert_eq!(studio["origin_country"], "US");
    assert_eq!(studio["entities_count"], 2);
    assert!(studio["description"].is_null() && studio["parent"].is_null());

    // С ключом — данные компании из TMDB при первом открытии, дальше из БД.
    let (url, requests) = fake_tmdb().await;
    let mut config = (*ctx.state.config).clone();
    config.tmdb_api_key = Some("test-key".into());
    config.tmdb_api_url = url;
    ctx.state.config = Arc::new(config);
    let studio = ctx.get_ok("/studios/legendary-pictures").await;
    assert_eq!(
        studio,
        json!({
            "slug": "legendary-pictures",
            "name": "Legendary Pictures",
            "logo_url": "https://image.tmdb.org/t/p/w300/new-leg.png",
            "origin_country": "US",
            "headquarters": "Burbank, California, United States",
            "homepage": "https://www.legendary.com",
            "description": "American film production company.",
            "parent": { "slug": "warner-bros-pictures", "name": "Warner Bros. Pictures",
                        "logo_url": "https://image.tmdb.org/t/p/w300/wb.png" },
            "entities_count": 2,
        })
    );
    ctx.get_ok("/studios/legendary-pictures").await;
    ctx.get_ok("/studios/hbo").await;
    ctx.get_ok("/studios/warner-bros-pictures").await;
    ctx.get_ok("/studios/warner-bros-pictures").await;
    // Legendary — один раз; HBO без TMDB id — ни разу; Warner — 404 запомнен.
    assert_eq!(*requests.lock().unwrap(), [923, 174]);
    assert_eq!(
        ctx.send(Method::GET, "/studios/nope", None, None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Новая студия из админки — сразу страница и ссылка в карточке.
    let id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM entities WHERE slug = 'dune-part-two-2024'")
            .fetch_one(&ctx.state.db)
            .await
            .unwrap();
    let token = ctx.admin().await;
    let response = ctx
        .send(
            Method::PATCH,
            &format!("/admin/entities/{id}"),
            Some(json!({ "metadata": { "studios": ["Legendary Pictures", "Студия «Ёлка»"] } })),
            Some(&token),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
    let card = ctx.get_ok("/entities/dune-part-two-2024").await;
    let linked: Vec<&str> = card["studios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["slug"].as_str().unwrap())
        .collect();
    assert_eq!(linked, ["legendary-pictures", "studiya-elka"]);
    assert_eq!(
        slugs(&ctx.get_ok("/entities?studio=studiya-elka").await),
        ["dune-part-two-2024"]
    );
}
