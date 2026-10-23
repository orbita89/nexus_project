//! Чат по интересам (комнаты сущностей, людей и тегов): отправка и «прочитано» по WebSocket, история,
//! мои чаты, правка, удаление и заглушение по REST. Приложение на случайном порту, события — по
//! общей шине `AppState`.
//!
//! Сущности — `fixtures/catalog.sql`, пользователи создаются через dev login.

use axum::http::{Method, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::time::Duration;
use test_utils::request;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const SOCIAL: &str = "/api/v1/social";
const ROOM: &str = "entity:dune-2021";
const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(300);
/// Комнаты из миграции `20261111100000`: 5 системных + 10 основных.
const SEEDED_ROOMS: i64 = 15;
const SYSTEM_ROOMS: usize = 5;

/// Процентное кодирование значения query-параметра: кириллица в URI недопустима.
fn enc(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Каналы из ответа `authenticated`.
fn subscribed(auth: &Value) -> Vec<&str> {
    auth["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|channel| channel.as_str().unwrap())
        .collect()
}

struct Ctx {
    state: AppState,
    addr: SocketAddr,
}

struct User {
    id: String,
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

    /// Вошедший клиент.
    async fn authed(&self, user: &User) -> Client {
        let mut client = self.client().await;
        client
            .request(
                json!({ "type": "auth", "token": user.token }),
                "authenticated",
            )
            .await;
        client
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
            id: body["user"]["id"].as_str().unwrap().to_string(),
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
        user: Option<&User>,
    ) -> Value {
        let app = nexus::build_app(self.state.clone());
        let token = user.map(|u| u.token.as_str());
        let response = request(app, method.clone(), &format!("{SOCIAL}{path}"), body, token).await;
        assert_eq!(response.status, status, "{method} {path}");
        if response.body.is_empty() {
            Value::Null
        } else {
            response.json()
        }
    }

    async fn interest(&self, user: &User, slug: &str) {
        let path = format!("/entities/{slug}/interest");
        self.api(StatusCode::NO_CONTENT, Method::PUT, &path, None, Some(user))
            .await;
    }

    /// «Мои чаты» без системных комнат: они есть у всех и всегда идут первыми, остальные
    /// проверки про них не знают (см. [`system_rooms`]).
    async fn chats(&self, user: &User) -> Vec<Value> {
        let chats = self.all_chats(user).await;
        chats
            .into_iter()
            .filter(|chat| chat["custom"]["system"] != json!(true))
            .collect()
    }

    async fn all_chats(&self, user: &User) -> Vec<Value> {
        let chats = self
            .api(StatusCode::OK, Method::GET, "/chats", None, Some(user))
            .await;
        chats.as_array().unwrap().clone()
    }

    async fn history(&self, query: &str) -> Value {
        let path = format!("/chat/entity/dune-2021/messages{query}");
        self.api(StatusCode::OK, Method::GET, &path, None, None)
            .await
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

    async fn expect(&mut self, kind: &str) -> Value {
        let message = self
            .next(WAIT)
            .await
            .unwrap_or_else(|| panic!("no {kind} message"));
        assert_eq!(message["type"], kind, "{message}");
        message
    }

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

    async fn say(&mut self, body: &str) -> Value {
        let reply = self
            .request(
                json!({ "type": "chat.send", "room": ROOM, "body": body, "client_id": "c1" }),
                "chat.sent",
            )
            .await;
        assert_eq!(reply["client_id"], "c1");
        assert_eq!(reply["room"], ROOM);
        reply["message"].clone()
    }
}

/// Отправка по WebSocket: ответ `chat.sent`, событие с сообщением подписчикам комнаты (гостю по
/// `subscribe`, вошедшему — по интересу), история, мои чаты с непрочитанными, `chat.read` гасит
/// счётчик и приходит на другие устройства.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn message_sent_over_websocket_reaches_room_and_history(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;
    ctx.interest(&bob, "dune-2021").await;

    let mut guest = ctx.client().await;
    guest
        .request(
            json!({ "type": "subscribe", "channels": [ROOM] }),
            "subscribed",
        )
        .await;
    let mut bob_ws = ctx.authed(&bob).await;
    let mut bob_phone = ctx.authed(&bob).await;
    let mut alice_ws = ctx.authed(&alice).await;

    let message = alice_ws.say("  Кто смотрел?  ").await;
    assert_eq!(message["body"], "Кто смотрел?");
    assert_eq!(message["author"]["id"], alice.id.as_str());
    assert_eq!(message["deleted"], false);
    // У alice нет интереса и ручной подписки — события ей нет.
    alice_ws.silent().await;

    for client in [&mut guest, &mut bob_ws, &mut bob_phone] {
        let event = client.event("chat.message.created").await;
        assert_eq!(event["channels"], json!([ROOM]));
        assert_eq!(event["data"]["message"], message);
    }

    let history = ctx.history("").await;
    assert_eq!(history["items"], json!([message]));
    assert_eq!(history["next_before"], Value::Null);

    let chats = ctx.chats(&bob).await;
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["room"], ROOM);
    assert_eq!(chats[0]["entity"]["slug"], "dune-2021");
    assert_eq!(chats[0]["last_message"]["id"], message["id"]);
    assert_eq!(chats[0]["unread"], 1);
    assert_eq!(chats[0]["muted"], false);
    // Без интересов чатов нет; своё сообщение — прочитано.
    assert!(ctx.chats(&alice).await.is_empty());

    let saved = bob_ws
        .request(
            json!({ "type": "chat.read", "room": ROOM, "message_id": message["id"] }),
            "chat.read_saved",
        )
        .await;
    assert_eq!(saved["message_id"], message["id"]);
    for client in [&mut bob_ws, &mut bob_phone] {
        let event = client.event("chat.read").await;
        assert_eq!(event["channels"], json!(["user:me"]));
        assert_eq!(event["data"]["message_id"], message["id"]);
    }
    assert_eq!(ctx.chats(&bob).await[0]["unread"], 0);

    // Ответ на сообщение.
    let reply = bob_ws
        .request(
            json!({ "type": "chat.send", "room": ROOM, "body": "Я", "reply_to": message["id"] }),
            "chat.sent",
        )
        .await;
    assert_eq!(reply["message"]["reply_to"], message["id"]);
    assert_eq!(reply["client_id"], Value::Null);
}

/// Ошибки `chat.send` / `chat.read`: гость, неизвестная комната, пустой и длинный текст,
/// `reply_to` не из комнаты, чужое сообщение в `chat.read`, лимит частоты.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn chat_errors_over_websocket(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;

    let mut guest = ctx.client().await;
    let error = guest
        .request(
            json!({ "type": "chat.send", "room": ROOM, "body": "x", "client_id": "g" }),
            "error",
        )
        .await;
    assert_eq!(error["error"], "not authenticated");
    assert_eq!(error["client_id"], "g");

    let mut ws = ctx.authed(&alice).await;
    let other = ws
        .request(
            json!({ "type": "chat.send", "room": "entity:dune-part-two-2024", "body": "x" }),
            "chat.sent",
        )
        .await;
    let long = "я".repeat(2001);
    for (message, text) in [
        (
            json!({ "type": "chat.send", "room": "thread:1", "body": "x" }),
            "unknown chat room: thread:1",
        ),
        (
            json!({ "type": "chat.send", "room": "entity:nope", "body": "x" }),
            "unknown entity in channel: entity:nope",
        ),
        (
            json!({ "type": "chat.send", "room": ROOM, "body": "  " }),
            "body must not be empty",
        ),
        (
            json!({ "type": "chat.send", "room": ROOM, "body": long }),
            "body is longer than 2000 chars",
        ),
        (
            json!({ "type": "chat.send", "room": ROOM, "body": "x", "reply_to": other["message"]["id"] }),
            "reply_to: no such message in this room",
        ),
        (
            json!({ "type": "chat.read", "room": ROOM, "message_id": other["message"]["id"] }),
            "not found",
        ),
    ] {
        let error = ws.request(message, "error").await;
        assert_eq!(error["error"], text);
    }

    // Лимит: уже 1 сообщение, ещё 4 проходят, шестое — нет.
    for _ in 0..4 {
        ws.say("x").await;
    }
    let error = ws
        .request(
            json!({ "type": "chat.send", "room": ROOM, "body": "x", "client_id": "c6" }),
            "error",
        )
        .await;
    assert_eq!(error["error"], "too many messages");
    assert_eq!(error["retry_after"], 10);
    assert_eq!(error["client_id"], "c6");
}

/// REST: правка — только автор, удаление — автор или admin (заглушка в истории), события
/// `chat.message.updated` / `deleted` с id, сообщение по id, заглушение, страницы истории.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn edit_delete_mute_and_history_pages(pool: PgPool) {
    let ctx = Ctx::new(pool.clone()).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;
    let admin = ctx.login("admin", "admin").await;
    ctx.interest(&bob, "dune-2021").await;

    let mut alice_ws = ctx.authed(&alice).await;
    let message = alice_ws.say("Первое").await;
    let id = message["id"].as_str().unwrap();
    let mut bob_ws = ctx.authed(&bob).await;
    let path = format!("/chat/messages/{id}");

    let body = Some(json!({ "body": "Правка" }));
    ctx.api(
        StatusCode::FORBIDDEN,
        Method::PATCH,
        &path,
        body.clone(),
        Some(&bob),
    )
    .await;
    ctx.api(
        StatusCode::UNAUTHORIZED,
        Method::PATCH,
        &path,
        body.clone(),
        None,
    )
    .await;
    let edited = ctx
        .api(StatusCode::OK, Method::PATCH, &path, body, Some(&alice))
        .await;
    assert_eq!(edited["body"], "Правка");
    assert!(edited["edited_at"].is_string());
    let event = bob_ws.event("chat.message.updated").await;
    assert_eq!(
        event["data"],
        json!({ "kind": "entity", "target_id": message["target_id"], "message_id": id })
    );
    assert_eq!(
        ctx.api(StatusCode::OK, Method::GET, &path, None, None)
            .await,
        edited
    );

    ctx.api(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        &path,
        None,
        Some(&bob),
    )
    .await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &path,
        None,
        Some(&admin),
    )
    .await;
    bob_ws.event("chat.message.deleted").await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &path,
        None,
        Some(&alice),
    )
    .await;
    bob_ws.silent().await;
    let stub = ctx
        .api(StatusCode::OK, Method::GET, &path, None, None)
        .await;
    assert_eq!(stub["deleted"], true);
    assert_eq!(stub["body"], Value::Null);
    assert_eq!(stub["author"], Value::Null);
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::PATCH,
        &path,
        Some(json!({ "body": "x" })),
        Some(&alice),
    )
    .await;
    // Удалённое не считается непрочитанным.
    assert_eq!(ctx.chats(&bob).await[0]["unread"], 0);
    let missing = format!("/chat/messages/{}", uuid::Uuid::new_v4());
    ctx.api(StatusCode::NOT_FOUND, Method::GET, &missing, None, None)
        .await;

    // Заглушение.
    let mute = "/chat/entity/dune-2021/mute";
    ctx.api(StatusCode::NO_CONTENT, Method::PUT, mute, None, Some(&bob))
        .await;
    ctx.api(StatusCode::NO_CONTENT, Method::PUT, mute, None, Some(&bob))
        .await;
    assert_eq!(ctx.chats(&bob).await[0]["muted"], true);
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        mute,
        None,
        Some(&bob),
    )
    .await;
    assert_eq!(ctx.chats(&bob).await[0]["muted"], false);
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::PUT,
        "/chat/entity/nope/mute",
        None,
        Some(&bob),
    )
    .await;

    // Страницы истории: ещё 4 сообщения (лимит частоты обходим вставкой).
    sqlx::query(
        "INSERT INTO chat_messages (room_id, author_id, body)
         SELECT m.room_id, $1::uuid, 'm' || n FROM chat_messages m, generate_series(1, 4) n
         WHERE m.id = $2::uuid ORDER BY n",
    )
    .bind(&bob.id)
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let page = ctx.history("?limit=2").await;
    let bodies: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["m3", "m4"]);
    let before = page["next_before"].as_str().unwrap();
    let page = ctx.history(&format!("?limit=10&before={before}")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 3);
    assert_eq!(page["items"][0]["deleted"], true);
    assert_eq!(page["next_before"], Value::Null);
    // Свои сообщения непрочитанными не считаются.
    assert_eq!(ctx.chats(&bob).await[0]["unread"], 0);
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/chat/entity/dune-2021/messages?limit=0",
        None,
        None,
    )
    .await;
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/chat/entity/nope/messages",
        None,
        None,
    )
    .await;
}

