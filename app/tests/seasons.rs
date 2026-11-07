//! Сезоны сериалов и их серии: админка (сезон — сущность `kind = season` с сериалом в
//! `parent_id`), три ручки чтения и то, что сезоны не попадают в разделы каталога.
//!
//! Данные — `fixtures/catalog.sql` (сериал `no-date`). Поиск и ISR выключены: здесь проверяется
//! PostgreSQL и HTTP, перестройка индекса — в `catalog.rs`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;
use test_utils::{request, TestResponse};

const CATALOG: &str = "/api/v1/catalog";
const SOCIAL: &str = "/api/v1/social";

struct Ctx {
    state: AppState,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let mut config = Config::from_env();
        config.dev_login = true;
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
        request(app, method, path, body, token).await
    }

    async fn get(&self, path: &str, token: Option<&str>) -> TestResponse {
        self.send(Method::GET, &format!("{CATALOG}{path}"), None, token)
            .await
    }

    async fn get_ok(&self, path: &str, token: Option<&str>) -> Value {
        let response = self.get(path, token).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {:?}",
            response.json()
        );
        response.json()
    }

    async fn token(&self, login: &str, role: &str) -> String {
        let response = self
            .send(
                Method::POST,
                "/api/v1/auth/dev/login",
                Some(json!({ "login": login, "role": role })),
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        response.json()["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Запрос админа к каталогу: статус и тело.
    async fn admin_send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let token = self.token("admin@example.com", "admin").await;
        let response = self
            .send(method, &format!("{CATALOG}{path}"), body, Some(&token))
            .await;
        let body = if response.body.is_empty() {
            Value::Null
        } else {
            response.json()
        };
        (response.status, body)
    }

    /// Сезон сериала `no-date` с номером `number`: id и slug.
    async fn create_season(&self, number: i64, slug: &str, title: &str) -> String {
        let (status, body) = self
            .admin_send(
                Method::POST,
                "/admin/entities",
                Some(json!({
                    "kind": "season",
                    "slug": slug,
                    "title": title,
                    "series_slug": "no-date",
                    "release_date": "2024-11-09",
                    "metadata": { "number": number, "episode_count": 2, "status": "ended" },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        body["id"].as_str().unwrap().to_string()
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn season_is_an_entity_of_its_series(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx.create_season(2, "no-date-season-2", "Сезон 2").await;

    // Карточка сезона: поля сущности, сериал для хлебных крошек, пока без серий.
    let card = ctx.get_ok("/seasons/no-date-season-2", None).await;
    assert_eq!(card["id"], id);
    assert_eq!(card["kind"], "season");
    assert_eq!(card["metadata"]["number"], 2);
    assert_eq!(card["series"]["slug"], "no-date");
    assert_eq!(card["rating"], json!({ "value": null, "count": 0 }));
    assert_eq!(card["my_rating"], Value::Null);
    assert!(card["episodes"].as_array().unwrap().is_empty());

    // Переключатель сезонов в шапке сериала — один запрос.
    let list = ctx.get_ok("/entities/no-date/seasons", None).await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["slug"], "no-date-season-2");
    assert_eq!(items[0]["number"], 2);
    assert_eq!(items[0]["episode_count"], 2);
    assert_eq!(items[0]["status"], "ended");
    assert_eq!(items[0]["premiere_date"], "2024-11-09");
    assert_eq!(items[0]["my_rating"], Value::Null);

    // Сериал не найден — 404, не пустой список.
    assert_eq!(
        ctx.get("/entities/no-such-series/seasons", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        ctx.get("/seasons/no-such-season", None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn seasons_order_specials_last(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.create_season(0, "no-date-season-0", "Спецвыпуски")
        .await;
    ctx.create_season(2, "no-date-season-2", "Сезон 2").await;
    ctx.create_season(1, "no-date-season-1", "Сезон 1").await;

    let list = ctx.get_ok("/entities/no-date/seasons", None).await;
    let numbers: Vec<i64> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["number"].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [1, 2, 0]);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn episodes_are_replaced_as_a_whole(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx.create_season(1, "no-date-season-1", "Сезон 1").await;

    let (status, body) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/seasons/{id}/episodes"),
            Some(json!({ "episodes": [
                {
                    "number": 2, "title": "Вторая", "original_title": "Second",
                    "air_date": "2024-11-16", "runtime_minutes": 42,
                    "still_url": "https://img.example/2.jpg",
                    "still_width": 1280, "still_height": 720,
                },
                { "number": 1, "title": "Первая", "air_date": "2024-11-09" },
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    let page = ctx.get_ok("/seasons/no-date-season-1/episodes", None).await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["limit"], 50);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items[0]["number"], 1);
    assert_eq!(items[0]["title"], "Первая");
    // Кадра нет — null, а не постер сезона: пропорции другие, заглушку ставит фронтенд.
    assert_eq!(items[0]["still_url"], Value::Null);
    assert_eq!(items[1]["number"], 2);
    assert_eq!(items[1]["runtime_minutes"], 42);
    assert_eq!(items[1]["still_width"], 1280);

    // Карточка сезона отдаёт те же серии.
    let card = ctx.get_ok("/seasons/no-date-season-1", None).await;
    assert_eq!(card["episodes"].as_array().unwrap().len(), 2);

    // Повторный PUT заменяет список целиком: лишняя серия снимается.
    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/seasons/{id}/episodes"),
            Some(json!({ "episodes": [{ "number": 1, "title": "Первая" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let page = ctx.get_ok("/seasons/no-date-season-1/episodes", None).await;
    assert_eq!(page["total"], 1);

    // Повтор номера и размеры кадра без самого кадра — 400.
    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/seasons/{id}/episodes"),
            Some(json!({ "episodes": [{ "number": 1 }, { "number": 1 }] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/seasons/{id}/episodes"),
            Some(json!({ "episodes": [{ "number": 1, "still_width": 10 }] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn season_rating_is_its_own(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.create_season(1, "no-date-season-1", "Сезон 1").await;
    let token = ctx.token("viewer@example.com", "user").await;

    // Оценка сезона — обычная социальная ручка по его slug'у, новой не понадобилось.
    let response = ctx
        .send(
            Method::PUT,
            &format!("{SOCIAL}/entities/no-date-season-1/review"),
            Some(json!({ "rating": 9 })),
            Some(&token),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "{:?}",
        response.json()
    );

    let list = ctx.get_ok("/entities/no-date/seasons", Some(&token)).await;
    let season = &list["items"][0];
    assert_eq!(season["rating"]["value"], 9.0);
    assert_eq!(season["rating"]["count"], 1);
    assert_eq!(season["my_rating"], 9);

    // Гостю — средняя есть, своей нет.
    let list = ctx.get_ok("/entities/no-date/seasons", None).await;
    assert_eq!(list["items"][0]["rating"]["value"], 9.0);
    assert_eq!(list["items"][0]["my_rating"], Value::Null);

    // Оценка сериала и оценки сезонов не смешиваются.
    let card = ctx.get_ok("/entities/no-date", None).await;
    assert_eq!(card["slug"], "no-date");
    let rating = ctx
        .send(
            Method::GET,
            &format!("{SOCIAL}/entities/no-date/rating"),
            None,
            None,
        )
        .await
        .json();
    assert_eq!(rating["count"], 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn seasons_stay_out_of_the_catalog(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.create_season(1, "no-date-season-1", "Сезон 1").await;

    // В разделе «Сериалы» — сам сериал, а не его сезоны.
    let page = ctx.get_ok("/entities?kind=series", None).await;
    let slugs: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["no-date"]);
    assert_eq!(ctx.get_ok("/entities?kind=season", None).await["total"], 0);

    // Админский список их показывает: находить и править сезоны надо.
    let (status, body) = ctx
        .admin_send(Method::GET, "/admin/entities?kind=season", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn season_needs_a_series(pool: PgPool) {
    let ctx = Ctx::new(pool);

    // Без сериала сезона не бывает.
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({
                "kind": "season", "slug": "orphan-season-1", "title": "Сезон 1",
                "metadata": { "number": 1 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Родителем может быть только сериал, и только у сезона.
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({
                "kind": "season", "slug": "wrong-season-1", "title": "Сезон 1",
                "series_slug": "dune-2021", "metadata": { "number": 1 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({
                "kind": "movie", "slug": "with-parent", "title": "Фильм",
                "series_slug": "no-date",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Номер сезона обязателен.
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({
                "kind": "season", "slug": "no-number", "title": "Сезон",
                "series_slug": "no-date", "metadata": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn deleting_a_series_takes_its_seasons(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let season_id = ctx.create_season(1, "no-date-season-1", "Сезон 1").await;
    let (_, series) = ctx.admin_send(Method::GET, "/entities/no-date", None).await;
    let series_id = series["id"].as_str().unwrap().to_string();

    ctx.admin_send(
        Method::PUT,
        &format!("/admin/seasons/{season_id}/episodes"),
        Some(json!({ "episodes": [{ "number": 1, "title": "Первая" }] })),
    )
    .await;

    let (status, _) = ctx
        .admin_send(
            Method::DELETE,
            &format!("/admin/entities/{series_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        ctx.get("/seasons/no-date-season-1", None).await.status,
        StatusCode::NOT_FOUND
    );
}
