//! ИИ-преподаватель (`catalog::tutor`): свои ключи пользователя (зашифрованы, наружу —
//! подсказка), обсуждения книги — только свои, вопрос — ответ потоком (SSE) от модели
//! пользователя; без своего ключа — 409 `no_ai_provider`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use shared::{AppState, Config};
use sqlx::PgPool;

async fn token(state: &AppState, login: &str) -> String {
    let response = test_utils::request(
        nexus::build_app(state.clone()),
        Method::POST,
        "/api/v1/auth/dev/login",
        Some(json!({ "login": login, "role": "user" })),
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{:?}", response.json());
    response.json()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn state(pool: PgPool) -> AppState {
    let mut config = Config::from_env();
    config.ai_secret_key = Some("test-secret".into());
    test_utils::state_with_config(pool, config).0
}

async fn send(
    state: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
    token: &str,
) -> test_utils::TestResponse {
    test_utils::request(
        nexus::build_app(state.clone()),
        method,
        &format!("/api/v1/catalog{path}"),
        body,
        Some(token),
    )
    .await
}

/// OpenAI-совместимая модель: отвечает потоком «Мастер — » + «это автор.», с расходом токенов.
async fn fake_model() -> String {
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
            assert_eq!(body["stream"], true);
            assert_eq!(body["messages"][0]["role"], "system");
            let sse = [
                r#"data: {"choices":[{"delta":{"content":"Мастер — "}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"это автор."}}]}"#,
                r#"data: {"choices":[],"usage":{"prompt_tokens":150,"completion_tokens":12}}"#,
                "data: [DONE]",
            ]
            .join("\n\n");
            (
                [("content-type", "text/event-stream")],
                format!("{sse}\n\n"),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn tutor_answers_with_users_own_key(pool: PgPool) {
    let state = state(pool.clone());
    let alice = token(&state, "alice@example.com").await;
    let bob = token(&state, "bob@example.com").await;

    // Только книги.
    let movie = send(
        &state,
        Method::POST,
        "/books/dune-2021/tutor",
        Some(json!({})),
        &alice,
    )
    .await;
    assert_eq!(movie.status, StatusCode::NOT_FOUND);

    let thread = send(
        &state,
        Method::POST,
        "/books/dune-novel/tutor",
        Some(json!({})),
        &alice,
    )
    .await;
    assert_eq!(thread.status, StatusCode::CREATED, "{:?}", thread.json());
    let id = thread.json()["id"].as_str().unwrap().to_string();

    // Без своего ключа — 409 no_ai_provider.
    let question = json!({ "text": "Кто такой Мастер?", "fraction": 0.3 });
    let no_key = send(
        &state,
        Method::POST,
        &format!("/tutor/{id}/messages"),
        Some(question.clone()),
        &alice,
    )
    .await;
    assert_eq!(no_key.status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&no_key.body).contains("no_ai_provider"));

    // Свой ключ: зашифрован, наружу — подсказка; первый — по умолчанию.
    let url = fake_model().await;
    let created = send(
        &state,
        Method::POST,
        "/me/ai-providers",
        Some(json!({ "kind": "openai", "title": "Свой", "base_url": url,
                     "model": "m", "api_key": "sk-secret-key-0001" })),
        &alice,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.json());
    assert_eq!(created.json()["is_default"], true);
    assert_eq!(created.json()["key_hint"], "sk-…0001");
    assert!(!String::from_utf8_lossy(&created.body).contains("sk-secret-key-0001"));
    let raw: Vec<u8> = sqlx::query_scalar("SELECT api_key FROM user_ai_providers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("sk-secret"));
    // Адрес не https — 400.
    let bad = send(
        &state,
        Method::POST,
        "/me/ai-providers",
        Some(json!({ "kind": "openai", "title": "x", "base_url": "http://evil.example", "model": "m", "api_key": "k" })),
        &alice,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    // Чужих провайдеров не видно.
    let others = send(&state, Method::GET, "/me/ai-providers", None, &bob).await;
    assert_eq!(others.json().as_array().unwrap().len(), 0);

    // Вопрос — ответ потоком; ответ сохранён, название — по первому вопросу.
    let answer = send(
        &state,
        Method::POST,
        &format!("/tutor/{id}/messages"),
        Some(question),
        &alice,
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = String::from_utf8_lossy(&answer.body).to_string();
    assert!(body.contains("event: delta"), "{body}");
    assert!(body.contains("event: done"), "{body}");

    let detail = send(&state, Method::GET, &format!("/tutor/{id}"), None, &alice).await;
    let detail = detail.json();
    assert_eq!(detail["title"], "Кто такой Мастер?");
    assert_eq!(detail["entity"]["slug"], "dune-novel");
    assert_eq!(detail["messages"][1]["role"], "assistant");
    assert_eq!(detail["messages"][1]["content"], "Мастер — это автор.");
    assert_eq!(detail["tokens_in"], 150);

    // Чужое обсуждение — 404; профиль — свои обсуждения с книгой.
    let foreign = send(&state, Method::GET, &format!("/tutor/{id}"), None, &bob).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    let own = send(&state, Method::GET, "/me/tutor", None, &alice).await;
    assert_eq!(own.json()["total"], 1);
    assert_eq!(own.json()["items"][0]["entity"]["slug"], "dune-novel");

    let deleted = send(
        &state,
        Method::DELETE,
        &format!("/tutor/{id}"),
        None,
        &alice,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
}

async fn admin_token(state: &AppState) -> String {
    let response = test_utils::request(
        nexus::build_app(state.clone()),
        Method::POST,
        "/api/v1/auth/dev/login",
        Some(json!({ "login": "admin@example.com", "role": "admin" })),
        None,
    )
    .await;
    response.json()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[sqlx::test(migrator = "nexus::MIGRATOR", fixtures("catalog"))]
async fn admin_overrides_prompts_per_kind(pool: PgPool) {
    let state = state(pool);
    let admin = admin_token(&state).await;
    let user = token(&state, "alice@example.com").await;

    // Только admin.
    let denied = send(&state, Method::GET, "/admin/ai/prompts", None, &user).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let list = send(&state, Method::GET, "/admin/ai/prompts", None, &admin).await;
    assert_eq!(list.status, StatusCode::OK);
    let list = list.json();
    assert!(list["scopes"].as_array().unwrap().contains(&json!("book")));
    let system = list["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "tutor.system")
        .unwrap();
    assert!(system["builtin"].as_str().unwrap().contains("{about}"));
    assert_eq!(system["overrides"].as_array().unwrap().len(), 0);

    // Опечатка в подстановке и неизвестная область — 400.
    let typo = send(
        &state,
        Method::PUT,
        "/admin/ai/prompts/tutor.system/book",
        Some(json!({ "template": "Книга {abuot}" })),
        &admin,
    )
    .await;
    assert_eq!(typo.status, StatusCode::BAD_REQUEST);
    let scope = send(
        &state,
        Method::PUT,
        "/admin/ai/prompts/tutor.system/planet",
        Some(json!({ "template": "x" })),
        &admin,
    )
    .await;
    assert_eq!(scope.status, StatusCode::BAD_REQUEST);

    // Для книг — свой текст; для фильмов — встроенный.
    let put = send(
        &state,
        Method::PUT,
        "/admin/ai/prompts/tutor.system/book",
        Some(json!({ "template": "Ты филолог. Книга {about}." })),
        &admin,
    )
    .await;
    assert_eq!(put.status, StatusCode::OK, "{:?}", put.json());
    let book = send(
        &state,
        Method::GET,
        "/admin/ai/prompts/preview?scope=book",
        None,
        &admin,
    )
    .await;
    assert!(book.json()["system"]
        .as_str()
        .unwrap()
        .starts_with("Ты филолог. Книга «Мастер и Маргарита»"));
    let movie = send(
        &state,
        Method::GET,
        "/admin/ai/prompts/preview?scope=movie",
        None,
        &admin,
    )
    .await;
    assert!(movie.json()["system"]
        .as_str()
        .unwrap()
        .starts_with("Ты — преподаватель"));

    // Сброс — снова встроенный; повторный сброс — 404.
    let reset = send(
        &state,
        Method::DELETE,
        "/admin/ai/prompts/tutor.system/book",
        None,
        &admin,
    )
    .await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
    let again = send(
        &state,
        Method::DELETE,
        "/admin/ai/prompts/tutor.system/book",
        None,
        &admin,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}
