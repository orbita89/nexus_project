//! realtime: настоящий WebSocket против приложения на случайном порту. События создаются
//! обычными запросами к API с тем же `AppState` (общая шина событий).
//!
//! Сущности — `fixtures/catalog.sql`, пользователи создаются через dev login.

use axum::http::{Method, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use shared::{AppState, Config, Role};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::time::Duration;
use test_utils::request;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

const SOCIAL: &str = "/api/v1/social";
/// Сколько ждём сообщение, которое должно прийти.
const WAIT: Duration = Duration::from_secs(5);
/// Сколько ждём, чтобы убедиться, что сообщение не придёт.
const QUIET: Duration = Duration::from_millis(300);

struct Ctx {
    state: AppState,
    addr: SocketAddr,
}

struct User {
    id: Uuid,
    token: String,
}

impl Ctx {
    async fn new(pool: PgPool) -> Self {
        let mut config = Config::from_env();
        config.dev_login = true;
        let (state, _) = test_utils::state_with_config(pool, config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = nexus::build_app(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { state, addr }
    }

    async fn client(&self) -> Client {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", self.addr))
            .await
            .unwrap();
        Client { ws }
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
        assert_eq!(response.status, StatusCode::OK);
        let body = response.json();
        User {
            id: body["user"]["id"].as_str().unwrap().parse().unwrap(),
            token: body["access_token"].as_str().unwrap().to_string(),
        }
    }

    /// Запрос к social, ожидающий `status`: тело ответа.
    async fn api(
        &self,
        status: StatusCode,
        method: Method,
        path: &str,
        body: Option<Value>,
        user: &User,
    ) -> Value {
        let app = nexus::build_app(self.state.clone());
        let response = request(
            app,
            method.clone(),
            &format!("{SOCIAL}{path}"),
            body,
            Some(&user.token),
        )
        .await;
        assert_eq!(response.status, status, "{method} {path}");
        if response.body.is_empty() {
            Value::Null
        } else {
            response.json()
        }
    }

    async fn thread(&self, author: &User, entities: &[&str]) -> String {
        let body = json!({ "title": "Тема", "body": "Текст", "entities": entities });
        let thread = self
            .api(
                StatusCode::CREATED,
                Method::POST,
                "/threads",
                Some(body),
                author,
            )
            .await;
        thread["id"].as_str().unwrap().to_string()
    }

    async fn post(&self, user: &User, thread: &str, parent: Option<&str>) -> String {
        let body = json!({ "body": "Сообщение", "parent_id": parent });
        let path = format!("/threads/{thread}/posts");
        let post = self
            .api(StatusCode::CREATED, Method::POST, &path, Some(body), user)
            .await;
        post["id"].as_str().unwrap().to_string()
    }

    async fn review(&self, user: &User, slug: &str) {
        let app = nexus::build_app(self.state.clone());
        let path = format!("{SOCIAL}/entities/{slug}/review");
        let body = Some(json!({ "rating": 8 }));
        let response = request(app, Method::PUT, &path, body, Some(&user.token)).await;
        assert!(response.status.is_success());
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn send(&mut self, message: Value) {
        self.ws
            .send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
    }

    /// Следующее JSON-сообщение (ping/pong пропускаются) или `None`, если за `wait` ничего нет.
    async fn next(&mut self, wait: Duration) -> Option<Value> {
        loop {
            let message = tokio::time::timeout(wait, self.ws.next()).await.ok()??;
            match message.unwrap() {
                Message::Text(text) => return Some(serde_json::from_str(&text).unwrap()),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    /// Следующее сообщение должно быть типа `kind`.
    async fn expect(&mut self, kind: &str) -> Value {
        let message = self
            .next(WAIT)
            .await
            .unwrap_or_else(|| panic!("no {kind} message"));
        assert_eq!(message["type"], kind, "{message}");
        message
    }

    /// Следующее сообщение — событие `event`.
    async fn event(&mut self, event: &str) -> Value {
        let message = self.expect("event").await;
        assert_eq!(message["event"], event, "{message}");
        message
    }

    async fn silent(&mut self) {
        if let Some(message) = self.next(QUIET).await {
            panic!("unexpected message: {message}");
        }
    }

    async fn request(&mut self, message: Value, reply: &str) -> Value {
        self.send(message).await;
        self.expect(reply).await
    }

    async fn subscribe(&mut self, channels: &[&str]) -> Value {
        self.request(
            json!({ "type": "subscribe", "channels": channels }),
            "subscribed",
        )
        .await
    }

    async fn auth(&mut self, token: &str) -> Value {
        self.request(json!({ "type": "auth", "token": token }), "authenticated")
            .await
    }
}

fn names(message: &Value) -> Vec<&str> {
    message["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect()
}

// ---------------------------------------------------------------- гость и каналы

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn guest_gets_events_of_subscribed_channels(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let author = ctx.login("author", "author").await;
    let user = ctx.login("user", "user").await;
    let mut guest = ctx.client().await;

    let subscribed = guest.subscribe(&["entity:dune-2021"]).await;
    assert_eq!(names(&subscribed), ["entity:dune-2021"]);

    // Новая тема про книгу и фильм приходит по каналу фильма; в данных только id.
    let thread = ctx.thread(&author, &["dune-novel", "dune-2021"]).await;
    let created = guest.event("thread.created").await;
    assert_eq!(names(&created), ["entity:dune-2021"]);
    assert_eq!(
        created["data"],
        json!({ "thread_id": thread, "author_id": author.id })
    );

    // Подписка на тему: сообщение подходит под оба канала, но приходит один раз.
    guest.subscribe(&[&format!("thread:{thread}")]).await;
    let post = ctx.post(&user, &thread, None).await;
    let created = guest.event("post.created").await;
    assert_eq!(
        names(&created),
        [format!("thread:{thread}").as_str(), "entity:dune-2021"]
    );
    assert_eq!(created["data"]["post_id"], post.as_str());
    assert_eq!(created["data"]["parent_id"], Value::Null);
    assert!(created["data"].get("body").is_none());

    ctx.api(
        StatusCode::OK,
        Method::PATCH,
        &format!("/posts/{post}"),
        Some(json!({ "body": "Правка" })),
        &user,
    )
    .await;
    guest.event("post.updated").await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/posts/{post}"),
        None,
        &user,
    )
    .await;
    guest.event("post.deleted").await;

    // Рецензии — по каналу сущности; чужая сущность не приходит.
    ctx.review(&user, "witcher-3").await;
    ctx.review(&user, "dune-2021").await;
    let review = guest.event("review.created").await;
    assert_eq!(names(&review), ["entity:dune-2021"]);

    // Отписка: события темы больше не приходят, по сущности — приходят.
    let unsubscribed = guest
        .request(
            json!({ "type": "unsubscribe", "channels": [format!("thread:{thread}")] }),
            "unsubscribed",
        )
        .await;
    assert_eq!(names(&unsubscribed), [format!("thread:{thread}").as_str()]);
    let admin = ctx.login("admin", "admin").await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        &format!("/admin/threads/{thread}/lock"),
        None,
        &admin,
    )
    .await;
    let locked = guest.event("thread.updated").await;
    assert_eq!(names(&locked), ["entity:dune-2021"]);
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/threads/{thread}"),
        None,
        &author,
    )
    .await;
    guest.event("thread.deleted").await;
    guest.silent().await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn bad_messages_get_errors(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let author = ctx.login("author", "author").await;
    let mut client = ctx.client().await;

    for channels in [
        json!(["nonsense"]),
        json!(["entity:no-such-entity"]),
        json!(["thread:not-a-uuid"]),
        json!(["user:me"]),
        // Всё или ничего: подписки на dune-2021 не будет.
        json!(["entity:dune-2021", "entity:no-such-entity"]),
    ] {
        let error = client
            .request(
                json!({ "type": "subscribe", "channels": channels }),
                "error",
            )
            .await;
        assert!(error["error"].is_string());
    }
    for message in [
        json!({ "type": "dance" }),
        json!({ "type": "subscribe" }),
        json!({ "type": "auth", "token": "not-a-jwt" }),
    ] {
        client.request(message, "error").await;
    }
    client.send(json!("just a string")).await;
    client.expect("error").await;
    client.request(json!({ "type": "ping" }), "pong").await;

    ctx.thread(&author, &["dune-2021"]).await;
    client.silent().await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn manual_channels_are_limited(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let mut client = ctx.client().await;
    let threads: Vec<String> = (0..realtime::MAX_CHANNELS)
        .map(|_| format!("thread:{}", Uuid::new_v4()))
        .collect();
    let threads: Vec<&str> = threads.iter().map(String::as_str).collect();
    client.subscribe(&threads).await;
    // Повторная подписка на те же каналы лимит не тратит.
    client.subscribe(&threads[..10]).await;
    let error = client
        .request(
            json!({ "type": "subscribe", "channels": ["entity:dune-2021"] }),
            "error",
        )
        .await;
    assert!(error["error"].as_str().unwrap().contains("200"));
}

// ---------------------------------------------------------------- вход и интересы

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn auth_adds_personal_channel_and_interests(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let author = ctx.login("author", "author").await;
    let user = ctx.login("user", "user").await;
    let other = ctx.login("other", "user").await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/entities/witcher-3/interest",
        None,
        &user,
    )
    .await;

    let mut client = ctx.client().await;
    let authenticated = client.auth(&user.token).await;
    assert_eq!(authenticated["user_id"], user.id.to_string());
    assert!(authenticated["expires_at"].is_string());
    assert_eq!(names(&authenticated), ["user:me", "entity:witcher-3"]);

    // Интерес: рецензия на «Ведьмака» приходит без ручной подписки.
    ctx.review(&other, "witcher-3").await;
    let review = client.event("review.created").await;
    assert_eq!(names(&review), ["entity:witcher-3"]);

    // Ответ на своё сообщение — в user:me; на свои ответы уведомлений нет.
    let thread = ctx.thread(&author, &["no-date"]).await;
    let mine = ctx.post(&user, &thread, None).await;
    ctx.post(&user, &thread, Some(&mine)).await;
    let reply = ctx.post(&other, &thread, Some(&mine)).await;
    let notified = client.event("reply.created").await;
    assert_eq!(names(&notified), ["user:me"]);
    assert_eq!(notified["data"]["post_id"], reply.as_str());
    assert_eq!(notified["data"]["parent_id"], mine.as_str());
    client.silent().await;

    // Интерес добавлен по API (например, с другого устройства) — подписка появляется сама.
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/entities/dune-novel/interest",
        None,
        &user,
    )
    .await;
    let added = client.event("interest.added").await;
    assert_eq!(names(&added), ["user:me"]);
    ctx.thread(&author, &["dune-novel"]).await;
    let created = client.event("thread.created").await;
    assert_eq!(names(&created), ["entity:dune-novel"]);

    // Убран — подписка пропадает.
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/entities/dune-novel/interest",
        None,
        &user,
    )
    .await;
    client.event("interest.removed").await;
    ctx.thread(&author, &["dune-novel"]).await;
    client.silent().await;

    // Чужие личные события не приходят.
    let other_post = ctx.post(&other, &thread, None).await;
    ctx.post(&author, &thread, Some(&other_post)).await;
    client.silent().await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn expired_token_downgrades_to_guest(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let author = ctx.login("author", "author").await;
    let user = ctx.login("user", "user").await;
    let other = ctx.login("other", "user").await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/entities/witcher-3/interest",
        None,
        &user,
    )
    .await;
    let short = |ttl| {
        ctx.state
            .jwt
            .issue_with_ttl(user.id, Role::User, Uuid::new_v4(), ttl)
            .unwrap()
    };

    let mut client = ctx.client().await;
    client.subscribe(&["entity:dune-2021"]).await;
    client.auth(&short(2)).await;
    // До конца меньше минуты — предупреждение сразу.
    client.expect("auth_expiring").await;
    let expired = client.expect("auth_expired").await;
    assert_eq!(names(&expired), ["user:me", "entity:witcher-3"]);

    // Теперь гость: личное и интересы не приходят, ручная подписка осталась.
    let thread = ctx.thread(&author, &["dune-2021"]).await;
    client.event("thread.created").await;
    let mine = ctx.post(&user, &thread, None).await;
    client.event("post.created").await;
    ctx.post(&other, &thread, Some(&mine)).await;
    client.event("post.created").await;
    ctx.review(&other, "witcher-3").await;
    client.silent().await;

    // Продление до истечения: новый auth — и auth_expired не будет.
    client.auth(&short(2)).await;
    client.expect("auth_expiring").await;
    client.auth(&user.token).await;
    client.silent().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    client.silent().await;
    ctx.review(&other, "witcher-3").await;
    client.event("review.updated").await;
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn connections_per_user_are_limited(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let user = ctx.login("user", "user").await;
    let mut clients = Vec::new();
    for _ in 0..realtime::MAX_CONNECTIONS_PER_USER {
        let mut client = ctx.client().await;
        client.auth(&user.token).await;
        clients.push(client);
    }
    let mut extra = ctx.client().await;
    extra
        .request(json!({ "type": "auth", "token": user.token }), "error")
        .await;
    // Гостем то же соединение работает.
    extra.subscribe(&["entity:dune-2021"]).await;
    // Повторный auth в уже вошедшем соединении место не занимает.
    clients[0].auth(&user.token).await;

    // Соединение закрыто — место освобождается (сервер замечает закрытие не мгновенно).
    clients.pop().unwrap().ws.close(None).await.unwrap();
    for attempt in 0.. {
        extra
            .send(json!({ "type": "auth", "token": user.token }))
            .await;
        let reply = extra.next(WAIT).await.unwrap();
        if reply["type"] == "authenticated" {
            break;
        }
        assert!(attempt < 20, "slot was not released: {reply}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
