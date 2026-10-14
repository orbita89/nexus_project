//! Холодный архив каталога: сборка горячего слоя и архива из дампа, поиск по архиву, промоушен.

use axum::http::{Method, StatusCode};
use catalog::archive::pipeline::{self, BuildOptions, BuildReport};
use catalog::archive::Archive;
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use test_utils::{request, TestResponse};

const CATALOG: &str = "/api/v1/catalog";

/// Каталог теста: индекс архива и дамп; удаляется при drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("nexus-archive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Ctx {
    state: AppState,
    dir: TempDir,
    /// Сборка без удаления из PostgreSQL (`--defer-delete`).
    defer_delete: bool,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let dir = TempDir::new();
        let mut config = Config::from_env();
        config.dev_login = true;
        config.archive_dir = Some(dir.0.join("index").to_string_lossy().into_owned());
        let state = test_utils::state_with_config(pool, config).0;
        Self {
            state,
            dir,
            defer_delete: false,
        }
    }

    fn archive(&self) -> Arc<Archive> {
        catalog::archive::open(&self.state).unwrap()
    }

    fn dump_path(&self) -> PathBuf {
        self.dir.0.join("dump.tsv")
    }

    async fn send(&self, method: Method, path: &str, token: Option<&str>) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        request(app, method, &format!("{CATALOG}{path}"), None, token).await
    }

    async fn login(&self, login: &str, role: &str) -> Value {
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
        response.json()
    }

    async fn token(&self, login: &str, role: &str) -> String {
        self.login(login, role).await["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn search(&self, q: &str) -> Vec<String> {
        let response = self
            .send(Method::GET, &format!("/archive/search?q={q}"), None)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        response.json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["slug"].as_str().unwrap().to_string())
            .collect()
    }

    async fn slugs(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT slug FROM entities ORDER BY slug")
            .fetch_all(&self.state.db)
            .await
            .unwrap()
    }

    /// Счётчики после удаления: работы человека `both` (всего, в активных) и активные с тегом.
    async fn counters(&self) -> (i32, i32, i32) {
        sqlx::query_as(
            "SELECT pc.credits, pc.active_credits, t.entities_count
             FROM person_credit_counts pc, people p, tags t
             WHERE pc.person_id = p.id AND p.slug = 'both' AND t.slug = 'drama'",
        )
        .fetch_one(&self.state.db)
        .await
        .unwrap()
    }

    async fn archived_tmdb(&self) -> Vec<i32> {
        sqlx::query_scalar("SELECT tmdb_id FROM archived_tmdb ORDER BY 1")
            .fetch_all(&self.state.db)
            .await
            .unwrap()
    }

    /// Каталог: фильмы TMDB с popularity, ручной фильм, фильм в интересах пользователя и люди.
    async fn seed(&self) {
        let films = [
            // slug, title, tmdb, popularity, active, manual
            ("ru-hit-1", "Хит один", 1, 50.0, true, false),
            ("ru-hit-2", "Хит два", 2, 40.0, true, false),
            ("ru-low", "Низкий старт", 3, 1.0, false, false),
            ("en-hit", "Big Hit", 4, 30.0, false, false),
            ("en-low", "Small Film", 5, 2.0, true, false),
            ("en-used", "Used Film", 6, 0.5, false, false),
            ("manual-low", "Manual Film", 7, 0.1, true, true),
        ];
        for (slug, title, tmdb_id, popularity, active, manual) in films {
            sqlx::query(
                "WITH e AS (
                     INSERT INTO entities (kind, slug, title, original_title, release_date,
                                           active, active_manual)
                     VALUES ('movie', $1, $2, $2 || ' (orig)', '2001-02-03', $5, $6) RETURNING id
                 ), t AS (INSERT INTO entity_tmdb (entity_id, tmdb_id) SELECT id, $3 FROM e)
                 INSERT INTO entity_stats (entity_id, tmdb_popularity) SELECT id, $4 FROM e",
            )
            .bind(slug)
            .bind(title)
            .bind(tmdb_id)
            .bind(popularity)
            .bind(active)
            .bind(manual)
            .execute(&self.state.db)
            .await
            .unwrap();
        }
        sqlx::raw_sql(
            "INSERT INTO people (slug, full_name) VALUES ('only-en-low', 'A'), ('both', 'B');
             INSERT INTO entity_credits (entity_id, person_id, role, position)
             SELECT e.id, p.id, 'actor', 0 FROM entities e, people p
             WHERE (e.slug, p.slug) IN (('en-low', 'only-en-low'), ('en-low', 'both'),
                                        ('ru-hit-1', 'both'));
             INSERT INTO tags (slug, name) VALUES ('drama', 'Драма');
             INSERT INTO entity_tags (entity_id, tag_id)
             SELECT e.id, t.id FROM entities e, tags t
             WHERE e.slug IN ('ru-hit-1', 'ru-hit-2') AND t.slug = 'drama'",
        )
        .execute(&self.state.db)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE entities SET cover_url = 'https://img.test/' || slug || '.jpg'
             WHERE slug IN ('en-low', 'ru-low')",
        )
        .execute(&self.state.db)
        .await
        .unwrap();
        let user = self.login("fan@example.com", "user").await;
        let user_id = uuid::Uuid::parse_str(user["user"]["id"].as_str().unwrap()).unwrap();
        sqlx::query(
            "INSERT INTO user_interests (user_id, entity_id)
             SELECT $1, id FROM entities WHERE slug = 'en-used'",
        )
        .bind(user_id)
        .execute(&self.state.db)
        .await
        .unwrap();
    }

    /// Дамп из БД плюс строки «извне»: популярный фильм, которого нет в каталоге, и непопулярный.
    async fn dump_and_build(&self) -> BuildReport {
        let report = pipeline::dump(&self.state, Some(self.archive()), &self.dump_path())
            .await
            .unwrap();
        assert_eq!(report.from_db, 6, "manual-low is not in the dump");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.dump_path())
            .unwrap();
        writeln!(
            file,
            "ru-new\tНовинка\tNew \"One\"\tmovie\t45\ttrue\t8\t2020-05-06"
        )
        .unwrap();
        writeln!(file, "old-gem\tOld Gem\t\tmovie\t0.1\tfalse\t9\t").unwrap();
        writeln!(file, "Bad Slug\tX\t\tmovie\t1\tfalse\t\t").unwrap();
        drop(file);

        let options = BuildOptions {
            ru: 2,
            foreign: 1,
            fresh: true,
            defer_delete: self.defer_delete,
        };
        pipeline::build(&self.state, self.archive(), &self.dump_path(), options)
            .await
            .unwrap()
    }
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().count()
}