/// Этап 2: интересы к людям и тегам, их комнаты. Автоподписка на `person:` / `tag:` при `auth` и
/// на лету (`interest.added`), сообщения в эти каналы, «мои чаты» всех трёх видов, история по
/// `/chat/{kind}/{slug}`, неизвестный вид — 404.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn person_and_tag_rooms(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    let person = "/people/denis-villeneuve/interest";
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        person,
        None,
        Some(&bob),
    )
    .await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        person,
        None,
        Some(&bob),
    )
    .await;
    let own = ctx
        .api(StatusCode::OK, Method::GET, person, None, Some(&bob))
        .await;
    assert_eq!(own["person"]["slug"], "denis-villeneuve");
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        person,
        None,
        Some(&alice),
    )
    .await;
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::PUT,
        "/people/nope/interest",
        None,
        Some(&bob),
    )
    .await;
    let list = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/interests/people",
            None,
            Some(&bob),
        )
        .await;
    assert_eq!(list["total"], 1);

    // Подключён до интереса к тегу: подписка приходит на лету.
    let mut bob_ws = ctx.client().await;
    let auth = bob_ws
        .request(
            json!({ "type": "auth", "token": bob.token }),
            "authenticated",
        )
        .await;
    // Кроме системных комнат, которые подписаны у всех.
    assert!(subscribed(&auth).contains(&"person:denis-villeneuve"));
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/tags/sci-fi/interest",
        None,
        Some(&bob),
    )
    .await;
    let added = bob_ws.event("interest.added").await;
    assert!(added["data"]["tag_id"].is_string());
    let tags = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/interests/tags",
            None,
            Some(&bob),
        )
        .await;
    assert_eq!(tags["items"][0]["tag"]["slug"], "sci-fi");

    let mut alice_ws = ctx.authed(&alice).await;
    for room in ["person:denis-villeneuve", "tag:sci-fi"] {
        let sent = alice_ws
            .request(
                json!({ "type": "chat.send", "room": room, "body": room }),
                "chat.sent",
            )
            .await;
        assert_eq!(sent["room"], room);
        let event = bob_ws.event("chat.message.created").await;
        assert_eq!(event["channels"], json!([room]));
        assert_eq!(event["data"]["message"], sent["message"]);
    }

    let chats = ctx.chats(&bob).await;
    let rooms: Vec<&str> = chats.iter().map(|c| c["room"].as_str().unwrap()).collect();
    assert_eq!(rooms, ["tag:sci-fi", "person:denis-villeneuve"]);
    assert_eq!(chats[0]["kind"], "tag");
    assert_eq!(chats[0]["tag"]["slug"], "sci-fi");
    assert_eq!(chats[0]["entity"], Value::Null);
    assert_eq!(chats[1]["person"]["slug"], "denis-villeneuve");
    assert_eq!(chats[1]["unread"], 1);

    let history = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/person/denis-villeneuve/messages",
            None,
            None,
        )
        .await;
    assert_eq!(history["items"][0]["body"], "person:denis-villeneuve");
    assert_eq!(history["items"][0]["kind"], "person");
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/chat/room/x/messages",
        None,
        None,
    )
    .await;
    let error = alice_ws
        .request(
            json!({ "type": "chat.send", "room": "tag:nope", "body": "x" }),
            "error",
        )
        .await;
    assert_eq!(error["error"], "unknown tag in channel: tag:nope");

    // Интерес убран — подписка снята.
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/tags/sci-fi/interest",
        None,
        Some(&bob),
    )
    .await;
    bob_ws.event("interest.removed").await;
    alice_ws
        .request(
            json!({ "type": "chat.send", "room": "tag:sci-fi", "body": "x" }),
            "chat.sent",
        )
        .await;
    bob_ws.silent().await;
}

