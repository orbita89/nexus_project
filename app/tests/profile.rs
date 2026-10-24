//! Свой профиль (модуль auth): username, имя, аватар, смена email.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::mail::Outbox;
use shared::AppState;
use sqlx::PgPool;
use test_utils::{request, token_from_email, TestResponse};

const PASSWORD: &str = "password123";
const AUTH: &str = "/api/v1/auth";

struct Ctx {
    state: AppState,
    outbox: Outbox,
    pool: PgPool,
}

impl Ctx {
    fn new(pool: PgPool) -> Self {
        let (state, outbox) = test_utils::state(pool.clone());
        Self {
            state,
            outbox,
            pool,
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        request(app, method, &format!("{AUTH}{path}"), body, token).await
    }

    /// Запрос, ожидающий `status`: тело ответа. Ошибка — всегда `{"error": "..."}`.
    async fn expect(
        &self,
        status: StatusCode,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> Value {
        let response = self.call(method.clone(), path, body, token).await;
        let text = String::from_utf8_lossy(&response.body).to_string();
        assert_eq!(response.status, status, "{method} {path}: {text}");
        if response.body.is_empty() {
            return Value::Null;
        }
        let body = response.json();
        if !status.is_success() {
            assert!(body["error"].is_string(), "{method} {path}: {body}");
            assert!(body["code"].is_string(), "{method} {path}: {body}");
        }
        body
    }

    /// Приложение с каталогом аватаров во временной папке: тесты не делят её между собой.
    fn with_avatars(pool: PgPool, dir: &str) -> Self {
        let mut config = shared::Config::from_env();
        config.avatars_dir = dir.to_string();
        let (state, outbox) = test_utils::state_with_config(pool.clone(), config);
        Self {
            state,
            outbox,
            pool,
        }
    }

    /// Загрузка фото профиля: статус и разобранное тело.
    async fn upload(&self, bytes: Vec<u8>, token: Option<&str>) -> (StatusCode, Value) {
        let app = nexus::build_app(self.state.clone());
        let response =
            test_utils::upload(app, &format!("{AUTH}/me/avatar"), "me.png", bytes, token).await;
        let body = if response.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&response.body).unwrap_or(Value::Null)
        };
        (response.status, body)
    }

    /// Запрос за файлом: тело не JSON, разбирать его нельзя.
    async fn file(&self, status: StatusCode, path: &str) -> Vec<u8> {
        let app = nexus::build_app(self.state.clone());
        let response = request(app, Method::GET, path, None, None).await;
        assert_eq!(response.status, status, "GET {path}");
        response.body.to_vec()
    }

    async fn patch_me(&self, status: StatusCode, body: Value, token: &str) -> Value {
        self.expect(status, Method::PATCH, "/me", Some(body), Some(token))
            .await
    }

    fn email_token(&self, to: &str) -> String {
        let email = self
            .outbox
            .last_to(to)
            .unwrap_or_else(|| panic!("no email to {to}"));
        token_from_email(&email.text)
    }

    /// Регистрация + подтверждение email: ответ с токенами.
    async fn register(&self, username: &str) -> Value {
        let email = format!("{username}@example.com");
        let body = json!({ "email": email, "username": username, "password": PASSWORD });
        self.expect(
            StatusCode::CREATED,
            Method::POST,
            "/register",
            Some(body),
            None,
        )
        .await;
        let body = json!({ "token": self.email_token(&email) });
        self.expect(
            StatusCode::OK,
            Method::POST,
            "/email/verify",
            Some(body),
            None,
        )
        .await
    }

    async fn login(&self, login: &str) -> TestResponse {
        let body = json!({ "login": login, "password": PASSWORD });
        self.call(Method::POST, "/login", Some(body), None).await
    }

