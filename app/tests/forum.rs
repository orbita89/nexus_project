//! Форум модуля social: темы с привязкой к нескольким сущностям, сообщения с ветками, модерация.
//!
//! Сущности — `fixtures/catalog.sql`, пользователи создаются через dev login.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::Config;
use sqlx::PgPool;
use test_utils::{request, TestResponse};

const SOCIAL: &str = "/api/v1/social";

struct Ctx {
    state: shared::AppState,
}

/// Пользователь, вошедший через dev login.
struct User {
    token: String,
    username: String,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let mut config = Config::from_env();
        config.dev_login = true;
        let (state, _) = test_utils::state_with_config(pool, config);
        Self { state }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        user: Option<&User>,
    ) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        let token = user.map(|user| user.token.as_str());
        request(app, method, &format!("{SOCIAL}{path}"), body, token).await
    }

    /// Запрос, ожидающий `status`: тело ответа (`null` — тела нет). Любая ошибка — `{"error": "..."}`.
    async fn expect(
        &self,
        status: StatusCode,
        method: Method,
        path: &str,
        body: Option<Value>,
        user: Option<&User>,
    ) -> Value {
        let response = self.send(method.clone(), path, body, user).await;
        let text = String::from_utf8_lossy(&response.body).to_string();
        assert_eq!(response.status, status, "{method} {path}: {text}");
        if response.body.is_empty() {
            return Value::Null;
        }
        let body = response.json();
        if !status.is_success() {
            assert!(body["error"].is_string(), "{method} {path}: {body}");
        }
        body
    }

    async fn get_ok(&self, path: &str, user: Option<&User>) -> Value {
        self.expect(StatusCode::OK, Method::GET, path, None, user)
            .await
    }

    async fn login(&self, name: &str, role: &str) -> User {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            Method::POST,
            "/api/v1/auth/dev/login",
            Some(json!({ "login": format!("{name}@example.com"), "role": role })),
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        let body = response.json();
        User {
            token: body["access_token"].as_str().unwrap().to_string(),
            username: body["user"]["username"].as_str().unwrap().to_string(),
        }
    }

    /// Тема от `user` про сущности `entities`: id.
    async fn thread(&self, user: &User, title: &str, entities: &[&str]) -> String {
        let created = self
            .expect(
                StatusCode::CREATED,
                Method::POST,
                "/threads",
                Some(json!({ "title": title, "body": "Текст темы", "entities": entities })),
                Some(user),
            )
            .await;
        created["id"].as_str().unwrap().to_string()
    }

    /// Сообщение в теме (ответ на `parent`, если задан): id.
    async fn post(&self, user: &User, thread: &str, body: &str, parent: Option<&str>) -> String {
        let created = self
            .expect(
                StatusCode::CREATED,
                Method::POST,
                &format!("/threads/{thread}/posts"),
                Some(json!({ "body": body, "parent_id": parent })),
                Some(user),
            )
            .await;
        created["id"].as_str().unwrap().to_string()
    }
}

fn items(page: &Value) -> &Vec<Value> {
    page["items"].as_array().unwrap()
}

/// Значения поля `field` (путь через `/`) у элементов массива.
fn pluck<'a>(list: &'a [Value], field: &str) -> Vec<&'a str> {
    list.iter()
        .map(|item| {
            field
                .split('/')
                .fold(item, |value, key| &value[key])
                .as_str()
                .unwrap_or_else(|| panic!("{field} in {item}"))
        })
        .collect()
}

