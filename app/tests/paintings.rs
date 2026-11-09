//! Страница картины: фрагменты полотна, исходник для зума и разбор.
//!
//! Данные — `fixtures/catalog.sql` плюс картина, которую заводит сам тест. Поиск и ISR
//! выключены: здесь проверяется PostgreSQL и HTTP.

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
        request(app, method, path, body, token).await
    }

    async fn get_ok(&self, path: &str) -> Value {
        let response = self
            .send(Method::GET, &format!("{CATALOG}{path}"), None, None)
            .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{path}: {:?}",
            response.json()
        );
        response.json()
    }

    async fn admin_send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let login = self
            .send(
                Method::POST,
                "/api/v1/auth/dev/login",
                Some(json!({ "login": "admin@example.com", "role": "admin" })),
                None,
            )
            .await;
        let token = login.json()["access_token"].as_str().unwrap().to_string();
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

    /// «Ночной дозор» с исходником для зума.
    async fn create_painting(&self, metadata: Value) -> String {
        let (status, body) = self
            .admin_send(
                Method::POST,
                "/admin/entities",
                Some(json!({
                    "kind": "painting",
                    "slug": "night-watch",
                    "title": "Ночной дозор",
                    "original_title": "De Nachtwacht",
                    "release_date": "1642-01-01",
                    "metadata": metadata,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        body["id"].as_str().unwrap().to_string()
    }
}

/// Кроп «скрытого автопортрета» из задачи фронтенда.
fn self_portrait() -> Value {
    json!({
        "title": "Скрытый автопортрет",
        "description": "В глубине толпы, между знаменем и шляпой, видно лицо самого Рембрандта.",
        "crop": { "x": 0.412, "y": 0.168, "width": 0.09, "height": 0.12 },
        "position": 3,
    })
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn fragments_come_with_the_card(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx
        .create_painting(json!({ "technique": "холст, масло" }))
        .await;

    // Фрагментов ещё нет — поле есть, но пустое.
    let card = ctx.get_ok("/entities/night-watch").await;
    assert_eq!(card["fragments"], json!([]));

    let (status, body) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/entities/{id}/fragments"),
            Some(json!({ "fragments": [
                self_portrait(),
                {
                    "title": "Тайна золотой девочки",
                    "crop": { "x": 0.3, "y": 0.4, "width": 0.1, "height": 0.2 },
                    "position": 1,
                },
            ]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    // Порядок — по position, а не по порядку в запросе: деталь рассказывают по смыслу.
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "Тайна золотой девочки");
    assert_eq!(items[1]["title"], "Скрытый автопортрет");
    assert_eq!(
        items[1]["crop"],
        json!({
            "x": 0.412, "y": 0.168, "width": 0.09, "height": 0.12
        })
    );
    assert!(!items[1]["description"].as_str().unwrap().is_empty());

    // Те же фрагменты — в карточке и в отдельной ручке.
    let card = ctx.get_ok("/entities/night-watch").await;
    assert_eq!(card["fragments"].as_array().unwrap().len(), 2);
    let list = ctx.get_ok("/entities/night-watch/fragments").await;
    assert_eq!(list, *card.get("fragments").unwrap());

    // Замена списка целиком: лишние фрагменты снимаются.
    let (status, body) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/entities/{id}/fragments"),
            Some(json!({ "fragments": [self_portrait()] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    // Без position порядок — как в списке.
    assert_eq!(body[0]["position"], 3);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn crop_must_stay_inside_the_canvas(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let id = ctx.create_painting(json!({})).await;
    let bad = |crop: Value| json!({ "fragments": [{ "title": "Деталь", "crop": crop }] });

    for crop in [
        json!({ "x": 0.9, "y": 0.1, "width": 0.2, "height": 0.1 }), // вылезает вправо
        json!({ "x": 0.1, "y": 0.95, "width": 0.1, "height": 0.2 }), // вылезает вниз
        json!({ "x": -0.1, "y": 0.1, "width": 0.1, "height": 0.1 }), // за левым краем
        json!({ "x": 0.1, "y": 0.1, "width": 0.0, "height": 0.1 }), // нулевая ширина
    ] {
        let (status, _) = ctx
            .admin_send(
                Method::PUT,
                &format!("/admin/entities/{id}/fragments"),
                Some(bad(crop.clone())),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{crop}");
    }

    // Фрагменты бывают только у картин.
    let movie_id = ctx.get_ok("/entities/dune-2021").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = ctx
        .admin_send(
            Method::PUT,
            &format!("/admin/entities/{movie_id}/fragments"),
            Some(json!({ "fragments": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn source_image_and_analysis_live_in_metadata(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.create_painting(json!({
        "image_url": "https://upload.wikimedia.org/night-watch.jpg",
        "image_width": 6720,
        "image_height": 5544,
        // Хвостовой слэш отбрасывается: дальше адрес склеивается через `/`.
        "iiif_base": "https://iiif.rijksmuseum.nl/public/SK-C-5/",
        "analysis": {
            "text": "Полотно заказала стрелковая рота…",
            "points": [
                { "title": "Тайна золотой девочки", "text": "Эмблема гильдии…" }
            ]
        },
        "trivia": ["Холст обрезали в 1715 году."],
    }))
    .await;

    let card = ctx.get_ok("/entities/night-watch").await;
    let meta = &card["metadata"];
    assert_eq!(meta["image_width"], 6720);
    assert_eq!(
        meta["iiif_base"],
        "https://iiif.rijksmuseum.nl/public/SK-C-5"
    );
    assert_eq!(
        meta["analysis"]["points"][0]["title"],
        "Тайна золотой девочки"
    );
    // Разбор — не то же, что любопытные факты: `trivia` остаётся как была.
    assert_eq!(meta["trivia"], json!(["Холст обрезали в 1715 году."]));
    // Озвучки в этой задаче нет: кнопка без звука хуже, чем её отсутствие.
    assert!(meta["analysis"]["audio_url"].is_null());
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn source_image_is_validated(pool: PgPool) {
    let ctx = Ctx::new(pool);

    // Все эти картины должны получить 400, поэтому slug у них один и тот же.
    async fn bad(ctx: &Ctx, metadata: Value) -> StatusCode {
        let (status, _) = ctx
            .admin_send(
                Method::POST,
                "/admin/entities",
                Some(json!({
                    "kind": "painting", "slug": "x-painting", "title": "Картина",
                    "metadata": metadata,
                })),
            )
            .await;
        status
    }

    // Размеры — только вдвоём и только вместе с самим файлом.
    assert_eq!(
        bad(
            &ctx,
            json!({ "image_url": "https://e/x.jpg", "image_width": 100 })
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        bad(&ctx, json!({ "image_width": 100, "image_height": 50 })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        bad(&ctx, json!({ "image_url": "file:///tmp/x.jpg" })).await,
        StatusCode::BAD_REQUEST
    );
    // Разбор: текст обязателен, тезисы не пустые.
    assert_eq!(
        bad(&ctx, json!({ "analysis": { "points": [] } })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        bad(&ctx, json!({ "analysis": { "text": "  " } })).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        bad(
            &ctx,
            json!({ "analysis": { "text": "Разбор", "points": [{ "title": "", "text": "x" }] } })
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    // Озвучку намеренно не принимаем, чтобы в интерфейсе не появилась кнопка без звука.
    assert_eq!(
        bad(
            &ctx,
            json!({ "analysis": { "text": "Разбор", "audio_url": "https://e/a.mp3" } })
        )
        .await,
        StatusCode::BAD_REQUEST
    );
}

#[test]
fn painting_seeds_directory_is_valid() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let candidates = [
        std::path::PathBuf::from("seeds/paintings"),
        std::path::PathBuf::from("../seeds/paintings"),
        std::path::PathBuf::from(manifest_dir).join("../seeds/paintings"),
    ];
    let path = candidates
        .into_iter()
        .find(|p| p.is_dir())
        .expect("seeds/paintings directory must exist");

    let entries = std::fs::read_dir(path).expect("read seeds/paintings");
    let mut count = 0;
    let mut slugs = std::collections::HashSet::new();

    for entry in entries {
        let entry = entry.expect("dir entry");
        let file_path = entry.path();
        if file_path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let content = std::fs::read_to_string(&file_path).expect("read json");
        let val: Value = serde_json::from_str(&content).expect("parse json");

        let slug = val["slug"].as_str().expect("slug string");
        assert!(slugs.insert(slug.to_string()), "duplicate slug: {slug}");

        let analysis = &val["analysis"];
        assert!(analysis.is_object(), "[{slug}] analysis must be an object");
        let text = analysis["text"].as_str().expect("analysis text");
        assert!(!text.trim().is_empty(), "[{slug}] analysis text non empty");
        let paragraphs: Vec<&str> = text
            .split("\n\n")
            .filter(|p| !p.trim().is_empty())
            .collect();
        assert!(
            paragraphs.len() >= 2 && paragraphs.len() <= 4,
            "[{slug}] analysis text paragraphs count must be 2..4, got {}",
            paragraphs.len()
        );

        let points = analysis["points"]
            .as_array()
            .expect("analysis points array");
        assert!(
            points.len() >= 2 && points.len() <= 10,
            "[{slug}] points count must be 2..10, got {}",
            points.len()
        );
        for pt in points {
            let title = pt["title"].as_str().expect("point title");
            assert!(!title.trim().is_empty() && title.len() <= 120);
            let p_text = pt["text"].as_str().expect("point text");
            assert!(!p_text.trim().is_empty() && p_text.len() <= 600);
        }

        let fragments = val["fragments"].as_array().expect("fragments array");
        assert!(
            fragments.len() >= 3 && fragments.len() <= 20,
            "[{slug}] fragments count must be 3..20, got {}",
            fragments.len()
        );
        for frag in fragments {
            let title = frag["title"].as_str().expect("fragment title");
            assert!(!title.trim().is_empty() && title.len() <= 120);
            if let Some(desc) = frag.get("description").and_then(|d| d.as_str()) {
                assert!(desc.len() <= 600);
            }
            let crop = &frag["crop"];
            let x = crop["x"].as_f64().expect("crop x");
            let y = crop["y"].as_f64().expect("crop y");
            let w = crop["width"].as_f64().expect("crop width");
            let h = crop["height"].as_f64().expect("crop height");
            assert!((0.0..=1.0).contains(&x), "[{slug}:{title}] x={x}");
            assert!((0.0..=1.0).contains(&y), "[{slug}:{title}] y={y}");
            assert!(w > 0.0 && w <= 1.0, "[{slug}:{title}] w={w}");
            assert!(h > 0.0 && h <= 1.0, "[{slug}:{title}] h={h}");
            assert!(x + w <= 1.00001, "[{slug}:{title}] x+w={}", x + w);
            assert!(y + h <= 1.00001, "[{slug}:{title}] y+h={}", y + h);
            assert!(w * h >= 0.025, "[{slug}:{title}] area {} < 0.025", w * h);
        }

        count += 1;
    }

    assert!(
        (15..=20).contains(&count),
        "must have 15-20 paintings seeded, got {count}"
    );
    assert!(slugs.contains("the-night-watch"));
    assert!(slugs.contains("las-meninas"));
    assert!(slugs.contains("guernica"));
}
