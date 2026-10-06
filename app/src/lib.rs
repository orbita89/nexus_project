//! nexus — модульный монолит. Здесь собирается HTTP-приложение из роутеров модулей;
//! `main.rs` только поднимает ресурсы и запускает сервер. Разделение нужно, чтобы тесты
//! могли собрать то же приложение без сети.
//! Модули друг от друга не зависят: всё общее приходит через `shared::AppState`.

use axum::{Json, Router};
use shared::{AppState, API_PREFIX};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::OpenApi as OpenApiSpec;
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

/// Миграции из `migrations/`, встроенные в бинарник.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Nexus API",
        description = "API платформы Nexus.\n\n\
            Как авторизоваться: выполните `POST /api/v1/auth/login` (тестовые пользователи: \
            `admin`, `author`, `user`, пароль `password123`), скопируйте `access_token` и вставьте его \
            в **Authorize** справа вверху. Токен живёт 15 минут.\n\n\
            Письма (подтверждение email, вход по ссылке, сброс пароля) в dev-окружении приходят в \
            Mailpit: http://localhost:8025. Токен — параметр `token=` в ссылке из письма."
    ),
    modifiers(&BearerAuth),
    tags(
        (name = "auth", description = "Регистрация, вход, токены, сессии, пароль"),
        (name = "oauth", description = "Вход через внешних провайдеров (Google, GitHub, Яндекс)"),
        (name = "admin", description = "Управление пользователями. Только роль admin"),
        (name = "dev", description = "Только для разработки: включается DEV_LOGIN=true"),
        (name = "system", description = "Служебное"),
    )
)]
struct ApiDoc;

/// Схема `bearer` для кнопки Authorize; эндпоинты ссылаются на неё через `security(("bearer" = []))`.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut OpenApiSpec) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .build(),
            ),
        );
    }
}

/// Роуты всех модулей и собранная из них OpenAPI-схема.
fn api() -> (Router<AppState>, OpenApiSpec) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .nest(&format!("{API_PREFIX}/auth"), auth::router())
        .nest(&format!("{API_PREFIX}/catalog"), catalog::router())
        .nest(&format!("{API_PREFIX}/social"), social::router())
        .split_for_parts()
}

/// OpenAPI-схема приложения (то же, что отдаётся на `/api-docs/openapi.json`).
pub fn openapi() -> OpenApiSpec {
    api().1
}

pub fn build_app(state: AppState) -> Router {
    let (router, spec) = api();
    // WebSocket в OpenAPI не описывается.
    let mut router = router.merge(realtime::router());
    if state.config.api_docs {
        router = router.merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", spec));
    }
    router
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(serde::Serialize, utoipa::ToSchema)]
struct Health {
    #[schema(example = "nexus")]
    service: &'static str,
    #[schema(example = "ok")]
    status: &'static str,
}

/// Приложение живо.
#[utoipa::path(get, path = "/health", tag = "system", responses((status = 200, body = Health)))]
async fn health() -> Json<Health> {
    Json(Health {
        service: "nexus",
        status: "ok",
    })
}