/// Top-K по двум спискам, вставка и включение горячих, архив, удаление ушедших в архив вместе с
/// людьми без работ; фильм с данными пользователя и ручной остаются.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn build_splits_hot_layer_and_archive(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.seed().await;

    let report = ctx.dump_and_build().await;
    assert_eq!(
        report,
        BuildReport {
            rows: 8,
            invalid: 1,
            hot: 3,
            inserted: 1,
            activated: 1,
            archived: 4,
            deleted: 3,
            kept_in_use: 1,
            kept_manual: 0,
            people_deleted: 1,
            deferred: 0,
            archive_size: 4,
        }
    );

    // ru: ru-hit-1 (50), ru-new (45); иностранный: en-hit (30). en-used — в интересах.
    assert_eq!(
        ctx.slugs().await,
        ["en-hit", "en-used", "manual-low", "ru-hit-1", "ru-new"]
    );
    let (active, tmdb_id, popularity, original): (bool, i32, f64, String) = sqlx::query_as(
        "SELECT e.active, t.tmdb_id, s.tmdb_popularity, e.original_title FROM entities e
         JOIN entity_tmdb t ON t.entity_id = e.id JOIN entity_stats s ON s.entity_id = e.id
         WHERE e.slug = 'ru-new'",
    )
    .fetch_one(&ctx.state.db)
    .await
    .unwrap();
    assert_eq!(
        (active, tmdb_id, popularity, original.as_str()),
        (true, 8, 45.0, "New \"One\"")
    );
    let en_hit: bool = sqlx::query_scalar("SELECT active FROM entities WHERE slug = 'en-hit'")
        .fetch_one(&ctx.state.db)
        .await
        .unwrap();
    assert!(en_hit);
    assert_eq!(ctx.archived_tmdb().await, [2, 3, 5, 9]);
    let people: Vec<String> = sqlx::query_scalar("SELECT slug FROM people ORDER BY slug")
        .fetch_all(&ctx.state.db)
        .await
        .unwrap();
    assert_eq!(people, ["both"]);
    assert_eq!(ctx.counters().await, (1, 1, 1));

    assert_eq!(ctx.search("хит").await, ["ru-hit-2"]);
    assert_eq!(ctx.search("small%20film").await, ["en-low"]);
    assert_eq!(
        ctx.search("orig").await.len(),
        3,
        "orig_title is searchable"
    );
    assert!(ctx.search("used").await.is_empty());
    let hit = ctx
        .send(Method::GET, "/archive/search?q=gem", None)
        .await
        .json();
    assert_eq!(
        hit["items"][0],
        json!({
            "slug": "old-gem", "title": "Old Gem", "original_title": null, "kind": "movie",
            "tmdb_id": 9, "release_date": null, "popularity": 0.1, "cover_url": null
        })
    );
    assert_eq!(
        (
            hit["total"].clone(),
            hit["limit"].clone(),
            hit["offset"].clone()
        ),
        (json!(1), json!(20), json!(0))
    );

    // Постер из PostgreSQL переехал в архив; страница фильма в архиве.
    let page = ctx
        .send(Method::GET, "/archive/entities/en-low", None)
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.json()["title"], "Small Film");
    assert_eq!(page.json()["cover_url"], "https://img.test/en-low.jpg");
    for missing in ["/archive/entities/ru-hit-1", "/archive/entities/Bad%20Slug"] {
        let response = ctx.send(Method::GET, missing, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{missing}");
    }

    // Страницы выдачи: «orig» есть у трёх документов.
    let first = ctx
        .send(Method::GET, "/archive/search?q=orig&limit=2", None)
        .await
        .json();
    let rest = ctx
        .send(Method::GET, "/archive/search?q=orig&limit=2&offset=2", None)
        .await
        .json();
    assert_eq!(
        (first["total"].clone(), rest["total"].clone()),
        (json!(3), json!(3))
    );
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_ne!(first["items"][0]["slug"], rest["items"][0]["slug"]);
    assert_ne!(first["items"][1]["slug"], rest["items"][0]["slug"]);

    // Повторный дамп: горячие из БД и весь архив; повторная сборка ничего не меняет.
    pipeline::dump(&ctx.state, Some(ctx.archive()), &ctx.dump_path())
        .await
        .unwrap();
    assert_eq!(lines(&ctx.dump_path()), 1 + 4 + 4);
    let dumped = std::fs::read_to_string(ctx.dump_path()).unwrap();
    assert!(
        dumped.contains("\thttps://img.test/en-low.jpg"),
        "the archive cover survives the next dump"
    );
    let again = pipeline::build(
        &ctx.state,
        ctx.archive(),
        &ctx.dump_path(),
        BuildOptions {
            ru: 2,
            foreign: 1,
            fresh: false,
            defer_delete: false,
        },
    )
    .await
    .unwrap();
    assert_eq!((again.inserted, again.activated, again.deleted), (0, 0, 0));
    assert_eq!((again.archived, again.archive_size), (4, 4));
    assert_eq!(ctx.slugs().await.len(), 5);
}