/// Этап 3: свои комнаты админа. Создание (только admin, slug проверяется, занят — 409), список и
/// комната по slug, вступление (как интерес: канал `room:` на лету, комната в «моих чатах»),
/// сообщения, правка и удаление комнаты админом с событиями, выход.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_rooms(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let admin = ctx.login("admin", "admin").await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    let body =
        json!({ "slug": "cyberpunk-fans", "title": " Киберпанк ", "description": "Обо всём" });
    ctx.api(
        StatusCode::FORBIDDEN,
        Method::POST,
        "/admin/chat/rooms",
        Some(body.clone()),
        Some(&alice),
    )
    .await;
    for slug in ["", "Cyber", "a--b", "-a", "a b"] {
        ctx.api(
            StatusCode::BAD_REQUEST,
            Method::POST,
            "/admin/chat/rooms",
            Some(json!({ "slug": slug, "title": "x" })),
            Some(&admin),
        )
        .await;
    }
    let room = ctx
        .api(
            StatusCode::CREATED,
            Method::POST,
            "/admin/chat/rooms",
            Some(body.clone()),
            Some(&admin),
        )
        .await;
    assert_eq!(room["title"], "Киберпанк");
    assert_eq!(room["members_count"], 0);
    ctx.api(
        StatusCode::CONFLICT,
        Method::POST,
        "/admin/chat/rooms",
        Some(body),
        Some(&admin),
    )
    .await;
    let list = ctx
        .api(StatusCode::OK, Method::GET, "/chat/rooms", None, None)
        .await;
    assert_eq!(list["total"], SEEDED_ROOMS + 1);
    // Системные комнаты — первыми.
    assert_eq!(list["items"][0]["slug"], "general");
    assert_eq!(list["items"][0]["system"], true);
    assert_eq!(
        ctx.api(
            StatusCode::OK,
            Method::GET,
            "/chat/rooms/cyberpunk-fans",
            None,
            None
        )
        .await,
        room
    );
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/chat/rooms/nope",
        None,
        None,
    )
    .await;

    let mut bob_ws = ctx.authed(&bob).await;
    let membership = "/chat/rooms/cyberpunk-fans/membership";
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        membership,
        None,
        Some(&bob),
    )
    .await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        membership,
        None,
        Some(&bob),
    )
    .await;
    let added = bob_ws.event("interest.added").await;
    assert_eq!(added["data"]["room_id"], room["id"]);
    bob_ws.silent().await;

    let mut alice_ws = ctx.authed(&alice).await;
    let sent = alice_ws
        .request(
            json!({ "type": "chat.send", "room": "room:cyberpunk-fans", "body": "Привет" }),
            "chat.sent",
        )
        .await;
    assert_eq!(sent["message"]["kind"], "room");
    assert_eq!(sent["message"]["target_id"], room["id"]);
    let event = bob_ws.event("chat.message.created").await;
    assert_eq!(event["channels"], json!(["room:cyberpunk-fans"]));

    let chats = ctx.chats(&bob).await;
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["room"], "room:cyberpunk-fans");
    assert_eq!(chats[0]["custom"]["members_count"], 1);
    assert_eq!(chats[0]["custom"]["messages_count"], 1);
    assert_eq!(chats[0]["unread"], 1);
    let history = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/room/cyberpunk-fans/messages",
            None,
            None,
        )
        .await;
    assert_eq!(history["items"][0]["body"], "Привет");
    // Новый вход: канал комнаты подписан сам.
    let mut bob_tablet = ctx.client().await;
    let auth = bob_tablet
        .request(
            json!({ "type": "auth", "token": bob.token }),
            "authenticated",
        )
        .await;
    assert!(subscribed(&auth).contains(&"room:cyberpunk-fans"));

    let path = "/admin/chat/rooms/cyberpunk-fans";
    let updated = ctx
        .api(
            StatusCode::OK,
            Method::PATCH,
            path,
            Some(json!({ "title": "Киберпанк 2077", "description": "" })),
            Some(&admin),
        )
        .await;
    assert_eq!(updated["title"], "Киберпанк 2077");
    assert_eq!(updated["description"], Value::Null);
    bob_ws.event("chat.room.updated").await;

    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        membership,
        None,
        Some(&bob),
    )
    .await;
    bob_ws.event("interest.removed").await;
    assert!(ctx.chats(&bob).await.is_empty());

    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        path,
        None,
        Some(&admin),
    )
    .await;
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::DELETE,
        path,
        None,
        Some(&admin),
    )
    .await;
    let error = alice_ws
        .request(
            json!({ "type": "chat.send", "room": "room:cyberpunk-fans", "body": "x" }),
            "error",
        )
        .await;
    assert_eq!(
        error["error"],
        "unknown room in channel: room:cyberpunk-fans"
    );
}

