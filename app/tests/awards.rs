//! Награды: публичный раздел (список премий, премия с годами, церемония с категориями),
//! админка (премии, церемонии, категории, замена номинаций целиком) и блок «Награды» на
//! карточке работы и в профиле человека. Данные — `fixtures/catalog.sql`.
//!
//! Meilisearch здесь не нужен: всё читается из PostgreSQL. Фасеты индекса проверяет
//! `catalog.rs` вместе с остальным поиском.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;
use test_utils::{request, TestResponse};

const CATALOG: &str = "/api/v1/catalog";

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
        request(app, method, &format!("{CATALOG}{path}"), body, token).await
    }

    async fn get(&self, path: &str) -> TestResponse {
        self.send(Method::GET, path, None, None).await
    }

    async fn get_ok(&self, path: &str) -> Value {
        let response = self.get(path).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {:?}",
            response.json()
        );
        response.json()
    }

    async fn token(&self, login: &str, role: &str) -> String {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            Method::POST,
            "/api/v1/auth/dev/login",
            Some(json!({ "login": login, "role": role })),
            None,
        )
        .await;
        response.json()["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Запрос админом: статус и тело (пустое — `null`).
    async fn admin_send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let token = self.token("admin@example.com", "admin").await;
        let response = self.send(method, path, body, Some(&token)).await;
        let json = if response.body.is_empty() {
            Value::Null
        } else {
            response.json()
        };
        (response.status, json)
    }

    /// Запрос админом, ожидающий успеха.
    async fn admin_ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, json) = self.admin_send(method, path, body).await;
        assert!(status.is_success(), "{path}: {status} {json:?}");
        json
    }

    /// «Оскар» с категориями и церемонией 2022 года.
    async fn oscar(&self) {
        self.admin_ok(
            Method::POST,
            "/admin/awards",
            Some(json!({
                "slug": "oscar",
                "title": "Оскар",
                "full_title": "Премия Американской киноакадемии",
                "kinds": ["movie"],
                "country": "США",
                "founded": 1929,
                "position": 100,
            })),
        )
        .await;
        self.admin_ok(
            Method::POST,
            "/admin/awards/oscar/editions",
            Some(json!({ "year": 2022, "season_year": 2021, "number": 94,
                         "place": "Dolby Theatre" })),
        )
        .await;
    }
}

