//! Авторизация: регистрация с подтверждением email, вход по паролю и по ссылке из письма,
//! токены и сессии, сброс и смена пароля, роли и блокировка, ограничение частоты.

use axum::http::{Method, StatusCode};
use axum::Router;
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

    /// Новое приложение со своими счётчиками rate limit (общая БД и почта).
    fn app(&self) -> Router {
        nexus::build_app(self.state.clone())
    }

    async fn post(&self, path: &str, body: Value) -> TestResponse {
        request(
            self.app(),
            Method::POST,
            &format!("{AUTH}{path}"),
            Some(body),
            None,
        )
        .await
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> TestResponse {
        request(
            self.app(),
            method,
            &format!("{AUTH}{path}"),
            body,
            Some(token),
        )
        .await
    }

    /// Токен из последнего письма на адрес.
    fn email_token(&self, to: &str) -> String {
        let email = self
            .outbox
            .last_to(to)
            .unwrap_or_else(|| panic!("no email to {to}"));
        token_from_email(&email.text)
    }

    /// Регистрация + подтверждение email. Возвращает ответ с токенами.
    async fn register_verified(&self, username: &str) -> Value {
        let email = format!("{username}@example.com");
        let response = self
            .post(
                "/register",
                json!({ "email": email, "username": username, "password": PASSWORD }),
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "{:?}",
            response.json()
        );

        let response = self
            .post(
                "/email/verify",
                json!({ "token": self.email_token(&email) }),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        response.json()
    }

    async fn login(&self, login: &str) -> TestResponse {
        self.post("/login", json!({ "login": login, "password": PASSWORD }))
            .await
    }

    /// Пользователь с ролью; возвращает access-токен с этой ролью.
    async fn login_as(&self, username: &str, role: &str) -> String {
        self.register_verified(username).await;
        sqlx::query("UPDATE users SET role = $2::user_role WHERE username = $1")
            .bind(username)
            .bind(role)
            .execute(&self.pool)
            .await
            .unwrap();
        access(&self.login(username).await.json())
    }
}

fn access(tokens: &Value) -> String {
    tokens["access_token"].as_str().unwrap().to_string()
}

// ------------------------------------------------------------ регистрация по паролю

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn register_sends_verification_email_and_returns_unverified_user(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let response = ctx
        .post(
            "/register",
            json!({ "email": "neo@example.com", "username": "neo", "password": PASSWORD }),
        )
        .await;

    assert_eq!(response.status, StatusCode::CREATED);
    let user = response.json();
    assert_eq!(user["role"], "user");
    assert!(user["email_verified_at"].is_null());
    assert!(user.get("password_hash").is_none());
    assert!(
        user.get("access_token").is_none(),
        "no tokens before verification"
    );

    let email = ctx
        .outbox
        .last_to("neo@example.com")
        .expect("verification email");
    assert!(email.text.contains("/auth/verify-email?token="));
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn login_requires_verified_email(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.post(
        "/register",
        json!({ "email": "neo@example.com", "username": "neo", "password": PASSWORD }),
    )
    .await;

    let response = ctx.login("neo").await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.json()["error"], "email not verified");

    // Неверный пароль у неподтверждённого — по-прежнему 401: не раскрываем, что аккаунт есть.
    let response = ctx
        .post(
            "/login",
            json!({ "login": "neo", "password": "wrong-password" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn verify_email_logs_in_and_token_is_single_use(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.post(
        "/register",
        json!({ "email": "neo@example.com", "username": "neo", "password": PASSWORD }),
    )
    .await;
    let token = ctx.email_token("neo@example.com");

    let response = ctx.post("/email/verify", json!({ "token": token })).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(!response.json()["user"]["email_verified_at"].is_null());
    assert!(response.json()["access_token"].is_string());

    let response = ctx.post("/email/verify", json!({ "token": token })).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    assert_eq!(ctx.login("neo").await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn resend_verification_invalidates_previous_link(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.post(
        "/register",
        json!({ "email": "neo@example.com", "username": "neo", "password": PASSWORD }),
    )
    .await;
    let old = ctx.email_token("neo@example.com");

    let response = ctx
        .post(
            "/email/verify/resend",
            json!({ "email": "NEO@example.com" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    let new = ctx.email_token("neo@example.com");
    assert_ne!(old, new);

    let response = ctx.post("/email/verify", json!({ "token": old })).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let response = ctx.post("/email/verify", json!({ "token": new })).await;
    assert_eq!(response.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn register_rejects_duplicates_case_insensitively(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.register_verified("neo").await;

    let response = ctx
        .post(
            "/register",
            json!({ "email": "NEO@example.com", "username": "other", "password": PASSWORD }),
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.json()["error"], "email already registered");

    let response = ctx
        .post(
            "/register",
            json!({ "email": "other@example.com", "username": "NEO", "password": PASSWORD }),
        )
        .await;
    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(response.json()["error"], "username already taken");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn register_validates_input(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let response = ctx
        .post(
            "/register",
            json!({ "email": "neo@example.com", "username": "neo", "password": "short" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // Неразобранное тело — 400 {"error": "..."}, а не 422 с текстом.
    for body in [json!({ "email": "neo@example.com" }), json!({ "email": 1 })] {
        let response = ctx.post("/register", body.clone()).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{body}");
        assert!(response.json()["error"].is_string(), "{body}");
    }
}

// ------------------------------------------------------------ вход по паролю

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn login_by_email_or_username(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.register_verified("neo").await;

    for login in ["neo", "NEO@example.com"] {
        assert_eq!(ctx.login(login).await.status, StatusCode::OK, "{login}");
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn login_fails_the_same_way_for_wrong_password_unknown_user_and_blocked(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.register_verified("neo").await;
    ctx.register_verified("blocked").await;
    sqlx::query("UPDATE users SET is_active = false WHERE username = 'blocked'")
        .execute(&ctx.pool)
        .await
        .unwrap();

    for (login, password) in [
        ("neo", "wrong-password"),
        ("nobody", PASSWORD),
        ("blocked", PASSWORD),
    ] {
        let response = ctx
            .post("/login", json!({ "login": login, "password": password }))
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{login}");
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn me_requires_valid_token(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let token = access(&ctx.register_verified("neo").await);

    let response = ctx.call(Method::GET, "/me", None, &token).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["username"], "neo");

    let response = request(ctx.app(), Method::GET, &format!("{AUTH}/me"), None, None).await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let response = ctx.call(Method::GET, "/me", None, "garbage").await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------ вход по ссылке из письма

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn email_login_creates_account_for_new_address(pool: PgPool) {
    let ctx = Ctx::new(pool);

    let response = ctx
        .post(
            "/email/login",
            json!({ "email": "Trinity+nexus@Example.com" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    let token = ctx.email_token("Trinity+nexus@Example.com");

    let response = ctx
        .post("/email/login/confirm", json!({ "token": token }))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
    let user = &response.json()["user"];
    assert_eq!(user["role"], "user");
    assert!(!user["email_verified_at"].is_null());
    assert!(user["username"].as_str().unwrap().starts_with("trinity_"));

    // Ссылка одноразовая.
    let response = ctx
        .post("/email/login/confirm", json!({ "token": token }))
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // Пароля нет — по паролю не войти.
    let response = ctx
        .post(
            "/login",
            json!({ "login": "Trinity+nexus@Example.com", "password": "anything-at-all" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn email_login_signs_into_existing_account_and_verifies_email(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let created = ctx
        .post(
            "/register",
            json!({ "email": "neo@example.com", "username": "neo", "password": PASSWORD }),
        )
        .await
        .json();

    ctx.post("/email/login", json!({ "email": "neo@example.com" }))
        .await;
    let response = ctx
        .post(
            "/email/login/confirm",
            json!({ "token": ctx.email_token("neo@example.com") }),
        )
        .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["user"]["id"], created["id"]);
    // Переход по ссылке подтверждает email — теперь можно входить и по паролю.
    assert_eq!(ctx.login("neo").await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn email_login_is_silently_ignored_for_blocked_user(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.register_verified("neo").await;
    sqlx::query("UPDATE users SET is_active = false")
        .execute(&ctx.pool)
        .await
        .unwrap();
    let emails_before = ctx.outbox.all().len();

    let response = ctx
        .post("/email/login", json!({ "email": "neo@example.com" }))
        .await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert_eq!(ctx.outbox.all().len(), emails_before);
}

// ------------------------------------------------------------ refresh и сессии

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn refresh_rotates_token(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let first = ctx.register_verified("neo").await["refresh_token"].clone();

    let response = ctx
        .post("/refresh", json!({ "refresh_token": first }))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let second = response.json()["refresh_token"].clone();
    assert_ne!(first, second);

    let response = ctx
        .post("/refresh", json!({ "refresh_token": second }))
        .await;
    assert_eq!(response.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn reusing_revoked_refresh_token_revokes_all_sessions(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let stolen = ctx.register_verified("neo").await["refresh_token"].clone();
    let fresh = ctx
        .post("/refresh", json!({ "refresh_token": stolen }))
        .await
        .json()["refresh_token"]
        .clone();

    let response = ctx
        .post("/refresh", json!({ "refresh_token": stolen }))
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let response = ctx
        .post("/refresh", json!({ "refresh_token": fresh }))
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn logout_revokes_refresh_token(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let refresh = ctx.register_verified("neo").await["refresh_token"].clone();

    let response = ctx
        .post("/logout", json!({ "refresh_token": refresh }))
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let response = ctx
        .post("/refresh", json!({ "refresh_token": refresh }))
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn sessions_list_marks_current_and_revokes_one(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let laptop = ctx.register_verified("neo").await; // сессия при подтверждении email
    let phone = ctx.login("neo").await.json();

    let sessions = ctx
        .call(Method::GET, "/sessions", None, &access(&phone))
        .await
        .json();
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions.iter().filter(|s| s["current"] == true).count(), 1);

    // С телефона завершаем сессию ноутбука.
    let laptop_session = sessions.iter().find(|s| s["current"] == false).unwrap();
    let response = ctx
        .call(
            Method::DELETE,
            &format!("/sessions/{}", laptop_session["id"].as_str().unwrap()),
            None,
            &access(&phone),
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let response = ctx
        .post(
            "/refresh",
            json!({ "refresh_token": laptop["refresh_token"] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    let response = ctx
        .post(
            "/refresh",
            json!({ "refresh_token": phone["refresh_token"] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn cannot_revoke_someone_elses_session(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let neo = ctx.register_verified("neo").await;
    let smith = access(&ctx.register_verified("smith").await);
    let neo_session: String =
        sqlx::query_scalar("SELECT r.id::text FROM refresh_tokens r JOIN users u ON u.id = r.user_id WHERE u.username = 'neo'")
            .fetch_one(&ctx.pool)
            .await
            .unwrap();

    let response = ctx
        .call(
            Method::DELETE,
            &format!("/sessions/{neo_session}"),
            None,
            &smith,
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    let response = ctx
        .post("/refresh", json!({ "refresh_token": neo["refresh_token"] }))
        .await;
    assert_eq!(response.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn logout_all_revokes_every_session(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let first = ctx.register_verified("neo").await;
    let second = ctx.login("neo").await.json();

    let response = ctx
        .call(Method::POST, "/logout-all", None, &access(&second))
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    for tokens in [first, second] {
        let response = ctx
            .post(
                "/refresh",
                json!({ "refresh_token": tokens["refresh_token"] }),
            )
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }
}

// ------------------------------------------------------------ пароль

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn password_reset_via_email_revokes_sessions(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let old_session = ctx.register_verified("neo").await;

    let response = ctx
        .post("/password/forgot", json!({ "email": "neo@example.com" }))
        .await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    let token = ctx.email_token("neo@example.com");

    let response = ctx
        .post(
            "/password/reset",
            json!({ "token": token, "password": "new-password-456" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    assert_eq!(ctx.login("neo").await.status, StatusCode::UNAUTHORIZED);
    let response = ctx
        .post(
            "/login",
            json!({ "login": "neo", "password": "new-password-456" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);

    let response = ctx
        .post(
            "/refresh",
            json!({ "refresh_token": old_session["refresh_token"] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn forgot_password_for_unknown_email_looks_the_same(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let response = ctx
        .post("/password/forgot", json!({ "email": "ghost@example.com" }))
        .await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert!(ctx.outbox.all().is_empty());
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn change_password_keeps_current_session_only(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let other_device = ctx.register_verified("neo").await;
    let this_device = ctx.login("neo").await.json();

    let response = ctx
        .call(
            Method::POST,
            "/password/change",
            Some(
                json!({ "current_password": "wrong-password", "new_password": "new-password-456" }),
            ),
            &access(&this_device),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let response = ctx
        .call(
            Method::POST,
            "/password/change",
            Some(json!({ "current_password": PASSWORD, "new_password": "new-password-456" })),
            &access(&this_device),
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);

    let response = ctx
        .post(
            "/refresh",
            json!({ "refresh_token": this_device["refresh_token"] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let response = ctx
        .post(
            "/refresh",
            json!({ "refresh_token": other_device["refresh_token"] }),
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------ админка

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_endpoints_require_admin_role(pool: PgPool) {
    let ctx = Ctx::new(pool);
    for (username, role) in [("plain", "user"), ("writer", "author")] {
        let token = ctx.login_as(username, role).await;
        let response = ctx.call(Method::GET, "/admin/users", None, &token).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{role}");
    }

    let response = request(
        ctx.app(),
        Method::GET,
        &format!("{AUTH}/admin/users"),
        None,
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);

    let token = ctx.login_as("boss", "admin").await;
    let response = ctx.call(Method::GET, "/admin/users", None, &token).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json().as_array().unwrap().len(), 3);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_promotes_user_to_author(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login_as("boss", "admin").await;
    let user_id = ctx.register_verified("neo").await["user"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let response = ctx
        .call(
            Method::PATCH,
            &format!("/admin/users/{user_id}/role"),
            Some(json!({ "role": "author" })),
            &admin,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["role"], "author");

    assert_eq!(ctx.login("neo").await.json()["user"]["role"], "author");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_blocks_user_and_revokes_sessions(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let admin = ctx.login_as("boss", "admin").await;
    let neo = ctx.register_verified("neo").await;
    let neo_id = neo["user"]["id"].as_str().unwrap();

    let response = ctx
        .call(
            Method::PATCH,
            &format!("/admin/users/{neo_id}/status"),
            Some(json!({ "is_active": false })),
            &admin,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["is_active"], false);

    let response = ctx
        .post("/refresh", json!({ "refresh_token": neo["refresh_token"] }))
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(ctx.login("neo").await.status, StatusCode::UNAUTHORIZED);

    let response = ctx
        .call(
            Method::PATCH,
            &format!("/admin/users/{neo_id}/status"),
            Some(json!({ "is_active": true })),
            &admin,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(ctx.login("neo").await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn admin_cannot_demote_or_block_self(pool: PgPool) {
    let ctx = Ctx::new(pool);
    let token = ctx.login_as("boss", "admin").await;
    let me = ctx.call(Method::GET, "/me", None, &token).await.json();
    let id = me["id"].as_str().unwrap();

    let response = ctx
        .call(
            Method::PATCH,
            &format!("/admin/users/{id}/role"),
            Some(json!({ "role": "user" })),
            &token,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    let response = ctx
        .call(
            Method::PATCH,
            &format!("/admin/users/{id}/status"),
            Some(json!({ "is_active": false })),
            &token,
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

// ------------------------------------------------------------ rate limiting и чистка

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn login_attempts_are_rate_limited_per_account(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.register_verified("neo").await;
    // Одно приложение — общие счётчики, как в проде.
    let app = ctx.app();
    let uri = format!("{AUTH}/login");
    let attempt = || {
        request(
            app.clone(),
            Method::POST,
            &uri,
            Some(json!({ "login": "neo", "password": "wrong-password" })),
            None,
        )
    };

    for _ in 0..10 {
        assert_eq!(attempt().await.status, StatusCode::UNAUTHORIZED);
    }
    let response = attempt().await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn cleanup_deletes_only_expired_tokens(pool: PgPool) {
    let ctx = Ctx::new(pool);
    ctx.register_verified("neo").await;
    ctx.register_verified("smith").await;
    sqlx::query(
        "UPDATE refresh_tokens SET expires_at = now() - interval '1 day'
         WHERE user_id = (SELECT id FROM users WHERE username = 'neo')",
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE email_tokens SET expires_at = now() - interval '1 day'")
        .execute(&ctx.pool)
        .await
        .unwrap();

    let deleted = auth::cleanup::run(&ctx.pool).await.unwrap();
    assert_eq!(deleted.sessions, 1);
    assert_eq!(deleted.email_tokens, 2);

    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM refresh_tokens")
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(left, 1);
}