/// Состояние комнаты для открытой панели (`GET /chat/{kind}/{slug}`): гостю `me: null`,
/// вошедшему — в «моих чатах» ли, заглушена ли, непрочитанные; `joined` в списке своих комнат.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn room_info_for_viewer(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let admin = ctx.login("admin", "admin").await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    let path = "/chat/entity/dune-2021";
    let guest = ctx.api(StatusCode::OK, Method::GET, path, None, None).await;
    assert_eq!(guest["room"], ROOM);
    assert_eq!(guest["entity"]["slug"], "dune-2021");
    assert_eq!(guest["last_message"], Value::Null);
    assert_eq!(guest["me"], Value::Null);
    let fresh = ctx
        .api(StatusCode::OK, Method::GET, path, None, Some(&bob))
        .await;
    assert_eq!(
        fresh["me"],
        json!({ "member": false, "muted": false, "unread": 0, "last_read_id": null })
    );

    let mut alice_ws = ctx.authed(&alice).await;
    let message = alice_ws.say("Привет").await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/chat/entity/dune-2021/mute",
        None,
        Some(&bob),
    )
    .await;
    let info = ctx
        .api(StatusCode::OK, Method::GET, path, None, Some(&bob))
        .await;
    assert_eq!(info["last_message"]["id"], message["id"]);
    assert_eq!(
        info["me"],
        json!({ "member": false, "muted": true, "unread": 1, "last_read_id": null })
    );
    let own = ctx
        .api(StatusCode::OK, Method::GET, path, None, Some(&alice))
        .await;
    assert_eq!(own["me"]["unread"], 0);
    assert_eq!(own["me"]["last_read_id"], message["id"]);
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/chat/entity/nope",
        None,
        None,
    )
    .await;
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        "/chat/planet/x",
        None,
        None,
    )
    .await;

    ctx.api(
        StatusCode::CREATED,
        Method::POST,
        "/admin/chat/rooms",
        Some(json!({ "slug": "books", "title": "Книги" })),
        Some(&admin),
    )
    .await;
    let rooms = ctx
        .api(StatusCode::OK, Method::GET, "/chat/rooms", None, None)
        .await;
    assert_eq!(rooms["items"][0]["joined"], Value::Null);
    let rooms = ctx
        .api(StatusCode::OK, Method::GET, "/chat/rooms", None, Some(&bob))
        .await;
    assert_eq!(rooms["items"][0]["joined"], false);
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/chat/rooms/books/membership",
        None,
        Some(&bob),
    )
    .await;
    let room = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/rooms/books",
            None,
            Some(&bob),
        )
        .await;
    assert_eq!(room["joined"], true);
    let info = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/room/books",
            None,
            Some(&bob),
        )
        .await;
    assert_eq!(info["me"]["member"], true);
    assert_eq!(info["custom"]["joined"], true);
    assert_eq!(ctx.chats(&bob).await[0]["custom"]["joined"], true);
}