/// Раздел целиком: премия заводится в админке, церемония наполняется одним PUT, читается гостем.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn awards_section(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.oscar().await;

    // Пустая премия уже видна списком: с числом церемоний и годами для плитки.
    let list = ctx.get_ok("/awards").await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["slug"], "oscar");
    assert_eq!(list["items"][0]["editions_count"], 1);
    assert_eq!(list["items"][0]["first_year"], 2022);
    assert_eq!(list["items"][0]["latest_year"], 2022);

    // Номинации церемонии: актёрская — это и работа, и человек сразу.
    ctx.admin_ok(
        Method::PUT,
        "/admin/awards/oscar/2022/nominations",
        Some(json!({ "categories": [
            { "slug": "best-picture", "title": "Лучший фильм", "position": 0,
              "nominations": [
                { "entity_slug": "dune-2021", "won": true },
                { "entity_slug": "dune-part-two-2024" },
                { "name": "Власть пса" },
              ] },
            { "slug": "best-actor", "title": "Лучшая мужская роль", "position": 1,
              "nominations": [
                { "entity_slug": "dune-2021", "person_slug": "timothee-chalamet",
                  "role": "Пол Атрейдес", "won": true },
              ] },
        ] })),
    )
    .await;

    // Страница премии: церемонии с числом номинаций и победителями главной категории.
    let award = ctx.get_ok("/awards/oscar").await;
    assert_eq!(award["title"], "Оскар");
    assert_eq!(award["kinds"], json!(["movie"]));
    let editions = award["editions"].as_array().unwrap();
    assert_eq!(editions.len(), 1);
    assert_eq!(editions[0]["year"], 2022);
    assert_eq!(editions[0]["season_year"], 2021);
    assert_eq!(editions[0]["nominations_count"], 4);
    assert_eq!(editions[0]["top"][0]["slug"], "dune-2021");
    assert_eq!(
        editions[0]["top"].as_array().unwrap().len(),
        1,
        "две победы одной работы — одна плитка в превью"
    );

    // Страница церемонии: категории по порядку, победители первыми, с постером и фото.
    let edition = ctx.get_ok("/awards/oscar/2022").await;
    assert_eq!(edition["award"]["slug"], "oscar");
    assert_eq!(edition["place"], "Dolby Theatre");
    let categories = edition["categories"].as_array().unwrap();
    assert_eq!(categories.len(), 2);
    assert_eq!(categories[0]["slug"], "best-picture");
    let nominations = categories[0]["nominations"].as_array().unwrap();
    assert_eq!(nominations.len(), 3);
    assert_eq!(nominations[0]["won"], true, "победитель первым");
    assert_eq!(nominations[0]["entity"]["slug"], "dune-2021");
    assert!(nominations[0]["entity"].get("cover_url").is_some());
    assert_eq!(
        nominations[2]["name"], "Власть пса",
        "работа не из каталога — именем"
    );
    assert_eq!(nominations[2]["entity"], Value::Null);
    let actor = &categories[1]["nominations"][0];
    assert_eq!(actor["person"]["slug"], "timothee-chalamet");
    assert_eq!(actor["entity"]["slug"], "dune-2021");
    assert_eq!(actor["role"], "Пол Атрейдес");

    // Вкладки раздела: премия только для фильмов.
    assert_eq!(ctx.get_ok("/awards?kind=movie").await["total"], 1);
    assert_eq!(ctx.get_ok("/awards?kind=game").await["total"], 0);
    assert_eq!(ctx.get_ok("/awards?q=кинокадем").await["total"], 0);
    assert_eq!(ctx.get_ok("/awards?q=оскар").await["total"], 1);
    assert_eq!(
        ctx.get("/awards?kind=comic").await.status,
        StatusCode::BAD_REQUEST
    );

    // Нет премии или года — 404.
    assert_eq!(ctx.get("/awards/nobel").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        ctx.get("/awards/oscar/1999").await.status,
        StatusCode::NOT_FOUND
    );
}

/// Награды на карточке работы и в профиле человека — ссылками на церемонию.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn awards_on_cards(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.oscar().await;

    // Без номинаций — пустой список, а не отсутствующее поле.
    assert_eq!(ctx.get_ok("/entities/dune-2021").await["awards"], json!([]));
    assert_eq!(
        ctx.get_ok("/people/timothee-chalamet").await["awards"],
        json!([])
    );

    ctx.admin_ok(
        Method::PUT,
        "/admin/awards/oscar/2022/nominations",
        Some(json!({ "categories": [
            { "slug": "best-picture", "title": "Лучший фильм",
              "nominations": [{ "entity_slug": "dune-2021", "won": true }] },
            { "slug": "best-actor", "title": "Лучшая мужская роль", "position": 1,
              "nominations": [{ "entity_slug": "dune-2021",
                                "person_slug": "timothee-chalamet" }] },
        ] })),
    )
    .await;

    let card = ctx.get_ok("/entities/dune-2021").await;
    let awards = card["awards"].as_array().unwrap();
    assert_eq!(awards.len(), 2);
    assert_eq!(awards[0]["won"], true, "полученные сверху");
    assert_eq!(awards[0]["award"]["slug"], "oscar");
    assert_eq!(awards[0]["award"]["title"], "Оскар");
    assert_eq!(awards[0]["year"], 2022);
    assert_eq!(awards[0]["category"]["slug"], "best-picture");
    assert_eq!(awards[0]["category"]["title"], "Лучший фильм");

    // В профиле человека — с работой, за которую он номинирован.
    let person = ctx.get_ok("/people/timothee-chalamet").await;
    let awards = person["awards"].as_array().unwrap();
    assert_eq!(awards.len(), 1);
    assert_eq!(awards[0]["won"], false);
    assert_eq!(awards[0]["category"]["slug"], "best-actor");
    assert_eq!(awards[0]["entity"]["slug"], "dune-2021");

    // Замена церемонии снимает номинацию — карточка пустеет.
    ctx.admin_ok(
        Method::PUT,
        "/admin/awards/oscar/2022/nominations",
        Some(json!({ "categories": [] })),
    )
    .await;
    assert_eq!(ctx.get_ok("/entities/dune-2021").await["awards"], json!([]));
}