// ---------------------------------------------------------------- темы

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn create_thread_needs_author_role(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let user = ctx.login("user", "user").await;
    let author = ctx.login("author", "author").await;
    let body = json!({ "title": "  Дюна: книга против фильма ", "body": " Сравниваем. ",
                       "entities": ["dune-novel", "dune-2021"] });

    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        "/threads",
        Some(body.clone()),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::POST,
        "/threads",
        Some(body.clone()),
        Some(&user),
    )
    .await;

    let created = ctx
        .expect(
            StatusCode::CREATED,
            Method::POST,
            "/threads",
            Some(body),
            Some(&author),
        )
        .await;
    assert_eq!(created["title"], "Дюна: книга против фильма");
    assert_eq!(created["body"], "Сравниваем.");
    assert_eq!(created["author"]["username"], author.username.as_str());
    // Порядок сущностей — как в запросе: первая главная.
    let entities = created["entities"].as_array().unwrap();
    assert_eq!(pluck(entities, "slug"), ["dune-novel", "dune-2021"]);
    assert_eq!(pluck(entities, "kind"), ["book", "movie"]);
    assert_eq!(created["posts_count"], 0);
    assert_eq!(created["is_locked"], false);
    assert_eq!(created["edited_at"], Value::Null);
    assert_eq!(created["last_post_at"], created["created_at"]);
    assert_eq!(created["posts"]["total"], 0);
    assert_eq!(created["posts"]["limit"], 50);

    // admin тоже может создавать темы.
    let admin = ctx.login("admin", "admin").await;
    ctx.thread(&admin, "От админа", &["witcher-3"]).await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn create_thread_validates_fields(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let bad = [
        json!({ "title": "T", "body": "B", "entities": [] }),
        json!({ "title": "T", "body": "B", "entities": ["dune-2021", "dune-2021"] }),
        json!({ "title": "T", "body": "B", "entities": ["no-such-entity"] }),
        json!({ "title": "   ", "body": "B", "entities": ["dune-2021"] }),
        json!({ "title": "T", "body": "", "entities": ["dune-2021"] }),
        json!({ "title": "x".repeat(201), "body": "B", "entities": ["dune-2021"] }),
        json!({ "title": "T", "body": "x".repeat(20_001), "entities": ["dune-2021"] }),
        json!({ "title": "T", "body": "B", "entities": vec!["dune-2021"; 11] }),
        json!({ "title": "T", "body": "B", "entities": ["dune-2021"], "pinned": true }),
        json!({ "title": "T", "body": "B" }),
    ];
    for body in bad {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::POST,
            "/threads",
            Some(body),
            Some(&author),
        )
        .await;
    }
    let page = ctx.get_ok("/threads", None).await;
    assert_eq!(page["total"], 0);
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn get_thread(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let id = ctx.thread(&author, "Тема", &["dune-2021"]).await;

    let thread = ctx.get_ok(&format!("/threads/{id}"), None).await;
    assert_eq!(thread["id"], id.as_str());
    assert_eq!(thread["body"], "Текст темы");
    assert_eq!(
        pluck(thread["entities"].as_array().unwrap(), "title"),
        ["Дюна"]
    );

    let missing = "00000000-0000-4000-8000-000000000000";
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        &format!("/threads/{missing}"),
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/threads/not-a-uuid",
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::GET,
        &format!("/threads/{id}?limit=x"),
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn thread_lists_new_and_active(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let other = ctx.login("other", "author").await;
    let user = ctx.login("user", "user").await;

    let both = ctx
        .thread(&author, "Книга и фильм", &["dune-novel", "dune-2021"])
        .await;
    let movie = ctx.thread(&other, "Только фильм", &["dune-2021"]).await;
    let game = ctx.thread(&author, "Ведьмак", &["witcher-3"]).await;
    // Ответ поднимает первую тему в активных.
    ctx.post(&user, &both, "Ответ", None).await;

    let new = ctx.get_ok("/threads?sort=new", None).await;
    assert_eq!(new["total"], 3);
    assert_eq!(pluck(items(&new), "id"), [&game, &movie, &both]);
    let active = ctx.get_ok("/threads", None).await;
    assert_eq!(pluck(items(&active), "id"), [&both, &game, &movie]);
    assert_eq!(items(&active)[0]["posts_count"], 1);
    // В списке нет текста темы.
    assert!(items(&active)[0].get("body").is_none());
    let paged = ctx.get_ok("/threads?limit=1&offset=1", None).await;
    assert_eq!(pluck(items(&paged), "id"), [&game]);
    assert_eq!(paged["total"], 3);

    // Тема про книгу и фильм видна у обеих сущностей.
    let movie_threads = ctx
        .get_ok("/entities/dune-2021/threads?sort=new", None)
        .await;
    assert_eq!(pluck(items(&movie_threads), "id"), [&movie, &both]);
    let book_threads = ctx.get_ok("/entities/dune-novel/threads", None).await;
    assert_eq!(pluck(items(&book_threads), "id"), [&both]);
    assert_eq!(
        pluck(
            items(&book_threads)[0]["entities"].as_array().unwrap(),
            "slug"
        ),
        ["dune-novel", "dune-2021"]
    );
    let none = ctx.get_ok("/entities/no-date/threads", None).await;
    assert_eq!(none["total"], 0);

    let mine = ctx
        .get_ok(&format!("/users/{}/threads", author.username), None)
        .await;
    assert_eq!(pluck(items(&mine), "id"), [&game, &both]);

    for path in ["/entities/unknown/threads", "/users/nobody/threads"] {
        ctx.expect(StatusCode::NOT_FOUND, Method::GET, path, None, None)
            .await;
    }
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/threads?sort=popular",
        None,
        None,
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn update_own_thread(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let other = ctx.login("other", "author").await;
    let admin = ctx.login("admin", "admin").await;
    let id = ctx.thread(&author, "Было", &["dune-2021"]).await;
    let path = format!("/threads/{id}");

    // Только заголовок: текст и сущности не меняются, появляется пометка «изменено».
    let updated = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(json!({ "title": "Стало" })),
            Some(&author),
        )
        .await;
    assert_eq!(updated["title"], "Стало");
    assert_eq!(updated["body"], "Текст темы");
    assert!(updated["edited_at"].is_string());
    assert_eq!(
        pluck(updated["entities"].as_array().unwrap(), "slug"),
        ["dune-2021"]
    );

    // Набор сущностей заменяется целиком и в новом порядке.
    let updated = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(json!({ "entities": ["dune-part-two-2024", "dune-novel"], "body": "Новый" })),
            Some(&author),
        )
        .await;
    assert_eq!(updated["body"], "Новый");
    assert_eq!(
        pluck(updated["entities"].as_array().unwrap(), "slug"),
        ["dune-part-two-2024", "dune-novel"]
    );
    let old = ctx.get_ok("/entities/dune-2021/threads", None).await;
    assert_eq!(old["total"], 0);

    // Неизвестная сущность — 400, и ничего не меняется.
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PATCH,
        &path,
        Some(json!({ "title": "Не сохранится", "entities": ["dune-novel", "unknown"] })),
        Some(&author),
    )
    .await;
    let thread = ctx.get_ok(&path, None).await;
    assert_eq!(thread["title"], "Стало");
    assert_eq!(thread["entities"].as_array().unwrap().len(), 2);

    // Чужую тему не меняет никто, даже admin (он только удаляет и закрывает).
    for user in [&other, &admin] {
        ctx.expect(
            StatusCode::FORBIDDEN,
            Method::PATCH,
            &path,
            Some(json!({ "title": "X" })),
            Some(user),
        )
        .await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PATCH,
        &path,
        Some(json!({ "title": "X" })),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        "/threads/00000000-0000-4000-8000-000000000000",
        Some(json!({ "title": "X" })),
        Some(&author),
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PATCH,
        &path,
        Some(json!({ "entities": [] })),
        Some(&author),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn delete_own_thread(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let other = ctx.login("other", "author").await;
    let user = ctx.login("user", "user").await;
    let id = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    let post = ctx.post(&user, &id, "Ответ", None).await;
    let path = format!("/threads/{id}");

    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &path,
        None,
        Some(&other),
    )
    .await;
    ctx.expect(StatusCode::UNAUTHORIZED, Method::DELETE, &path, None, None)
        .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &path,
        None,
        Some(&author),
    )
    .await;
    ctx.expect(StatusCode::NOT_FOUND, Method::GET, &path, None, None)
        .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &path,
        None,
        Some(&author),
    )
    .await;
    // Сообщения удалены вместе с темой.
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &format!("/posts/{post}"),
        None,
        Some(&user),
    )
    .await;
    let entity = ctx.get_ok("/entities/dune-2021/threads", None).await;
    assert_eq!(entity["total"], 0);
}