/// Системные комнаты: участник — каждый, они первыми в «моих чатах», канал подписан без
/// `subscribe`, выйти и изменить нельзя, гость читает.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn system_rooms(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let admin = ctx.login("admin", "admin").await;
    let alice = ctx.login("alice", "user").await;

    let chats = ctx.all_chats(&alice).await;
    let system: Vec<&str> = chats
        .iter()
        .take(SYSTEM_ROOMS)
        .map(|chat| chat["custom"]["slug"].as_str().unwrap())
        .collect();
    assert_eq!(system, ["general", "movies", "series", "books", "games"]);
    assert!(chats.iter().take(SYSTEM_ROOMS).all(|chat| {
        chat["custom"]["system"] == json!(true) && chat["custom"]["joined"] == json!(true)
    }));
    // Участников у системной столько же, сколько пользователей (строк chat_room_members нет).
    assert_eq!(chats[0]["custom"]["members_count"], 2);
    assert_eq!(chats[0]["custom"]["owner"], Value::Null);

    // Гость читает системную комнату и видит её в каталоге комнат.
    let room = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/rooms/general",
            None,
            None,
        )
        .await;
    assert_eq!(room["system"], true);
    assert_eq!(room["joined"], Value::Null);
    ctx.api(
        StatusCode::OK,
        Method::GET,
        "/chat/room/general/messages",
        None,
        None,
    )
    .await;
    let info = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/room/general",
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(info["me"]["member"], true);

    // Вступить — 204 без записи, выйти, изменить, удалить и пожаловаться нельзя.
    for (status, method, path, body, user) in [
        (
            StatusCode::NO_CONTENT,
            Method::PUT,
            "/chat/rooms/general/membership",
            None,
            &alice,
        ),
        (
            StatusCode::FORBIDDEN,
            Method::DELETE,
            "/chat/rooms/general/membership",
            None,
            &alice,
        ),
        (
            StatusCode::FORBIDDEN,
            Method::PATCH,
            "/chat/rooms/general",
            Some(json!({ "title": "Своё" })),
            &alice,
        ),
        (
            StatusCode::FORBIDDEN,
            Method::DELETE,
            "/chat/rooms/general",
            None,
            &alice,
        ),
        (
            StatusCode::FORBIDDEN,
            Method::POST,
            "/chat/rooms/general/report",
            Some(json!({ "reason": "Не нравится" })),
            &alice,
        ),
        // Админу системная тоже закрыта.
        (
            StatusCode::FORBIDDEN,
            Method::PATCH,
            "/admin/chat/rooms/general",
            Some(json!({ "title": "Своё" })),
            &admin,
        ),
        (
            StatusCode::FORBIDDEN,
            Method::DELETE,
            "/admin/chat/rooms/general",
            None,
            &admin,
        ),
    ] {
        ctx.api(status, method, path, body, Some(user)).await;
    }
    assert_eq!(ctx.all_chats(&alice).await.len(), SYSTEM_ROOMS);

    // Канал системной комнаты подписан без subscribe.
    let mut ws = ctx.client().await;
    let auth = ws
        .request(
            json!({ "type": "auth", "token": alice.token }),
            "authenticated",
        )
        .await;
    assert!(subscribed(&auth).contains(&"room:general"));
}