/// Первичная раскладка: сборка без удаления, затем `scripts/archive-purge.sql` одним проходом —
/// та же итоговая БД, что и построчно, со счётчиками.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn deferred_delete_and_purge_script(pool: PgPool) {
    let mut ctx = Ctx::new(pool);
    ctx.defer_delete = true;
    ctx.seed().await;

    let report = ctx.dump_and_build().await;
    assert_eq!(
        (report.deleted, report.deferred, report.archived),
        (0, 3, 4)
    );
    assert_eq!(ctx.slugs().await.len(), 8, "nothing deleted yet");
    assert_eq!(ctx.archived_tmdb().await, [2, 3, 5, 9]);

    sqlx::raw_sql(include_str!("../../scripts/archive-purge.sql"))
        .execute(&ctx.state.db)
        .await
        .unwrap();
    assert_eq!(
        ctx.slugs().await,
        ["en-hit", "en-used", "manual-low", "ru-hit-1", "ru-new"]
    );
    let people: Vec<String> = sqlx::query_scalar("SELECT slug FROM people ORDER BY slug")
        .fetch_all(&ctx.state.db)
        .await
        .unwrap();
    assert_eq!(people, ["both"]);
    assert_eq!(ctx.counters().await, (1, 1, 1));

    // Повторный запуск ничего не находит.
    sqlx::raw_sql(include_str!("../../scripts/archive-purge.sql"))
        .execute(&ctx.state.db)
        .await
        .unwrap();
    assert_eq!(ctx.slugs().await.len(), 5);
}