    /// username сменили 31 день назад: можно менять снова.
    async fn age_username_change(&self, username: &str) {
        sqlx::query(
            "UPDATE users SET username_changed_at = now() - interval '31 days'
             WHERE username = $1::citext",
        )
        .bind(username)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

fn access(tokens: &Value) -> String {
    tokens["access_token"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------- профиль

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn update_profile_fields(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let neo = access(&ctx.register("neo").await);

    let user = ctx
        .patch_me(
            StatusCode::OK,
            json!({ "display_name": "  Нео ", "avatar_url": "https://example.com/neo.png" }),
            &neo,
        )
        .await;
    assert_eq!(user["display_name"], "Нео");
    assert_eq!(user["avatar_url"], "https://example.com/neo.png");
    assert_eq!(user["username"], "neo");
    assert_eq!(user["username_changed_at"], Value::Null);

    // Не переданное не меняется; null и пустая строка очищают.
    let user = ctx
        .patch_me(StatusCode::OK, json!({ "display_name": null }), &neo)
        .await;
    assert_eq!(user["display_name"], Value::Null);
    assert_eq!(user["avatar_url"], "https://example.com/neo.png");
    let user = ctx
        .patch_me(StatusCode::OK, json!({ "avatar_url": " " }), &neo)
        .await;
    assert_eq!(user["avatar_url"], Value::Null);

    let me = ctx
        .expect(StatusCode::OK, Method::GET, "/me", None, Some(&neo))
        .await;
    assert_eq!(me["avatar_url"], Value::Null);

    // «О себе» и видимость аналитики — те же правила: не переданное не меняется.
    let user = ctx
        .patch_me(
            StatusCode::OK,
            json!({ "bio": "  Смотрю фантастику  ", "stats_public": true }),
            &neo,
        )
        .await;
    assert_eq!(user["bio"], "Смотрю фантастику");
    assert_eq!(user["stats_public"], true);
    let user = ctx
        .patch_me(StatusCode::OK, json!({ "display_name": "Нео" }), &neo)
        .await;
    assert_eq!(user["bio"], "Смотрю фантастику");
    assert_eq!(user["stats_public"], true);
    let user = ctx
        .patch_me(
            StatusCode::OK,
            json!({ "bio": null, "stats_public": false }),
            &neo,
        )
        .await;
    assert_eq!(user["bio"], Value::Null);
    assert_eq!(user["stats_public"], false);

    for bad in [
        json!({ "bio": "x".repeat(1001) }),
        json!({ "display_name": "x".repeat(65) }),
        json!({ "avatar_url": "http://example.com/neo.png" }),
        json!({ "avatar_url": "javascript:alert(1)" }),
        json!({ "username": "нео" }),
        json!({ "username": "ab" }),
        json!({ "email": "other@example.com" }),
        json!({ "role": "admin" }),
    ] {
        ctx.patch_me(StatusCode::BAD_REQUEST, bad, &neo).await;
    }
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::PATCH,
        "/me",
        Some(json!({})),
        None,
    )
    .await;
}

/// Картинка нужного размера, собранная на лету: в фикстурах бинарников не держим.
fn png(width: u32, height: u32) -> Vec<u8> {
    let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([80, 120, 200, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// Фото профиля: загрузка, раздача, замена (старый файл исчезает), удаление.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn avatar_upload_replaces_and_deletes_file(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("nexus-avatars-{}", uuid::Uuid::new_v4()));
    let ctx = Ctx::with_avatars(pool, dir.to_str().unwrap());
    let neo = access(&ctx.register("neo").await);

    // Не картинку не принимаем.
    let (status, error) = ctx
        .upload(b"this is definitely not a png".to_vec(), Some(&neo))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["code"], "not_an_image");
    // И без токена тоже.
    let (status, _) = ctx.upload(png(64, 64), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, user) = ctx.upload(png(1024, 512), Some(&neo)).await;
    assert_eq!(status, StatusCode::OK, "{user}");
    let url = user["avatar_url"].as_str().unwrap().to_string();
    assert!(url.starts_with("/api/v1/auth/avatars/"), "{url}");

    // Файл раздаётся всем и это WebP, уменьшенный до 512 по длинной стороне.
    let bytes = ctx.file(StatusCode::OK, &url).await;
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (512, 256));

    // Новая загрузка даёт новый адрес, старый файл стирается.
    let (_, user) = ctx.upload(png(64, 64), Some(&neo)).await;
    let next = user["avatar_url"].as_str().unwrap().to_string();
    assert_ne!(next, url);
    ctx.file(StatusCode::NOT_FOUND, &url).await;
    ctx.file(StatusCode::OK, &next).await;

    // Свой https-адрес вместо загруженного — файл тоже уходит.
    let user = ctx
        .patch_me(
            StatusCode::OK,
            json!({ "avatar_url": "https://example.com/neo.png" }),
            &neo,
        )
        .await;
    assert_eq!(user["avatar_url"], "https://example.com/neo.png");
    ctx.file(StatusCode::NOT_FOUND, &next).await;

    // Удаление убирает и адрес от провайдера.
    let user = ctx
        .expect(
            StatusCode::OK,
            Method::DELETE,
            "/me/avatar",
            None,
            Some(&neo),
        )
        .await;
    assert_eq!(user["avatar_url"], Value::Null);

    let _ = std::fs::remove_dir_all(&dir);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn username_change_is_unique_and_limited(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let neo = access(&ctx.register("neo").await);
    ctx.register("trinity").await;

    // Занят (без учёта регистра) — 409; тот же самый — не смена.
    ctx.patch_me(StatusCode::CONFLICT, json!({ "username": "Trinity" }), &neo)
        .await;
    let same = ctx
        .patch_me(StatusCode::OK, json!({ "username": "neo" }), &neo)
        .await;
    assert_eq!(same["username_changed_at"], Value::Null);

    let user = ctx
        .patch_me(StatusCode::OK, json!({ "username": " the_one " }), &neo)
        .await;
    assert_eq!(user["username"], "the_one");
    assert!(user["username_changed_at"].is_string());

    // Вход и профиль в social — по новому имени; старое свободно.
    assert_eq!(ctx.login("the_one").await.status, StatusCode::OK);
    assert_eq!(ctx.login("neo").await.status, StatusCode::UNAUTHORIZED);
    let app = nexus::build_app(ctx.state.clone());
    let social = request(app, Method::GET, "/api/v1/social/users/the_one", None, None).await;
    assert_eq!(social.status, StatusCode::OK);

    // Снова — только через 30 дней; имя и аватар менять можно.
    let error = ctx
        .patch_me(StatusCode::BAD_REQUEST, json!({ "username": "neo" }), &neo)
        .await;
    assert!(
        error["error"].as_str().unwrap().contains("30 days"),
        "{error}"
    );
    ctx.patch_me(StatusCode::OK, json!({ "display_name": "Нео" }), &neo)
        .await;
    ctx.age_username_change("the_one").await;
    let user = ctx
        .patch_me(StatusCode::OK, json!({ "username": "neo" }), &neo)
        .await;
    assert_eq!(user["username"], "neo");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn blocked_user_cannot_edit_profile(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let neo = access(&ctx.register("neo").await);
    sqlx::query("UPDATE users SET is_active = false WHERE username = 'neo'")
        .execute(&ctx.pool)
        .await
        .unwrap();
    ctx.patch_me(
        StatusCode::UNAUTHORIZED,
        json!({ "display_name": "X" }),
        &neo,
    )
    .await;
    let body = json!({ "new_email": "x@example.com", "password": PASSWORD });
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        "/me/email",
        Some(body),
        Some(&neo),
    )
    .await;
}

// ---------------------------------------------------------------- смена email

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn change_email_confirms_new_address_and_notifies_old(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let other_device = ctx.register("neo").await;
    let this_device = ctx.login("neo").await.json();
    let token = access(&this_device);
    let start = |body: Value| ctx.call(Method::POST, "/me/email", Some(body), Some(&token));

    for (body, status) in [
        (
            json!({ "new_email": "new@example.com" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "new_email": "new@example.com", "password": "wrong-password" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "new_email": "NEO@example.com", "password": PASSWORD }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "new_email": "not-an-email", "password": PASSWORD }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(start(body.clone()).await.status, status, "{body}");
    }
    ctx.register("trinity").await;
    let taken = json!({ "new_email": "Trinity@example.com", "password": PASSWORD });
    assert_eq!(start(taken).await.status, StatusCode::CONFLICT);

    // Первая ссылка гасится второй (даже на другой адрес).
    let body = json!({ "new_email": "first@example.com", "password": PASSWORD });
    assert_eq!(start(body).await.status, StatusCode::ACCEPTED);
    let first = ctx.email_token("first@example.com");
    let body = json!({ "new_email": "new@example.com", "password": PASSWORD });
    assert_eq!(start(body).await.status, StatusCode::ACCEPTED);
    let link = ctx.outbox.last_to("new@example.com").unwrap();
    assert!(
        link.text.contains("/auth/change-email?token="),
        "{}",
        link.text
    );
    let body = json!({ "token": first });
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::POST,
        "/email/change/confirm",
        Some(body),
        None,
    )
    .await;
    // Пока не подтверждено, email прежний.
    let me = ctx
        .expect(StatusCode::OK, Method::GET, "/me", None, Some(&token))
        .await;
    assert_eq!(me["email"], "neo@example.com");

    // Подтверждение с токеном этого устройства: оно остаётся, остальные выходят.
    let body = json!({ "token": ctx.email_token("new@example.com") });
    let user = ctx
        .expect(
            StatusCode::OK,
            Method::POST,
            "/email/change/confirm",
            Some(body.clone()),
            Some(&token),
        )
        .await;
    assert_eq!(user["email"], "new@example.com");
    assert!(user["email_verified_at"].is_string());
    ctx.expect(
        StatusCode::BAD_REQUEST,
        Method::POST,
        "/email/change/confirm",
        Some(body),
        None,
    )
    .await;

    let notice = ctx.outbox.last_to("neo@example.com").unwrap();
    assert!(notice.text.contains("new@example.com"), "{}", notice.text);
    let refresh = |tokens: &Value| {
        ctx.call(
            Method::POST,
            "/refresh",
            Some(json!({ "refresh_token": tokens["refresh_token"] })),
            None,
        )
    };
    assert_eq!(refresh(&this_device).await.status, StatusCode::OK);
    assert_eq!(
        refresh(&other_device).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(ctx.login("new@example.com").await.status, StatusCode::OK);
    assert_eq!(
        ctx.login("neo@example.com").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn passwordless_user_changes_email_without_password(pool: PgPool) {
    let ctx = Ctx::new(pool);
    // Вход по ссылке: аккаунт без пароля.
    let body = json!({ "email": "link@example.com" });
    ctx.expect(
        StatusCode::ACCEPTED,
        Method::POST,
        "/email/login",
        Some(body),
        None,
    )
    .await;
    let body = json!({ "token": ctx.email_token("link@example.com") });
    let tokens = ctx
        .expect(
            StatusCode::OK,
            Method::POST,
            "/email/login/confirm",
            Some(body),
            None,
        )
        .await;

    let body = json!({ "new_email": "moved@example.com" });
    ctx.expect(
        StatusCode::ACCEPTED,
        Method::POST,
        "/me/email",
        Some(body),
        Some(&access(&tokens)),
    )
    .await;
    // Ссылку открыли на другом устройстве, без входа: все сессии отозваны.
    let body = json!({ "token": ctx.email_token("moved@example.com") });
    let user = ctx
        .expect(
            StatusCode::OK,
            Method::POST,
            "/email/change/confirm",
            Some(body),
            None,
        )
        .await;
    assert_eq!(user["email"], "moved@example.com");
    let refresh = json!({ "refresh_token": tokens["refresh_token"] });
    let response = ctx
        .call(Method::POST, "/refresh", Some(refresh), None)
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn email_taken_before_confirmation_is_conflict(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let neo = access(&ctx.register("neo").await);
    let body = json!({ "new_email": "shared@example.com", "password": PASSWORD });
    ctx.expect(
        StatusCode::ACCEPTED,
        Method::POST,
        "/me/email",
        Some(body),
        Some(&neo),
    )
    .await;
    let token = ctx.email_token("shared@example.com");
    ctx.register("shared").await;

    let body = json!({ "token": token });
    ctx.expect(
        StatusCode::CONFLICT,
        Method::POST,
        "/email/change/confirm",
        Some(body),
        None,
    )
    .await;
    ctx.expect(
        StatusCode::UNAUTHORIZED,
        Method::POST,
        "/me/email",
        Some(json!({ "new_email": "x@example.com" })),
        None,
    )
    .await;
}

/// `has_password`: по нему фронтенд решает, спрашивать ли пароль при смене email.
#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn has_password_reflects_whether_password_is_set(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let neo = ctx.register("neo").await;
    assert_eq!(neo["user"]["has_password"], true);

    // Вход по ссылке создаёт аккаунт без пароля.
    let email = "trinity@example.com";
    let body = json!({ "email": email });
    ctx.expect(
        StatusCode::ACCEPTED,
        Method::POST,
        "/email/login",
        Some(body),
        None,
    )
    .await;
    let body = json!({ "token": ctx.email_token(email) });
    let trinity = ctx
        .expect(
            StatusCode::OK,
            Method::POST,
            "/email/login/confirm",
            Some(body),
            None,
        )
        .await;
    assert_eq!(trinity["user"]["has_password"], false);
    let token = trinity["access_token"].as_str().unwrap();
    let me = ctx
        .expect(StatusCode::OK, Method::GET, "/me", None, Some(token))
        .await;
    assert_eq!(me["has_password"], false);

    // Пароль задан через «забыли пароль».
    let body = json!({ "email": email });
    ctx.expect(
        StatusCode::ACCEPTED,
        Method::POST,
        "/password/forgot",
        Some(body),
        None,
    )
    .await;
    let body = json!({ "token": ctx.email_token(email), "password": PASSWORD });
    ctx.expect(
        StatusCode::NO_CONTENT,
        Method::POST,
        "/password/reset",
        Some(body),
        None,
    )
    .await;
    let response = ctx.login(email).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["user"]["has_password"], true);
}