/// Своя комната пользователя: создание, владелец, правка и удаление владельцем, лимиты,
/// зарезервированные слаги и жалобы.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn own_rooms(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let admin = ctx.login("admin", "admin").await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    let body =
        json!({ "slug": "cyberpunk-fans", "title": " Киберпанк ", "description": "Обо всём" });
    // Гостю — 401.
    ctx.api(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        "/chat/rooms",
        Some(body.clone()),
        None,
    )
    .await;
    let room = ctx
        .api(
            StatusCode::CREATED,
            Method::POST,
            "/chat/rooms",
            Some(body.clone()),
            Some(&alice),
        )
        .await;
    assert_eq!(room["title"], "Киберпанк");
    assert_eq!(room["system"], false);
    assert_eq!(room["owner"]["id"], alice.id);
    // Автор — сразу участник: комната в его чатах, канал подписан.
    assert_eq!(room["members_count"], 1);
    assert_eq!(ctx.chats(&alice).await[0]["room"], "room:cyberpunk-fans");

    // Занятый и зарезервированный слаг — 409, кривой — 400.
    ctx.api(
        StatusCode::CONFLICT,
        Method::POST,
        "/chat/rooms",
        Some(body),
        Some(&bob),
    )
    .await;
    for slug in ["general", "offtop", "admin", "search"] {
        ctx.api(
            StatusCode::CONFLICT,
            Method::POST,
            "/chat/rooms",
            Some(json!({ "slug": slug, "title": "Своё" })),
            Some(&bob),
        )
        .await;
    }
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::POST,
        "/chat/rooms",
        Some(json!({ "slug": "Не Слаг", "title": "Своё" })),
        Some(&bob),
    )
    .await;

    // Третья за сутки исчерпывает суточный лимит → 429 с retry_after.
    for n in 2..=3 {
        ctx.api(
            StatusCode::CREATED,
            Method::POST,
            "/chat/rooms",
            Some(json!({ "slug": format!("room-{n}"), "title": format!("Комната {n}") })),
            Some(&alice),
        )
        .await;
    }
    let limited = ctx
        .api(
            StatusCode::TOO_MANY_REQUESTS,
            Method::POST,
            "/chat/rooms",
            Some(json!({ "slug": "room-4", "title": "Комната 4" })),
            Some(&alice),
        )
        .await;
    assert_eq!(limited["code"], "rate_limited");
    assert!(limited["retry_after"].as_u64().unwrap() > 0);

    // Чужую комнату не изменить и не удалить.
    let path = "/chat/rooms/cyberpunk-fans";
    ctx.api(
        StatusCode::FORBIDDEN,
        Method::PATCH,
        path,
        Some(json!({ "title": "Моё" })),
        Some(&bob),
    )
    .await;
    ctx.api(
        StatusCode::FORBIDDEN,
        Method::DELETE,
        path,
        None,
        Some(&bob),
    )
    .await;

    // Жалоба: повторная не создаёт дубля, админ видит список.
    let report = "/chat/rooms/cyberpunk-fans/report";
    for _ in 0..2 {
        ctx.api(
            StatusCode::NO_CONTENT,
            Method::POST,
            report,
            Some(json!({ "reason": "Реклама" })),
            Some(&bob),
        )
        .await;
    }
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::POST,
        report,
        Some(json!({ "reason": " " })),
        Some(&bob),
    )
    .await;
    let reported = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/admin/chat/rooms/reports",
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(reported["total"], 1);
    assert_eq!(reported["items"][0]["reports_count"], 1);
    assert_eq!(reported["items"][0]["room"]["slug"], "cyberpunk-fans");
    ctx.api(
        StatusCode::FORBIDDEN,
        Method::GET,
        "/admin/chat/rooms/reports",
        None,
        Some(&bob),
    )
    .await;

    // Владелец правит и удаляет свою.
    let updated = ctx
        .api(
            StatusCode::OK,
            Method::PATCH,
            path,
            Some(json!({ "title": "Киберпанк 2077", "description": "" })),
            Some(&alice),
        )
        .await;
    assert_eq!(updated["title"], "Киберпанк 2077");
    assert_eq!(updated["description"], Value::Null);
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        path,
        None,
        Some(&alice),
    )
    .await;
    ctx.api(StatusCode::NOT_FOUND, Method::GET, path, None, None)
        .await;
    // Удалённая освободила место: можно завести взамен (суточный лимит считает заведённые).
    ctx.api(
        StatusCode::TOO_MANY_REQUESTS,
        Method::POST,
        "/chat/rooms",
        Some(json!({ "slug": "cyberpunk-fans", "title": "Снова" })),
        Some(&alice),
    )
    .await;

    // Админ правит и удаляет чужую комнату, у его собственной владельца нет.
    let admins = ctx
        .api(
            StatusCode::CREATED,
            Method::POST,
            "/admin/chat/rooms",
            Some(json!({ "slug": "staff", "title": "Площадка" })),
            Some(&admin),
        )
        .await;
    assert_eq!(admins["owner"], Value::Null);
    ctx.api(
        StatusCode::OK,
        Method::PATCH,
        "/chat/rooms/room-2",
        Some(json!({ "title": "Поправил админ" })),
        Some(&admin),
    )
    .await;
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/chat/rooms/room-2",
        None,
        Some(&admin),
    )
    .await;
}

/// Активность комнат: `messages_24h`, `last_message_at` и `?sort=active|members|new`.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn room_activity_and_sort(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    // Две свои комнаты: в «тихой» больше участников, в «живой» — сообщения.
    for (slug, title) in [("quiet", "Тихая"), ("loud", "Живая")] {
        ctx.api(
            StatusCode::CREATED,
            Method::POST,
            "/chat/rooms",
            Some(json!({ "slug": slug, "title": title })),
            Some(&alice),
        )
        .await;
    }
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::PUT,
        "/chat/rooms/quiet/membership",
        None,
        Some(&bob),
    )
    .await;
    let mut ws = ctx.authed(&alice).await;
    for body in ["Раз", "Два"] {
        ws.request(
            json!({ "type": "chat.send", "room": "room:loud", "body": body }),
            "chat.sent",
        )
        .await;
    }

    let loud = ctx
        .api(StatusCode::OK, Method::GET, "/chat/rooms/loud", None, None)
        .await;
    assert_eq!(loud["messages_24h"], 2);
    assert!(loud["last_message_at"].is_string());
    let quiet = ctx
        .api(StatusCode::OK, Method::GET, "/chat/rooms/quiet", None, None)
        .await;
    assert_eq!(quiet["messages_24h"], 0);
    assert_eq!(quiet["last_message_at"], Value::Null);

    // Системные всегда первыми, поэтому смотрим на первую несистемную.
    let first_of = |list: &Value| -> String {
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|room| room["system"] == json!(false))
            .unwrap()["slug"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Без sort — по активности; «тихая» всплывает только по участникам.
    for (sort, expected) in [
        ("", "loud"),
        ("active", "loud"),
        ("members", "quiet"),
        ("new", "loud"),
    ] {
        let path = format!("/chat/rooms?limit=100&sort={sort}");
        let path = if sort.is_empty() {
            "/chat/rooms?limit=100".to_string()
        } else {
            path
        };
        let list = ctx
            .api(StatusCode::OK, Method::GET, &path, None, None)
            .await;
        assert_eq!(first_of(&list), expected, "sort={sort}");
    }
    // Неизвестный sort — 400.
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::GET,
        "/chat/rooms?sort=nope",
        None,
        None,
    )
    .await;
}