/// Поиск: проверка параметров; без `ARCHIVE_DIR` — 503.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn archive_search_validates_query(pool: PgPool) {
    let ctx = Ctx::new(pool.clone());
    for path in [
        "/archive/search?q=%20",
        "/archive/search?q=a&limit=0",
        "/archive/search?q=a&limit=21",
        "/archive/search?q=a&offset=-1",
        "/archive/search?q=a&offset=1001",
    ] {
        let response = ctx.send(Method::GET, path, None).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{path}");
    }
    let long = "a".repeat(201);
    let response = ctx
        .send(Method::GET, &format!("/archive/search?q={long}"), None)
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(ctx.search("anything").await.is_empty());
    // Синтаксис запроса Tantivy не даёт ошибку.
    assert!(ctx.search("title%3A%22(").await.is_empty());

    let mut state = ctx.state.clone();
    let mut config = (*state.config).clone();
    config.archive_dir = None;
    state.config = Arc::new(config);
    for path in ["/archive/search?q=a", "/archive/entities/a"] {
        let app = nexus::build_app(state.clone());
        let response = request(app, Method::GET, &format!("{CATALOG}{path}"), None, None).await;
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
    }
}

/// Промоушен: только админ; фильм возвращается в каталог видимым и пропадает из архива;
/// пока писатель архива занят — 409 без записи в БД.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn promote_moves_film_from_archive_to_catalog(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.seed().await;
    ctx.dump_and_build().await;
    let user = ctx.token("user@example.com", "user").await;
    let admin = ctx.token("admin@example.com", "admin").await;
    let path = "/admin/archive/promote/ru-low";

    assert_eq!(
        ctx.send(Method::POST, path, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ctx.send(Method::POST, path, Some(&user)).await.status,
        StatusCode::FORBIDDEN
    );
    for missing in [
        "/admin/archive/promote/nope",
        "/admin/archive/promote/Bad%20Slug",
    ] {
        let response = ctx.send(Method::POST, missing, Some(&admin)).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{missing}");
    }

    let writer = ctx.archive().writer().unwrap();
    let busy = ctx.send(Method::POST, path, Some(&admin)).await;
    assert_eq!(busy.status, StatusCode::CONFLICT, "{:?}", busy.json());
    assert!(!ctx.slugs().await.contains(&"ru-low".to_string()));
    drop(writer);

    let response = ctx.send(Method::POST, path, Some(&admin)).await;
    assert_eq!(
        response.status,
        StatusCode::CREATED,
        "{:?}",
        response.json()
    );
    let body = response.json();
    assert_eq!(body["slug"], "ru-low");
    assert_eq!(body["title"], "Низкий старт");
    assert_eq!(body["original_title"], "Низкий старт (orig)");
    assert_eq!(body["release_date"], "2001-02-03");
    assert_eq!(body["cover_url"], "https://img.test/ru-low.jpg");

    let card = ctx.send(Method::GET, "/entities/ru-low", None).await;
    assert_eq!(
        card.status,
        StatusCode::OK,
        "visible: TMDB film with a past date"
    );
    let manual: bool =
        sqlx::query_scalar("SELECT active_manual FROM entities WHERE slug = 'ru-low'")
            .fetch_one(&ctx.state.db)
            .await
            .unwrap();
    assert!(manual, "hot-layer builds keep promoted films");
    assert_eq!(ctx.archived_tmdb().await, [2, 5, 9]);
    assert!(ctx.search("низкий").await.is_empty());
    assert_eq!(ctx.archive().len(), 3);

    let again = ctx.send(Method::POST, path, Some(&admin)).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}
