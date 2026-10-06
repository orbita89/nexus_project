//! Вход через OAuth-провайдера (против поддельного провайдера на локальном порту) и dev login.

use axum::extract::Form;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use shared::config::{OAuthKind, OAuthProviderConfig};
use shared::{AppState, Config};
use sqlx::PgPool;
use std::collections::HashMap;
use test_utils::{query_param, request, TestResponse};

const AUTH: &str = "/api/v1/auth";
const GOOD_CODE: &str = "good-code";

/// Поддельный OIDC-провайдер: `/token` принимает только [`GOOD_CODE`] с PKCE verifier,
/// `/userinfo` отдаёт `profile`. Возвращает базовый URL.
async fn spawn_provider(profile: Value) -> String {
    let app = Router::new()
        .route(
            "/token",
            post(|Form(form): Form<HashMap<String, String>>| async move {
                let ok = form.get("grant_type").map(String::as_str) == Some("authorization_code")
                    && form.get("code").map(String::as_str) == Some(GOOD_CODE)
                    && form.get("client_secret").map(String::as_str) == Some("secret")
                    && form.get("code_verifier").is_some_and(|v| v.len() >= 43);
                if ok {
                    (
                        StatusCode::OK,
                        Json(json!({ "access_token": "provider-token" })),
                    )
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "invalid_grant" })),
                    )
                }
            }),
        )
        .route(
            "/userinfo",
            get(move |headers: HeaderMap| {
                let profile = profile.clone();
                async move {
                    if headers["authorization"] == "Bearer provider-token" {
                        (StatusCode::OK, Json(profile))
                    } else {
                        (StatusCode::UNAUTHORIZED, Json(json!({})))
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Ctx {
    state: AppState,
    pool: PgPool,
}

impl Ctx {
    async fn new(pool: PgPool, profile: Value) -> Self {
        let base = spawn_provider(profile).await;
        let mut config = Config::from_env();
        config.app_base_url = "http://front.test".into();
        config.oauth_providers = vec![OAuthProviderConfig {
            name: "mock".into(),
            kind: OAuthKind::Oidc,
            client_id: "client".into(),
            client_secret: "secret".into(),
            authorize_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            userinfo_url: format!("{base}/userinfo"),
            scopes: "openid email".into(),
        }];
        let (state, _) = test_utils::state_with_config(pool.clone(), config);
        Self { state, pool }
    }

    async fn get(&self, path: &str) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        request(app, Method::GET, &format!("{AUTH}{path}"), None, None).await
    }

    async fn post(&self, path: &str, body: Value) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        request(
            app,
            Method::POST,
            &format!("{AUTH}{path}"),
            Some(body),
            None,
        )
        .await
    }

    /// start → (браузер у провайдера) → callback. Возвращает адрес, куда callback отправил браузер.
    async fn login_via_provider(&self, code: &str) -> String {
        let start = self.get("/oauth/mock/start").await;
        assert_eq!(start.status, StatusCode::SEE_OTHER);
        let state = query_param(&start.location(), "state").unwrap();

        let callback = self
            .get(&format!("/oauth/mock/callback?code={code}&state={state}"))
            .await;
        assert_eq!(callback.status, StatusCode::SEE_OTHER);
        callback.location()
    }

    /// Полный вход: возвращает ответ `/oauth/exchange`.
    async fn login(&self) -> Value {
        let redirect = self.login_via_provider(GOOD_CODE).await;
        let code = query_param(&redirect, "code")
            .unwrap_or_else(|| panic!("no code in redirect: {redirect}"));
        let response = self.post("/oauth/exchange", json!({ "code": code })).await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
        response.json()
    }
}

fn profile(sub: &str, email: &str, verified: bool) -> Value {
    json!({ "sub": sub, "email": email, "email_verified": verified, "name": "Neo Anderson" })
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn start_redirects_to_provider_with_state_and_pkce(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("1", "neo@example.com", true)).await;

    let response = ctx.get("/oauth/mock/start").await;

    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let location = response.location();
    assert!(location.contains("/authorize?"), "{location}");
    assert_eq!(
        query_param(&location, "client_id").as_deref(),
        Some("client")
    );
    assert_eq!(
        query_param(&location, "response_type").as_deref(),
        Some("code")
    );
    assert_eq!(
        query_param(&location, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(query_param(&location, "code_challenge").is_some());
    assert!(query_param(&location, "state").is_some());
    assert!(query_param(&location, "redirect_uri")
        .unwrap()
        .contains("%2Fapi%2Fv1%2Fauth%2Foauth%2Fmock%2Fcallback"));
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn providers_lists_enabled_and_unknown_provider_is_404(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("1", "neo@example.com", true)).await;

    assert_eq!(
        ctx.get("/oauth/providers").await.json(),
        json!({ "providers": ["mock"] })
    );
    assert_eq!(
        ctx.get("/oauth/google/start").await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn new_user_is_created_and_same_provider_account_logs_into_it_again(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("google-42", "Neo@Example.com", true)).await;

    let first = ctx.login().await;
    let user = &first["user"];
    assert_eq!(user["email"], "Neo@Example.com");
    assert_eq!(user["display_name"], "Neo Anderson");
    assert_eq!(user["role"], "user");
    assert!(!user["email_verified_at"].is_null());

    let second = ctx.login().await;
    assert_eq!(second["user"]["id"], first["user"]["id"]);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn verified_email_links_to_existing_account(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("google-42", "neo@example.com", true)).await;
    let existing: String = sqlx::query_scalar(
        "INSERT INTO users (email, username, password_hash) VALUES ('NEO@example.com', 'neo', 'x')
         RETURNING id::text",
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();

    let tokens = ctx.login().await;

    assert_eq!(tokens["user"]["id"], existing.as_str());
    // Провайдер подтвердил адрес — теперь он подтверждён и у нас.
    assert!(!tokens["user"]["email_verified_at"].is_null());
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn unverified_provider_email_does_not_take_over_existing_account(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("evil-1", "neo@example.com", false)).await;
    sqlx::query(
        "INSERT INTO users (email, username, password_hash) VALUES ('neo@example.com', 'neo', 'x')",
    )
    .execute(&ctx.pool)
    .await
    .unwrap();

    let redirect = ctx.login_via_provider(GOOD_CODE).await;

    assert_eq!(
        redirect,
        "http://front.test/auth/oauth/callback?error=email_in_use"
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn callback_rejects_unknown_state_and_reused_state(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("1", "neo@example.com", true)).await;

    let response = ctx
        .get(&format!(
            "/oauth/mock/callback?code={GOOD_CODE}&state=forged"
        ))
        .await;
    assert_eq!(
        query_param(&response.location(), "error").as_deref(),
        Some("invalid_state")
    );

    let start = ctx.get("/oauth/mock/start").await;
    let state = query_param(&start.location(), "state").unwrap();
    let callback = format!("/oauth/mock/callback?code={GOOD_CODE}&state={state}");
    assert!(query_param(&ctx.get(&callback).await.location(), "code").is_some());
    assert_eq!(
        query_param(&ctx.get(&callback).await.location(), "error").as_deref(),
        Some("invalid_state")
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn provider_errors_are_reported_to_frontend(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("1", "neo@example.com", true)).await;

    // Пользователь нажал «Отмена» у провайдера.
    let start = ctx.get("/oauth/mock/start").await;
    let state = query_param(&start.location(), "state").unwrap();
    let response = ctx
        .get(&format!(
            "/oauth/mock/callback?error=access_denied&state={state}"
        ))
        .await;
    assert_eq!(
        query_param(&response.location(), "error").as_deref(),
        Some("access_denied")
    );

    // Провайдер не принял код.
    let redirect = ctx.login_via_provider("bad-code").await;
    assert_eq!(
        query_param(&redirect, "error").as_deref(),
        Some("provider_error")
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn profile_without_email_is_rejected(pool: PgPool) {
    let ctx = Ctx::new(pool, json!({ "sub": "1", "name": "No Email" })).await;

    let redirect = ctx.login_via_provider(GOOD_CODE).await;

    assert_eq!(
        query_param(&redirect, "error").as_deref(),
        Some("email_required")
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn blocked_user_cannot_log_in_via_provider(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("1", "neo@example.com", true)).await;
    ctx.login().await;
    sqlx::query("UPDATE users SET is_active = false")
        .execute(&ctx.pool)
        .await
        .unwrap();

    let redirect = ctx.login_via_provider(GOOD_CODE).await;

    assert_eq!(
        query_param(&redirect, "error").as_deref(),
        Some("account_blocked")
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn exchange_code_is_single_use(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("1", "neo@example.com", true)).await;
    let redirect = ctx.login_via_provider(GOOD_CODE).await;
    let code = query_param(&redirect, "code").unwrap();

    assert_eq!(
        ctx.post("/oauth/exchange", json!({ "code": code }))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        ctx.post("/oauth/exchange", json!({ "code": code }))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

// ------------------------------------------------------------ dev login

async fn dev_ctx(pool: PgPool, enabled: bool) -> Ctx {
    let mut config = Config::from_env();
    config.dev_login = enabled;
    let (state, _) = test_utils::state_with_config(pool.clone(), config);
    Ctx { state, pool }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn dev_login_is_404_when_disabled(pool: PgPool) {
    let ctx = dev_ctx(pool, false).await;
    let response = ctx
        .post("/dev/login", json!({ "login": "neo@example.com" }))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn dev_login_creates_user_with_role_and_logs_in(pool: PgPool) {
    let ctx = dev_ctx(pool, true).await;

    let response = ctx
        .post(
            "/dev/login",
            json!({ "login": "writer@example.com", "role": "author" }),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
    let tokens = response.json();
    assert_eq!(tokens["user"]["role"], "author");

    // Повторно — тот же пользователь, по username тоже находится.
    let username = tokens["user"]["username"].as_str().unwrap();
    let again = ctx
        .post("/dev/login", json!({ "login": username }))
        .await
        .json();
    assert_eq!(again["user"]["id"], tokens["user"]["id"]);
    assert_eq!(again["user"]["role"], "author");

    let response = ctx
        .post("/dev/login", json!({ "login": "no-such-user" }))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------ привязка из профиля

impl Ctx {
    /// Пользователь, созданный напрямую: id и access-токен.
    async fn user(&self, email: &str) -> (uuid::Uuid, String) {
        let id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, username) VALUES ($1, split_part($1, '@', 1)) RETURNING id",
        )
        .bind(email)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let token = self
            .state
            .jwt
            .issue(id, shared::Role::User, uuid::Uuid::new_v4())
            .unwrap();
        (id, token)
    }

    async fn authed(&self, method: Method, path: &str, token: &str) -> TestResponse {
        let app = nexus::build_app(self.state.clone());
        request(app, method, &format!("{AUTH}{path}"), None, Some(token)).await
    }

    /// Привязка: ссылка из профиля → (браузер у провайдера) → callback. Адрес возврата.
    async fn link_via_provider(&self, token: &str, query: &str) -> String {
        let start = self.authed(Method::POST, "/me/oauth/mock", token).await;
        assert_eq!(start.status, StatusCode::OK);
        let url = start.json()["url"].as_str().unwrap().to_string();
        assert!(url.contains("/authorize?"), "{url}");
        let state = query_param(&url, "state").unwrap();
        let callback = self
            .get(&format!("/oauth/mock/callback?{query}&state={state}"))
            .await;
        assert_eq!(callback.status, StatusCode::SEE_OTHER);
        callback.location()
    }

    async fn linked(&self, token: &str) -> Value {
        let response = self.authed(Method::GET, "/me/oauth", token).await;
        assert_eq!(response.status, StatusCode::OK);
        response.json()
    }
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn link_attaches_provider_to_current_user_with_any_email(pool: PgPool) {
    // У провайдера другой и даже не подтверждённый адрес: привязку подтверждает вход в профиль.
    let ctx = Ctx::new(pool, profile("42", "other@elsewhere.io", false)).await;
    let (neo, token) = ctx.user("neo@example.com").await;

    let back = ctx
        .link_via_provider(&token, &format!("code={GOOD_CODE}"))
        .await;
    assert_eq!(back, "http://front.test/settings/accounts?linked=mock");
    let linked = ctx.linked(&token).await;
    assert_eq!(linked.as_array().unwrap().len(), 1);
    assert_eq!(linked[0]["provider"], "mock");
    assert_eq!(linked[0]["email"], "other@elsewhere.io");

    // Повторная привязка того же аккаунта — без ошибки и без дубля.
    let back = ctx
        .link_via_provider(&token, &format!("code={GOOD_CODE}"))
        .await;
    assert!(back.ends_with("?linked=mock"), "{back}");
    assert_eq!(ctx.linked(&token).await.as_array().unwrap().len(), 1);

    // Теперь вход через провайдера ведёт в этот аккаунт.
    let tokens = ctx.login().await;
    assert_eq!(tokens["user"]["id"], neo.to_string());
    assert_eq!(tokens["user"]["email"], "neo@example.com");
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn link_rejects_account_of_another_user_and_second_account(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("42", "trinity@example.com", true)).await;
    // Аккаунт провайдера уже у trinity (создан входом).
    ctx.login().await;
    let (_, neo) = ctx.user("neo@example.com").await;
    let back = ctx
        .link_via_provider(&neo, &format!("code={GOOD_CODE}"))
        .await;
    assert_eq!(
        back,
        "http://front.test/settings/accounts?error=already_linked"
    );
    assert!(ctx.linked(&neo).await.as_array().unwrap().is_empty());

    // Аккаунт 42 свободен, но у morpheus уже привязан другой аккаунт этого провайдера.
    sqlx::query("DELETE FROM oauth_accounts WHERE provider_user_id = '42'")
        .execute(&ctx.pool)
        .await
        .unwrap();
    let (morpheus_id, morpheus) = ctx.user("morpheus@example.com").await;
    sqlx::query(
        "INSERT INTO oauth_accounts (provider, provider_user_id, user_id) VALUES ('mock', '7', $1)",
    )
    .bind(morpheus_id)
    .execute(&ctx.pool)
    .await
    .unwrap();
    let back = ctx
        .link_via_provider(&morpheus, &format!("code={GOOD_CODE}"))
        .await;
    assert!(back.ends_with("?error=provider_already_linked"), "{back}");

    // Отмена у провайдера при привязке — тоже на страницу настроек.
    let back = ctx.link_via_provider(&neo, "error=access_denied").await;
    assert_eq!(
        back,
        "http://front.test/settings/accounts?error=access_denied"
    );
}

#[sqlx::test(migrator = "nexus::MIGRATOR")]
async fn unlink_and_link_errors(pool: PgPool) {
    let ctx = Ctx::new(pool, profile("42", "neo@example.com", true)).await;
    let tokens = ctx.login().await;
    let token = tokens["access_token"].as_str().unwrap();
    assert_eq!(ctx.linked(token).await.as_array().unwrap().len(), 1);

    let response = ctx.authed(Method::DELETE, "/me/oauth/mock", token).await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert!(ctx.linked(token).await.as_array().unwrap().is_empty());
    let response = ctx.authed(Method::DELETE, "/me/oauth/mock", token).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    let response = ctx.authed(Method::POST, "/me/oauth/unknown", token).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    for (method, path) in [
        (Method::GET, "/me/oauth"),
        (Method::POST, "/me/oauth/mock"),
        (Method::DELETE, "/me/oauth/mock"),
    ] {
        let app = nexus::build_app(ctx.state.clone());
        let response = request(app, method, &format!("{AUTH}{path}"), None, None).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
    }
}