// ---------------------------------------------------------------- сообщения

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn posts_form_branches(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;
    let thread = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    let before = ctx.get_ok(&format!("/threads/{thread}"), None).await;

    let created = ctx
        .expect(
            StatusCode::CREATED,
            Method::POST,
            &format!("/threads/{thread}/posts"),
            Some(json!({ "body": "  Первый  " })),
            Some(&alice),
        )
        .await;
    assert_eq!(created["body"], "Первый");
    assert_eq!(created["author"]["username"], alice.username.as_str());
    assert_eq!(created["thread_id"], thread.as_str());
    assert_eq!(created["parent_id"], Value::Null);
    assert_eq!(created["reply_to"], Value::Null);
    assert_eq!(created["deleted"], false);
    let first = created["id"].as_str().unwrap().to_string();

    let reply = ctx
        .expect(
            StatusCode::CREATED,
            Method::POST,
            &format!("/threads/{thread}/posts"),
            Some(json!({ "body": "Ответ Алисе", "parent_id": first })),
            Some(&bob),
        )
        .await;
    assert_eq!(reply["parent_id"], first.as_str());
    assert_eq!(reply["reply_to"]["username"], alice.username.as_str());
    let reply = reply["id"].as_str().unwrap().to_string();
    let deeper = ctx
        .post(&alice, &thread, "Ответ на ответ", Some(&reply))
        .await;

    // Плоский список по времени, ветки — через parent_id.
    let detail = ctx.get_ok(&format!("/threads/{thread}"), None).await;
    let posts = items(&detail["posts"]);
    assert_eq!(pluck(posts, "id"), [&first, &reply, &deeper]);
    assert_eq!(posts[2]["parent_id"], reply.as_str());
    assert_eq!(posts[2]["reply_to"]["username"], bob.username.as_str());
    assert_eq!(detail["posts_count"], 3);
    // last_post_at — время последнего сообщения.
    assert_ne!(detail["last_post_at"], before["last_post_at"]);
    assert_eq!(detail["last_post_at"], posts[2]["created_at"]);

    let page = ctx
        .get_ok(&format!("/threads/{thread}?limit=1&offset=1"), None)
        .await;
    assert_eq!(pluck(items(&page["posts"]), "id"), [&reply]);
    assert_eq!(page["posts"]["total"], 3);
    assert_eq!(page["posts"]["limit"], 1);

    // Родитель из другой темы или несуществующий — 400.
    let other = ctx.thread(&author, "Другая", &["witcher-3"]).await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::POST,
        &format!("/threads/{other}/posts"),
        Some(json!({ "body": "X", "parent_id": first })),
        Some(&bob),
    )
    .await;
    for body in [
        json!({ "body": "   " }),
        json!({ "body": "x".repeat(10_001) }),
        json!({}),
    ] {
        ctx.expect(
            StatusCode::BAD_REQUEST,
            Method::POST,
            &format!("/threads/{thread}/posts"),
            Some(body),
            Some(&bob),
        )
        .await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        &format!("/threads/{thread}/posts"),
        Some(json!({ "body": "X" })),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::POST,
        "/threads/00000000-0000-4000-8000-000000000000/posts",
        Some(json!({ "body": "X" })),
        Some(&bob),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn update_own_post(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let alice = ctx.login("alice", "user").await;
    let admin = ctx.login("admin", "admin").await;
    let thread = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    let post = ctx.post(&alice, &thread, "Было", None).await;
    let path = format!("/posts/{post}");

    let updated = ctx
        .expect(
            StatusCode::OK,
            Method::PATCH,
            &path,
            Some(json!({ "body": "Стало" })),
            Some(&alice),
        )
        .await;
    assert_eq!(updated["body"], "Стало");
    assert!(updated["edited_at"].is_string());

    for user in [&author, &admin] {
        ctx.expect(
            StatusCode::FORBIDDEN,
            Method::PATCH,
            &path,
            Some(json!({ "body": "X" })),
            Some(user),
        )
        .await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PATCH,
        &path,
        Some(json!({ "body": "X" })),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::PATCH,
        &path,
        Some(json!({ "body": "" })),
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        "/posts/00000000-0000-4000-8000-000000000000",
        Some(json!({ "body": "X" })),
        Some(&alice),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn delete_post_with_replies_leaves_placeholder(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;
    let thread = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    let first = ctx.post(&alice, &thread, "Первый", None).await;
    let reply = ctx.post(&bob, &thread, "Ответ", Some(&first)).await;
    let lonely = ctx.post(&alice, &thread, "Без ответов", None).await;

    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &format!("/posts/{first}"),
        None,
        Some(&bob),
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::DELETE,
        &format!("/posts/{first}"),
        None,
        None,
    )
    .await;

    // Без ответов — исчезает целиком.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/posts/{lonely}"),
        None,
        Some(&alice),
    )
    .await;
    // С ответом — остаётся заглушка без текста и автора.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/posts/{first}"),
        None,
        Some(&alice),
    )
    .await;
    let detail = ctx.get_ok(&format!("/threads/{thread}"), None).await;
    let posts = items(&detail["posts"]);
    assert_eq!(pluck(posts, "id"), [&first, &reply]);
    assert_eq!(posts[0]["deleted"], true);
    assert_eq!(posts[0]["body"], Value::Null);
    assert_eq!(posts[0]["author"], Value::Null);
    assert_eq!(posts[1]["parent_id"], first.as_str());
    assert_eq!(posts[1]["reply_to"], Value::Null);
    assert_eq!(detail["posts_count"], 1);
    assert_eq!(detail["posts"]["total"], 2);

    // Заглушку нельзя ни удалить, ни изменить, ни ответить на неё.
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &format!("/posts/{first}"),
        None,
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PATCH,
        &format!("/posts/{first}"),
        Some(json!({ "body": "X" })),
        Some(&alice),
    )
    .await;
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::POST,
        &format!("/threads/{thread}/posts"),
        Some(json!({ "body": "X", "parent_id": first })),
        Some(&bob),
    )
    .await;

    // Последний ответ удалён — заглушка больше не нужна и уходит следом.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/posts/{reply}"),
        None,
        Some(&bob),
    )
    .await;
    let detail = ctx.get_ok(&format!("/threads/{thread}"), None).await;
    assert_eq!(detail["posts"]["total"], 0);
    assert_eq!(detail["posts_count"], 0);
}