/// Админка: права, проверки полей и удаление премии с номинациями только по `?force=true`.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn awards_admin(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.oscar().await;

    // Без токена — 401, не админ — 403.
    let response = ctx
        .send(Method::POST, "/admin/awards", Some(json!({})), None)
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    let user = ctx.token("user@example.com", "user").await;
    let response = ctx
        .send(
            Method::DELETE,
            "/admin/awards/oscar",
            None,
            Some(user.as_str()),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    // Проверки полей и занятый slug.
    let (status, body) = ctx
        .admin_send(
            Method::POST,
            "/admin/awards",
            Some(json!({ "slug": "Оскар", "title": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/awards",
            Some(json!({ "slug": "oscar", "title": "Оскар" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/awards/oscar/editions",
            Some(json!({ "year": 1700 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/awards/oscar/editions",
            Some(json!({ "year": 2022 })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "этот год уже есть");

    // Неизвестная работа в номинации — 400, а не 500.
    let (status, body) = ctx
        .admin_send(
            Method::PUT,
            "/admin/awards/oscar/2022/nominations",
            Some(json!({ "categories": [
                { "slug": "best-picture", "title": "Лучший фильм",
                  "nominations": [{ "entity_slug": "no-such-film" }] },
            ] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // Категории, которой нет, без названия не завести.
    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            "/admin/awards/oscar/2022/nominations",
            Some(json!({ "categories": [
                { "slug": "best-sound", "nominations": [{ "name": "Дюна" }] },
            ] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    ctx.admin_ok(
        Method::PUT,
        "/admin/awards/oscar/2022/nominations",
        Some(json!({ "categories": [
            { "slug": "best-picture", "title": "Лучший фильм",
              "nominations": [{ "entity_slug": "dune-2021", "won": true }] },
        ] })),
    )
    .await;

    // Правка премии и категории.
    let award = ctx
        .admin_ok(
            Method::PATCH,
            "/admin/awards/oscar",
            Some(json!({ "kinds": ["movie", "series"], "country": null })),
        )
        .await;
    assert_eq!(award["kinds"], json!(["movie", "series"]));
    assert_eq!(award["country"], Value::Null);
    let category = ctx
        .admin_ok(
            Method::PATCH,
            "/admin/awards/oscar/categories/best-picture",
            Some(json!({ "title": "Лучший фильм года" })),
        )
        .await;
    assert_eq!(category["title"], "Лучший фильм года");
    assert_eq!(
        ctx.get_ok("/entities/dune-2021").await["awards"][0]["category"]["title"],
        "Лучший фильм года"
    );

    // Категорию с номинациями не удалить, премию с ними — только с ?force=true.
    let (status, _) = ctx
        .admin_send(
            Method::DELETE,
            "/admin/awards/oscar/categories/best-picture",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = ctx
        .admin_send(Method::DELETE, "/admin/awards/oscar", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = ctx
        .admin_send(Method::DELETE, "/admin/awards/oscar?force=true", None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(ctx.get_ok("/awards").await["total"], 0);
    assert_eq!(ctx.get_ok("/entities/dune-2021").await["awards"], json!([]));

    // Нет премии — 404.
    let (status, _) = ctx
        .admin_send(Method::PATCH, "/admin/awards/oscar", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
