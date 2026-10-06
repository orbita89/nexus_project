//! Каталог: чтение без авторизации, админка, поиск через Meilisearch.
//!
//! Данные — `fixtures/catalog.sql`. Тесты поиска ходят в настоящий Meilisearch (`MEILI_URL`,
//! `MEILI_MASTER_KEY`, поднимается `make up`), у каждого теста свой префикс индексов.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;
use std::time::Duration;
use test_utils::{request, TestResponse};

const CATALOG: &str = "/api/v1/catalog";

struct Ctx {
    state: AppState,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let mut config = Config::from_env();
        config.dev_login = true;
        let (state, _) = test_utils::state_with_config(pool, config);
        Self { state }
    }

    fn with_search(pool: PgPool) -> Self {
        let ctx = Self::new(pool);
        Self {
            state: test_utils::with_search(ctx.state),
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

    /// GET, ожидающий 200: тело ответа.
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
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        response.json()["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn admin(&self) -> String {
        self.token("admin@example.com", "admin").await
    }

    /// Запрос от админа: статус и тело (`null`, если тела нет).
    async fn admin_send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let token = self.admin().await;
        let response = self.send(method, path, body, Some(&token)).await;
        let body = if response.body.is_empty() {
            Value::Null
        } else {
            response.json()
        };
        (response.status, body)
    }

    async fn entity_id(&self, slug: &str) -> String {
        self.get_ok(&format!("/entities/{slug}")).await["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn person_id(&self, slug: &str) -> String {
        self.get_ok(&format!("/people/{slug}")).await["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn tag_id(&self, slug: &str) -> String {
        let tags = self.get_ok("/tags").await;
        tags.as_array()
            .unwrap()
            .iter()
            .find(|t| t["slug"] == slug)
            .unwrap_or_else(|| panic!("no tag {slug}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Удаляет индексы теста в Meilisearch.
    async fn drop_search_index(&self) {
        let index = self.state.search.index(catalog::search::INDEX);
        let _ = self
            .state
            .search
            .call(Method::DELETE, &format!("/indexes/{index}"), None)
            .await;
    }

    /// Ждёт, пока поиск вернёт ожидаемое: индексация в Meilisearch асинхронная.
    async fn search_until(&self, path: &str, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..100 {
            let body = self.get_ok(path).await;
            if done(&body) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "search {path} did not converge: {}",
            self.get_ok(path).await
        );
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

// ---------------------------------------------------------------- чтение

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn list_entities_newest_first_with_pagination(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let page = ctx.get_ok("/entities").await;
    assert_eq!(page["total"], 5);
    assert_eq!(page["limit"], 20);
    assert_eq!(page["offset"], 0);
    assert_eq!(
        slugs(&page),
        [
            "dune-part-two-2024",
            "dune-2021",
            "witcher-3",
            "dune-novel",
            "no-date"
        ]
    );
    let first = &page["items"][0];
    assert_eq!(first["kind"], "movie");
    assert_eq!(first["title"], "Дюна: Часть вторая");
    assert_eq!(first["release_date"], "2024-03-01");

    let page = ctx.get_ok("/entities?limit=2&offset=1").await;
    assert_eq!(page["total"], 5);
    assert_eq!(slugs(&page), ["dune-2021", "witcher-3"]);

    // limit ограничивается 1..=100, offset не меньше 0.
    let page = ctx.get_ok("/entities?limit=1000&offset=-5").await;
    assert_eq!(page["limit"], 100);
    assert_eq!(page["offset"], 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn list_entities_filters(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let page = ctx.get_ok("/entities?kind=movie").await;
    assert_eq!(slugs(&page), ["dune-part-two-2024", "dune-2021"]);
    assert_eq!(page["total"], 2);

    let page = ctx.get_ok("/entities?tag=sci-fi").await;
    assert_eq!(
        slugs(&page),
        ["dune-part-two-2024", "dune-2021", "dune-novel"]
    );

    let page = ctx.get_ok("/entities?tag=sci-fi&kind=book").await;
    assert_eq!(slugs(&page), ["dune-novel"]);

    let page = ctx.get_ok("/entities?year=2021").await;
    assert_eq!(slugs(&page), ["dune-2021"]);

    // Подстрока без учёта регистра, в названии или оригинальном названии.
    let page = ctx.get_ok("/entities?q=%D0%94%D0%AE%D0%9D").await; // «ДЮН»
    assert_eq!(page["total"], 3);
    let page = ctx.get_ok("/entities?q=witcher").await;
    assert_eq!(slugs(&page), ["witcher-3"]);
    // % — обычный символ, а не шаблон LIKE.
    let page = ctx.get_ok("/entities?q=100%25").await;
    assert_eq!(slugs(&page), ["no-date"]);
    let page = ctx.get_ok("/entities?q=%25").await;
    assert_eq!(slugs(&page), ["no-date"]);

    let page = ctx.get_ok("/entities?tag=no-such-tag").await;
    assert_eq!(page["total"], 0);
    assert_eq!(page["items"], json!([]));
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn list_entities_rejects_bad_filters(pool: PgPool) {
    let ctx = Ctx::new(pool);

    assert_eq!(
        ctx.get("/entities?kind=comic").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        ctx.get("/entities?year=0").await.status,
        StatusCode::BAD_REQUEST
    );
    // Ошибка разбора query — тоже {"error": "..."}.
    let response = ctx.get("/entities?year=abc").await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.json()["error"].is_string());
    let response = ctx.get("/entities?kind=comic").await;
    assert!(response.json()["error"].as_str().unwrap().contains("kind"));
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn entity_card_with_tags_and_credits(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let card = ctx.get_ok("/entities/dune-2021").await;
    assert_eq!(card["kind"], "movie");
    assert_eq!(card["title"], "Дюна");
    assert_eq!(card["original_title"], "Dune");
    assert_eq!(card["metadata"], json!({ "runtime_min": 155 }));
    assert!(card["created_at"].is_string());
    assert_eq!(card["tags"].as_array().unwrap().len(), 1);
    assert_eq!(card["tags"][0]["slug"], "sci-fi");
    assert_eq!(card["tags"][0]["name"], "Научная фантастика");

    // Титры по position: режиссёр (0) раньше актёра (1).
    let credits = card["credits"].as_array().unwrap();
    assert_eq!(credits.len(), 2);
    assert_eq!(credits[0]["role"], "director");
    assert_eq!(credits[0]["character_name"], Value::Null);
    assert_eq!(credits[0]["person"]["slug"], "denis-villeneuve");
    assert_eq!(credits[1]["role"], "actor");
    assert_eq!(credits[1]["character_name"], "Пол Атрейдес");
    assert_eq!(credits[1]["person"]["full_name"], "Тимоти Шаламе");

    let response = ctx.get("/entities/no-such-entity").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.json()["error"], "not found");
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn people_list_and_search(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let page = ctx.get_ok("/people").await;
    assert_eq!(page["total"], 4);
    // По алфавиту.
    assert_eq!(
        slugs(&page),
        [
            "denis-villeneuve",
            "nobody",
            "timothee-chalamet",
            "frank-herbert"
        ]
    );
    assert_eq!(page["items"][0]["birth_date"], "1967-10-03");

    let page = ctx.get_ok("/people?q=%D0%B2%D0%B8%D0%BB%D1%8C").await; // «виль»
    assert_eq!(slugs(&page), ["denis-villeneuve"]);

    let page = ctx.get_ok("/people?limit=1&offset=1").await;
    assert_eq!(page["total"], 4);
    assert_eq!(slugs(&page), ["nobody"]);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn person_card_with_filmography(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let card = ctx.get_ok("/people/denis-villeneuve").await;
    assert_eq!(card["full_name"], "Дени Вильнёв");
    let credits = card["credits"].as_array().unwrap();
    // Новые работы сверху.
    assert_eq!(credits.len(), 2);
    assert_eq!(credits[0]["entity"]["slug"], "dune-part-two-2024");
    assert_eq!(credits[0]["role"], "director");
    assert_eq!(credits[1]["entity"]["slug"], "dune-2021");
    assert_eq!(credits[1]["entity"]["kind"], "movie");

    let card = ctx.get_ok("/people/nobody").await;
    assert_eq!(card["credits"], json!([]));

    assert_eq!(
        ctx.get("/people/no-such-person").await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn tags_list_with_counts(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let tags = ctx.get_ok("/tags").await;
    let tags: Vec<(&str, i64)> = tags
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["slug"].as_str().unwrap(),
                t["entities_count"].as_i64().unwrap(),
            )
        })
        .collect();
    // По названию.
    assert_eq!(tags, [("unused", 0), ("sci-fi", 3), ("fantasy", 1)]);
}

// ---------------------------------------------------------------- права

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_endpoints_require_admin(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let user = ctx.token("reader@example.com", "user").await;
    let author = ctx.token("writer@example.com", "author").await;
    let id = ctx.entity_id("dune-2021").await;
    let any = "00000000-0000-4000-8000-000000000000";

    let endpoints = [
        (Method::POST, "/admin/entities".to_string()),
        (Method::PATCH, format!("/admin/entities/{id}")),
        (Method::DELETE, format!("/admin/entities/{id}")),
        (Method::PUT, format!("/admin/entities/{id}/tags")),
        (Method::POST, format!("/admin/entities/{id}/credits")),
        (
            Method::DELETE,
            format!("/admin/entities/{id}/credits/{any}"),
        ),
        (Method::POST, "/admin/people".to_string()),
        (Method::PATCH, format!("/admin/people/{any}")),
        (Method::DELETE, format!("/admin/people/{any}")),
        (Method::POST, "/admin/tags".to_string()),
        (Method::PATCH, format!("/admin/tags/{any}")),
        (Method::DELETE, format!("/admin/tags/{any}")),
        (Method::POST, "/admin/search/reindex".to_string()),
    ];
    for (method, path) in endpoints {
        let body = Some(json!({}));
        let response = ctx.send(method.clone(), &path, body.clone(), None).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{method} {path}");
        for token in [&user, &author] {
            let response = ctx
                .send(method.clone(), &path, body.clone(), Some(token))
                .await;
            assert_eq!(response.status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    // Ничего не изменилось.
    assert_eq!(ctx.get_ok("/entities").await["total"], 5);
}

// ---------------------------------------------------------------- админка: сущности

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn create_entity(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let (status, card) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({
                "kind": "book",
                "slug": "dune-messiah",
                "title": "  Мессия Дюны ",
                "original_title": "Dune Messiah",
                "description": "",
                "release_date": "1969-10-15",
                "cover_url": "https://img.example/messiah.jpg",
                "metadata": { "isbn": "978-0-441-01359-3", "pages": 256 },
                "tags": ["sci-fi", "fantasy", "sci-fi"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{card}");
    assert_eq!(card["title"], "Мессия Дюны");
    assert_eq!(card["description"], Value::Null);
    // ISBN хранится без дефисов.
    assert_eq!(
        card["metadata"],
        json!({ "isbn": "9780441013593", "pages": 256 })
    );
    assert_eq!(card["tags"].as_array().unwrap().len(), 2);
    assert_eq!(card["credits"], json!([]));

    let fetched = ctx.get_ok("/entities/dune-messiah").await;
    assert_eq!(fetched["id"], card["id"]);
    assert_eq!(fetched["cover_url"], "https://img.example/messiah.jpg");

    // Минимальный набор полей: metadata по умолчанию {}.
    let (status, card) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({ "kind": "game", "slug": "minimal", "title": "Минимум" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{card}");
    assert_eq!(card["metadata"], json!({}));
    assert_eq!(card["tags"], json!([]));
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn create_entity_validates(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let base = json!({ "kind": "movie", "slug": "new-movie", "title": "Фильм" });
    let with = |patch: Value| {
        let mut body = base.clone();
        body.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        body
    };

    let cases = [
        (json!({ "slug": "New Movie" }), "slug"),
        (json!({ "title": "   " }), "title"),
        (json!({ "cover_url": "javascript:alert(1)" }), "cover_url"),
        (json!({ "metadata": { "pages": 100 } }), "unknown field"),
        (
            json!({ "metadata": { "runtime_min": "долго" } }),
            "invalid metadata",
        ),
        (json!({ "metadata": { "runtime_min": 0 } }), "runtime_min"),
        (json!({ "metadata": { "countries": ["USA"] } }), "countries"),
        (json!({ "metadata": "movie" }), "object"),
        (
            json!({ "tags": ["sci-fi", "nope", "nada"] }),
            "unknown tags: nada, nope",
        ),
    ];
    for (patch, expected) in cases {
        let (status, body) = ctx
            .admin_send(Method::POST, "/admin/entities", Some(with(patch.clone())))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{patch}: {body}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(expected), "{patch}: {error}");
    }
    // Неизвестный kind и лишние поля отсекает разбор JSON: тоже 400 в общем формате.
    for (patch, expected) in [
        (json!({ "kind": "comic" }), "unknown variant `comic`"),
        (json!({ "rating": 10 }), "unknown field `rating`"),
    ] {
        let (status, body) = ctx
            .admin_send(Method::POST, "/admin/entities", Some(with(patch.clone())))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{patch}");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(expected), "{patch}: {error}");
    }
    // Ничего не создано, в том числе при ошибке в тегах (транзакция откатилась).
    assert_eq!(
        ctx.get("/entities/new-movie").await.status,
        StatusCode::NOT_FOUND
    );

    let (status, body) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(with(json!({ "slug": "dune-2021" }))),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "slug is already taken");
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn update_entity(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx.entity_id("dune-2021").await;
    let path = format!("/admin/entities/{id}");

    // Не переданные поля не меняются, null очищает.
    let (status, card) = ctx
        .admin_send(
            Method::PATCH,
            &path,
            Some(json!({
                "title": "Дюна (2021)",
                "original_title": null,
                "metadata": { "runtime_min": 156, "age_rating": "PG-13" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{card}");
    assert_eq!(card["title"], "Дюна (2021)");
    assert_eq!(card["original_title"], Value::Null);
    assert_eq!(card["release_date"], "2021-10-22");
    assert_eq!(
        card["metadata"],
        json!({ "runtime_min": 156, "age_rating": "PG-13" })
    );
    assert_eq!(card["credits"].as_array().unwrap().len(), 2);

    let (status, card) = ctx
        .admin_send(
            Method::PATCH,
            &path,
            Some(json!({ "slug": "dune-part-one" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(card["title"], "Дюна (2021)");
    assert_eq!(
        ctx.get("/entities/dune-2021").await.status,
        StatusCode::NOT_FOUND
    );
    ctx.get_ok("/entities/dune-part-one").await;

    // metadata проверяется по kind сущности (movie), kind не меняется.
    let (status, _) = ctx
        .admin_send(
            Method::PATCH,
            &path,
            Some(json!({ "metadata": { "pages": 1 } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = ctx
        .admin_send(Method::PATCH, &path, Some(json!({ "kind": "book" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = ctx
        .admin_send(Method::PATCH, &path, Some(json!({ "slug": "dune-novel" })))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = ctx
        .admin_send(
            Method::PATCH,
            "/admin/entities/00000000-0000-4000-8000-000000000000",
            Some(json!({ "title": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn delete_entity(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx.entity_id("dune-2021").await;

    let (status, body) = ctx
        .admin_send(Method::DELETE, &format!("/admin/entities/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
    assert_eq!(
        ctx.get("/entities/dune-2021").await.status,
        StatusCode::NOT_FOUND
    );
    // Участие удалено каскадно.
    let card = ctx.get_ok("/people/timothee-chalamet").await;
    assert_eq!(card["credits"], json!([]));

    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("/admin/entities/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn set_entity_tags(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx.entity_id("dune-2021").await;
    let path = format!("/admin/entities/{id}/tags");

    let (status, tags) = ctx
        .admin_send(
            Method::PUT,
            &path,
            Some(json!({ "tags": ["fantasy", "unused"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tags}");
    let tags: Vec<&str> = tags
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["slug"].as_str().unwrap())
        .collect();
    assert_eq!(tags, ["unused", "fantasy"]);

    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            &path,
            Some(json!({ "tags": ["fantasy", "nope"] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Набор не изменился.
    assert_eq!(
        ctx.get_ok("/entities/dune-2021").await["tags"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (status, tags) = ctx
        .admin_send(Method::PUT, &path, Some(json!({ "tags": [] })))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tags, json!([]));

    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            "/admin/entities/00000000-0000-4000-8000-000000000000/tags",
            Some(json!({ "tags": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn add_and_delete_credits(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let entity = ctx.entity_id("dune-part-two-2024").await;
    let person = ctx.person_id("timothee-chalamet").await;
    let path = format!("/admin/entities/{entity}/credits");
    let credit = json!({ "person_id": person, "role": "actor", "character_name": "Пол Атрейдес", "position": 1 });

    let (status, added) = ctx
        .admin_send(Method::POST, &path, Some(credit.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["role"], "actor");
    assert_eq!(added["person"]["slug"], "timothee-chalamet");
    let card = ctx.get_ok("/entities/dune-part-two-2024").await;
    assert_eq!(card["credits"][1]["id"], added["id"]);

    let (status, _) = ctx
        .admin_send(Method::POST, &path, Some(credit.clone()))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Роль без персонажа тоже не дублируется (UNIQUE NULLS NOT DISTINCT).
    let director = json!({ "person_id": person, "role": "director" });
    let (status, _) = ctx
        .admin_send(Method::POST, &path, Some(director.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = ctx.admin_send(Method::POST, &path, Some(director)).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let unknown = "00000000-0000-4000-8000-000000000000";
    let (status, body) = ctx
        .admin_send(
            Method::POST,
            &path,
            Some(json!({ "person_id": unknown, "role": "actor" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "person not found");
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            &path,
            Some(json!({ "person_id": person, "role": "Actor!" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            &format!("/admin/entities/{unknown}/credits"),
            Some(json!({ "person_id": person, "role": "actor" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let credit_id = added["id"].as_str().unwrap();
    // Чужая сущность в пути — 404.
    let other = ctx.entity_id("dune-2021").await;
    let (status, _) = ctx
        .admin_send(
            Method::DELETE,
            &format!("/admin/entities/{other}/credits/{credit_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("{path}/{credit_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("{path}/{credit_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- админка: люди и теги

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn people_crud(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let (status, person) = ctx
        .admin_send(
            Method::POST,
            "/admin/people",
            Some(json!({ "slug": "david-lynch", "full_name": "Дэвид Линч", "birth_date": "1946-01-20", "bio": "Режиссёр." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{person}");
    assert_eq!(person["full_name"], "Дэвид Линч");
    let id = person["id"].as_str().unwrap();

    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/people",
            Some(json!({ "slug": "david-lynch", "full_name": "Другой" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/people",
            Some(json!({ "slug": "x", "full_name": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, person) = ctx
        .admin_send(
            Method::PATCH,
            &format!("/admin/people/{id}"),
            Some(json!({ "bio": null, "photo_url": "https://img.example/lynch.jpg" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{person}");
    assert_eq!(person["bio"], Value::Null);
    assert_eq!(person["birth_date"], "1946-01-20");
    assert_eq!(person["photo_url"], "https://img.example/lynch.jpg");

    let (status, _) = ctx
        .admin_send(
            Method::PATCH,
            &format!("/admin/people/{id}"),
            Some(json!({ "slug": "nobody" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Удаление человека убирает его из титров.
    let villeneuve = ctx.person_id("denis-villeneuve").await;
    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("/admin/people/{villeneuve}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let card = ctx.get_ok("/entities/dune-2021").await;
    assert_eq!(card["credits"].as_array().unwrap().len(), 1);
    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("/admin/people/{villeneuve}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = ctx
        .admin_send(
            Method::PATCH,
            &format!("/admin/people/{villeneuve}"),
            Some(json!({ "full_name": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn tags_crud(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let (status, tag) = ctx
        .admin_send(
            Method::POST,
            "/admin/tags",
            Some(json!({ "slug": "cyberpunk", "name": "Киберпанк" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{tag}");
    assert_eq!(tag["slug"], "cyberpunk");

    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/tags",
            Some(json!({ "slug": "cyberpunk", "name": "Ещё раз" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/tags",
            Some(json!({ "slug": "Кибер", "name": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let sci_fi = ctx.tag_id("sci-fi").await;
    let (status, tag) = ctx
        .admin_send(
            Method::PATCH,
            &format!("/admin/tags/{sci_fi}"),
            Some(json!({ "name": "Фантастика" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag["slug"], "sci-fi");
    assert_eq!(tag["name"], "Фантастика");
    let (status, _) = ctx
        .admin_send(
            Method::PATCH,
            &format!("/admin/tags/{sci_fi}"),
            Some(json!({ "slug": "fantasy" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Удаление тега снимает его со всех сущностей.
    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("/admin/tags/{sci_fi}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(ctx.get_ok("/entities/dune-2021").await["tags"], json!([]));
    let (status, _) = ctx
        .admin_send(Method::DELETE, &format!("/admin/tags/{sci_fi}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- поиск

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn search_returns_503_when_meilisearch_is_off(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let response = ctx.get("/search?q=dune").await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.json()["error"],
        "search is temporarily unavailable"
    );

    let (status, _) = ctx
        .admin_send(Method::POST, "/admin/search/reindex", None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Запись в админке работает и без поиска.
    let (status, _) = ctx
        .admin_send(
            Method::POST,
            "/admin/tags",
            Some(json!({ "slug": "new", "name": "Новое" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn search_after_reindex(pool: PgPool) {
    let ctx = Ctx::with_search(pool);

    // Индекса ещё нет — пустая выдача, а не ошибка.
    let page = ctx.get_ok("/search?q=dune").await;
    assert_eq!(page["items"], json!([]));

    let (status, body) = ctx
        .admin_send(Method::POST, "/admin/search/reindex", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["indexed"], 5);

    // Опечатка и оригинальное название.
    let page = ctx.get_ok("/search?q=dunne").await;
    assert_eq!(page["total"], 3, "{page}");
    let first = &page["items"][0];
    assert!(first["slug"].as_str().unwrap().starts_with("dune"));
    assert!(first["kind"].is_string() && first["title"].is_string());

    // По имени участника.
    let page = ctx
        .get_ok("/search?q=%D0%92%D0%B8%D0%BB%D1%8C%D0%BD%D1%91%D0%B2")
        .await; // «Вильнёв»
    let mut found = slugs(&page);
    found.sort();
    assert_eq!(found, ["dune-2021", "dune-part-two-2024"]);

    // Фильтры.
    assert_eq!(
        slugs(&ctx.get_ok("/search?q=dune&kind=book").await),
        ["dune-novel"]
    );
    assert_eq!(
        slugs(&ctx.get_ok("/search?q=dune&year=2024").await),
        ["dune-part-two-2024"]
    );
    assert_eq!(
        slugs(&ctx.get_ok("/search?tag=fantasy").await),
        ["witcher-3"]
    );
    let page = ctx.get_ok("/search?q=dune&limit=1&offset=1").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["limit"], 1);
    assert_eq!(page["offset"], 1);

    // slug тега подставляется в фильтр Meilisearch, поэтому проверяется.
    let response = ctx.get("/search?tag=x%22%20OR%20kind%20%3D%20book").await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // Повторная перестройка (через swap) не ломает индекс.
    let (status, body) = ctx
        .admin_send(Method::POST, "/admin/search/reindex", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["indexed"], 5);
    assert_eq!(ctx.get_ok("/search?q=dunne").await["total"], 3);

    ctx.drop_search_index().await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn search_follows_admin_changes(pool: PgPool) {
    let ctx = Ctx::with_search(pool);
    let (status, _) = ctx
        .admin_send(Method::POST, "/admin/search/reindex", None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, card) = ctx
        .admin_send(
            Method::POST,
            "/admin/entities",
            Some(json!({ "kind": "book", "slug": "neuromancer", "title": "Нейромант", "original_title": "Neuromancer", "tags": ["sci-fi"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    ctx.search_until("/search?q=neuromancer", |p| slugs(p) == ["neuromancer"])
        .await;

    // Переименование тега попадает в документы сущностей с этим тегом.
    let sci_fi = ctx.tag_id("sci-fi").await;
    ctx.admin_send(
        Method::PATCH,
        &format!("/admin/tags/{sci_fi}"),
        Some(json!({ "slug": "science-fiction" })),
    )
    .await;
    ctx.search_until("/search?tag=science-fiction", |p| p["total"] == 4)
        .await;

    // Переименование человека — в документы его работ.
    let person = ctx.person_id("frank-herbert").await;
    ctx.admin_send(
        Method::PATCH,
        &format!("/admin/people/{person}"),
        Some(json!({ "full_name": "Frank Herbert" })),
    )
    .await;
    ctx.search_until("/search?q=herbert", |p| slugs(p) == ["dune-novel"])
        .await;

    let id = card["id"].as_str().unwrap();
    ctx.admin_send(Method::DELETE, &format!("/admin/entities/{id}"), None)
        .await;
    ctx.search_until("/search?q=neuromancer", |p| p["total"] == 0)
        .await;

    ctx.drop_search_index().await;
}

// ---------------------------------------------------------------- дамп

/// `seeds/catalog.sql` загружается и проходит те же проверки metadata, что и API.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("../../seeds/catalog.sql"))]
async fn seed_catalog_is_valid(pool: PgPool) {
    let rows: Vec<(String, catalog::models::EntityKind, Value)> =
        sqlx::query_as("SELECT slug, kind, metadata FROM entities")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(rows.len() >= 40, "seed has {} entities", rows.len());
    for (slug, kind, metadata) in rows {
        let normalized = catalog::metadata::validate(kind, metadata.clone())
            .unwrap_or_else(|e| panic!("{slug}: {e}"));
        assert_eq!(normalized, metadata, "{slug}: metadata is not normalized");
    }

    let ctx = Ctx::new(pool);
    for kind in ["movie", "series", "book", "game"] {
        let page = ctx.get_ok(&format!("/entities?kind={kind}")).await;
        assert!(page["total"].as_i64().unwrap() >= 5, "few {kind}s");
    }
    // Медиа для фронтенда: фильм с трейлером, фильм только с постером, книга с обложкой.
    let dune = ctx.get_ok("/entities/dune-2021").await;
    let trailers = &dune["metadata"]["trailers"];
    assert_eq!(trailers[0]["provider"], "youtube");
    assert_eq!(trailers[0]["id"], "n9xhJrPXop4");
    assert_eq!(trailers[1]["provider"], "rutube");
    assert!(dune["cover_url"].as_str().unwrap().starts_with("https://"));
    let lynch = ctx.get_ok("/entities/dune-1984").await;
    assert!(lynch["metadata"].get("trailers").is_none());
    assert!(lynch["cover_url"].is_string());
    let novel = ctx.get_ok("/entities/dune-novel").await;
    assert!(novel["cover_url"]
        .as_str()
        .unwrap()
        .contains("openlibrary.org"));

    // Франшиза в разных типах: книга, фильм и игра по «Пикнику на обочине».
    let card = ctx.get_ok("/people/arkady-strugatsky").await;
    assert_eq!(card["credits"].as_array().unwrap().len(), 2);
}