// ---------------------------------------------------------------- модерация

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_locks_thread(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let alice = ctx.login("alice", "user").await;
    let admin = ctx.login("admin", "admin").await;
    let thread = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    let post = ctx.post(&alice, &thread, "До закрытия", None).await;
    let lock = format!("/admin/threads/{thread}/lock");

    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::PUT,
        &lock,
        None,
        Some(&author),
    )
    .await;
    ctx.expect(StatusCode::UNAUTHORIZED, Method::PUT, &lock, None, None)
        .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        &lock,
        None,
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::PUT,
        &lock,
        None,
        Some(&admin),
    )
    .await;
    let detail = ctx.get_ok(&format!("/threads/{thread}"), None).await;
    assert_eq!(detail["is_locked"], true);

    // Отвечать нельзя (кроме admin), но своё править можно.
    for user in [&alice, &author] {
        ctx.expect(
            StatusCode::FORBIDDEN,
            Method::POST,
            &format!("/threads/{thread}/posts"),
            Some(json!({ "body": "X" })),
            Some(user),
        )
        .await;
    }
    ctx.post(&admin, &thread, "Тема закрыта", None).await;
    ctx.expect(
        StatusCode::OK,
        Method::PATCH,
        &format!("/posts/{post}"),
        Some(json!({ "body": "Исправлено" })),
        Some(&alice),
    )
    .await;

    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &lock,
        None,
        Some(&author),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &lock,
        None,
        Some(&admin),
    )
    .await;
    ctx.post(&alice, &thread, "Снова открыта", None).await;

    let missing = "/admin/threads/00000000-0000-4000-8000-000000000000/lock";
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::PUT,
        missing,
        None,
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        missing,
        None,
        Some(&admin),
    )
    .await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_deletes_any_thread_and_post(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;
    let admin = ctx.login("admin", "admin").await;
    let thread = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    let first = ctx.post(&alice, &thread, "Спам", None).await;
    ctx.post(&bob, &thread, "Ответ на спам", Some(&first)).await;

    let post_path = format!("/admin/posts/{first}");
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &post_path,
        None,
        Some(&author),
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::DELETE,
        &post_path,
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &post_path,
        None,
        Some(&admin),
    )
    .await;
    // С ответом — заглушка, как и при удалении автором.
    let detail = ctx.get_ok(&format!("/threads/{thread}"), None).await;
    assert_eq!(items(&detail["posts"])[0]["deleted"], true);
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &post_path,
        None,
        Some(&admin),
    )
    .await;

    let thread_path = format!("/admin/threads/{thread}");
    ctx.expect(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &thread_path,
        None,
        Some(&author),
    )
    .await;
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &thread_path,
        None,
        Some(&admin),
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::GET,
        &format!("/threads/{thread}"),
        None,
        None,
    )
    .await;
    ctx.expect(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        &thread_path,
        None,
        Some(&admin),
    )
    .await;
}

// ---------------------------------------------------------------- профиль

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn profile_counts_forum_activity(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let author = ctx.login("author", "author").await;
    let alice = ctx.login("alice", "user").await;
    let thread = ctx.thread(&author, "Тема", &["dune-2021"]).await;
    ctx.thread(&author, "Ещё", &["witcher-3"]).await;
    let first = ctx.post(&alice, &thread, "Раз", None).await;
    ctx.post(&alice, &thread, "Два", None).await;
    ctx.post(&author, &thread, "Ответ", Some(&first)).await;
    // Удалённое (даже заглушкой) не считается.
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/posts/{first}"),
        None,
        Some(&alice),
    )
    .await;

    let profile = ctx
        .get_ok(&format!("/users/{}", author.username), None)
        .await;
    assert_eq!(profile["threads_count"], 2);
    assert_eq!(profile["posts_count"], 1);
    let profile = ctx
        .get_ok(&format!("/users/{}", alice.username), None)
        .await;
    assert_eq!(profile["threads_count"], 0);
    assert_eq!(profile["posts_count"], 1);
}