/// Упоминания: `@username` шлёт `chat.mention` названному, себя и несуществующих пропускает.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn mentions_notify_named_users(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    let mut bob_ws = ctx.authed(&bob).await;
    let mut alice_ws = ctx.authed(&alice).await;
    let sent = alice_ws
        .request(
            json!({ "type": "chat.send", "room": "room:general", "body": "@bob глянь, @nobody тоже" }),
            "chat.sent",
        )
        .await;

    // Бобу прилетает само сообщение (он в системной комнате) и следом упоминание.
    bob_ws.event("chat.message.created").await;
    let mention = bob_ws.event("chat.mention").await;
    assert_eq!(mention["channels"], json!(["user:me"]));
    assert_eq!(mention["data"]["kind"], "room");
    assert_eq!(mention["data"]["message"]["id"], sent["message"]["id"]);
    assert_eq!(
        mention["data"]["message"]["body"],
        "@bob глянь, @nobody тоже"
    );

    // Себя упомянуть — тишина (кроме самого сообщения в канале комнаты).
    alice_ws
        .request(
            json!({ "type": "chat.send", "room": "room:general", "body": "@alice сам себе" }),
            "chat.sent",
        )
        .await;
    bob_ws.event("chat.message.created").await;
    bob_ws.silent().await;
}

/// Поиск по комнатам (`?q=`), по сообщениям (`/chat/search`) и по пользователям (`/users?q=`).
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn search_rooms_messages_and_users(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    ctx.api(
        StatusCode::CREATED,
        Method::POST,
        "/chat/rooms",
        Some(json!({ "slug": "cyberpunk-fans", "title": "Киберпанк", "description": "Про неон" })),
        Some(&alice),
    )
    .await;

    // Поиск по комнатам: по названию, по описанию и по куску слова.
    for (q, expected) in [("Киберпанк", 1), ("неон", 1), ("берпан", 1), ("бузина", 0)]
    {
        let path = format!("/chat/rooms?limit=100&q={}", enc(q));
        let list = ctx
            .api(StatusCode::OK, Method::GET, &path, None, None)
            .await;
        assert_eq!(list["total"], expected, "q={q}");
        if expected == 1 {
            assert_eq!(list["items"][0]["slug"], "cyberpunk-fans", "q={q}");
        }
    }
    // Системные комнаты тоже ищутся по названию.
    let found = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/rooms?q={}", enc("Сериалы")),
            None,
            None,
        )
        .await;
    assert_eq!(found["items"][0]["slug"], "series");
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::GET,
        &format!("/chat/rooms?q={}", enc(&"я".repeat(101))),
        None,
        None,
    )
    .await;

    // Сообщения: в системной комнате и в своей.
    let mut ws = ctx.authed(&alice).await;
    for (room, body) in [
        ("room:general", "Смотрел вчера Дюну, отличное кино"),
        ("room:cyberpunk-fans", "Дюна тут ни при чём"),
        ("room:general", "А это про другое"),
    ] {
        ws.request(
            json!({ "type": "chat.send", "room": room, "body": body }),
            "chat.sent",
        )
        .await;
    }

    // Боб состоит только в системных: своя комната Алисы в его поиск не попадает.
    let mine = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}", enc("дюна")),
            None,
            Some(&bob),
        )
        .await;
    assert_eq!(mine["items"].as_array().unwrap().len(), 1);
    assert_eq!(mine["items"][0]["room"], "room:general");
    assert_eq!(
        mine["items"][0]["message"]["body"],
        "Смотрел вчера Дюну, отличное кино"
    );
    // Морфология: «Дюну» находится по «дюна».
    assert_eq!(mine["next_before"], Value::Null);

    // Алиса состоит в обеих — находит оба сообщения.
    let both = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}", enc("дюна")),
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(both["items"].as_array().unwrap().len(), 2);

    // Поиск по одной комнате доступен и гостю.
    let in_room = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}&room=room:cyberpunk-fans", enc("дюна")),
            None,
            None,
        )
        .await;
    assert_eq!(in_room["items"].as_array().unwrap().len(), 1);
    assert_eq!(in_room["items"][0]["room"], "room:cyberpunk-fans");
    // Без room гостю — 401.
    ctx.api(
        StatusCode::UNAUTHORIZED,
        Method::GET,
        &format!("/chat/search?q={}", enc("дюна")),
        None,
        None,
    )
    .await;
    // Курсор: по одному на страницу.
    let page = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}&limit=1", enc("дюна")),
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let next = page["next_before"].as_str().unwrap();
    let rest = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}&limit=1&before={next}", enc("дюна")),
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_ne!(
        rest["items"][0]["message"]["id"],
        page["items"][0]["message"]["id"]
    );

    let q = enc("дюна");
    for path in [
        "/chat/search?q=".to_string(),
        format!("/chat/search?q={q}&limit=0"),
        format!("/chat/search?q={q}&room={}", enc("нечто")),
    ] {
        ctx.api(
            StatusCode::BAD_REQUEST,
            Method::GET,
            &path,
            None,
            Some(&alice),
        )
        .await;
    }
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::GET,
        &format!("/chat/search?q={}&room=room:nope", enc("дюна")),
        None,
        Some(&alice),
    )
    .await;

    // Удалённое сообщение не находится.
    let deleted = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}&room=room:cyberpunk-fans", enc("дюна")),
            None,
            Some(&alice),
        )
        .await;
    let id = deleted["items"][0]["message"]["id"].as_str().unwrap();
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        &format!("/chat/messages/{id}"),
        None,
        Some(&alice),
    )
    .await;
    let gone = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            &format!("/chat/search?q={}&room=room:cyberpunk-fans", enc("дюна")),
            None,
            Some(&alice),
        )
        .await;
    assert!(gone["items"].as_array().unwrap().is_empty());

    // Пользователи для «@».
    let users = ctx
        .api(StatusCode::OK, Method::GET, "/users?q=ali", None, None)
        .await;
    assert_eq!(users[0]["username"], "alice");
    let none = ctx
        .api(StatusCode::OK, Method::GET, "/users?q=zzz", None, None)
        .await;
    assert!(none.as_array().unwrap().is_empty());
    for path in ["/users?q=", "/users?q=ali&limit=0"] {
        ctx.api(StatusCode::BAD_REQUEST, Method::GET, path, None, None)
            .await;
    }
}

/// Канал `rooms`: подписанный клиент узнаёт о новых, изменённых и удалённых комнатах.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn rooms_channel_reports_catalog_changes(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;

    let mut watcher = ctx.client().await;
    let subscribed = watcher
        .request(
            json!({ "type": "subscribe", "channels": ["rooms"] }),
            "subscribed",
        )
        .await;
    assert_eq!(subscribed["channels"], json!(["rooms"]));

    let room = ctx
        .api(
            StatusCode::CREATED,
            Method::POST,
            "/chat/rooms",
            Some(json!({ "slug": "cyberpunk-fans", "title": "Киберпанк" })),
            Some(&alice),
        )
        .await;
    let created = watcher.event("chat.room.created").await;
    assert_eq!(created["channels"], json!(["rooms"]));
    assert_eq!(created["data"]["room_id"], room["id"]);

    ctx.api(
        StatusCode::OK,
        Method::PATCH,
        "/chat/rooms/cyberpunk-fans",
        Some(json!({ "title": "Киберпанк 2077" })),
        Some(&alice),
    )
    .await;
    assert_eq!(
        watcher.event("chat.room.updated").await["data"]["room_id"],
        room["id"]
    );
    ctx.api(
        StatusCode::NO_CONTENT,
        Method::DELETE,
        "/chat/rooms/cyberpunk-fans",
        None,
        Some(&alice),
    )
    .await;
    watcher.event("chat.room.deleted").await;
    watcher.silent().await;
}

/// Скрытые комнаты: системную можно убрать из «Моих чатов», оставшись в ней.
#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn hidden_system_rooms(pool: PgPool) {
    let ctx = Ctx::new(pool).await;
    let alice = ctx.login("alice", "user").await;
    let bob = ctx.login("bob", "user").await;

    assert_eq!(ctx.all_chats(&alice).await.len(), SYSTEM_ROOMS);
    let hidden = "/chat/rooms/general/hidden";

    // Скрыть — и строка уходит из списка; повторно — тоже 204.
    for _ in 0..2 {
        ctx.api(
            StatusCode::NO_CONTENT,
            Method::PUT,
            hidden,
            None,
            Some(&alice),
        )
        .await;
    }
    let chats = ctx.all_chats(&alice).await;
    assert_eq!(chats.len(), SYSTEM_ROOMS - 1);
    assert!(chats
        .iter()
        .all(|chat| chat["custom"]["slug"] != json!("general")));
    // У других не скрыта.
    assert_eq!(ctx.all_chats(&bob).await.len(), SYSTEM_ROOMS);

    // Комната осталась рабочей: панель, история, отправка и подписка на канал.
    let info = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/room/general",
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(info["custom"]["hidden"], true);
    assert_eq!(info["custom"]["joined"], true);
    assert_eq!(info["me"]["member"], true);
    let room = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/rooms/general",
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(room["hidden"], true);
    // Гостю — null, как и joined.
    let guest = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/rooms/general",
            None,
            None,
        )
        .await;
    assert_eq!(guest["hidden"], Value::Null);

    let mut ws = ctx.authed(&alice).await;
    let auth = ws
        .request(
            json!({ "type": "auth", "token": alice.token }),
            "authenticated",
        )
        .await;
    assert!(subscribed(&auth).contains(&"room:general"));
    ws.request(
        json!({ "type": "chat.send", "room": "room:general", "body": "Я ещё тут" }),
        "chat.sent",
    )
    .await;
    let history = ctx
        .api(
            StatusCode::OK,
            Method::GET,
            "/chat/room/general/messages",
            None,
            Some(&alice),
        )
        .await;
    assert_eq!(history["items"][0]["body"], "Я ещё тут");

    // Вернуть; повторно — тоже 204.
    for _ in 0..2 {
        ctx.api(
            StatusCode::NO_CONTENT,
            Method::DELETE,
            hidden,
            None,
            Some(&alice),
        )
        .await;
    }
    assert_eq!(ctx.all_chats(&alice).await.len(), SYSTEM_ROOMS);

    // Несистемную не скрывают — из неё выходят.
    ctx.api(
        StatusCode::CREATED,
        Method::POST,
        "/chat/rooms",
        Some(json!({ "slug": "cyberpunk-fans", "title": "Киберпанк" })),
        Some(&alice),
    )
    .await;
    ctx.api(
        StatusCode::BAD_REQUEST,
        Method::PUT,
        "/chat/rooms/cyberpunk-fans/hidden",
        None,
        Some(&alice),
    )
    .await;
    ctx.api(
        StatusCode::NOT_FOUND,
        Method::PUT,
        "/chat/rooms/nope/hidden",
        None,
        Some(&alice),
    )
    .await;
    ctx.api(StatusCode::UNAUTHORIZED, Method::PUT, hidden, None, None)
        .await;
}
