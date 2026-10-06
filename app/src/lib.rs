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
use utoipa_swagger_ui::{SwaggerUi, Url};

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

/// Отдельная вкладка Swagger UI для каталога: у него много эндпоинтов и свои схемы.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Nexus API: каталог",
        description = "Фильмы, сериалы, книги, игры, люди, теги и поиск.\n\n\
            Чтение — без авторизации. Админка (`/admin/...`) — только роль `admin`: получите токен \
            во вкладке **Nexus API** (`POST /api/v1/auth/login`, пользователь `admin`, пароль \
            `password123`) и вставьте его в **Authorize**. Вкладка переключается в списке \
            «Select a definition» справа вверху."
    ),
    modifiers(&BearerAuth),
    tags(
        (name = "catalog", description = "Каталог: сущности, люди, теги, поиск. Без авторизации"),
        (name = "catalog-admin", description = "Управление каталогом. Только роль admin"),
    )
)]
struct CatalogDoc;

/// OpenAPI-документ: вкладка в Swagger UI (список «Select a definition») и файл в `documents/api/`.
pub struct ApiDocument {
    /// Название в списке Swagger UI.
    pub name: &'static str,
    /// Где отдаётся схема.
    pub url: &'static str,
    /// Файл-контракт в `documents/api/`.
    pub file: &'static str,
    pub spec: OpenApiSpec,
}

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

/// Роуты всех модулей и OpenAPI-документы. Первый документ открывается в Swagger UI по умолчанию.
fn api() -> (Router<AppState>, Vec<ApiDocument>) {
    let (router, main) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .nest(&format!("{API_PREFIX}/auth"), auth::router())
        .nest(&format!("{API_PREFIX}/social"), social::router())
        .split_for_parts();
    let (catalog_router, catalog) = OpenApiRouter::with_openapi(CatalogDoc::openapi())
        .nest(&format!("{API_PREFIX}/catalog"), catalog::router())
        .split_for_parts();

    let docs = vec![
        ApiDocument {
            name: "Nexus API",
            url: "/api-docs/openapi.json",
            file: "openapi.json",
            spec: main,
        },
        ApiDocument {
            name: "Каталог",
            url: "/api-docs/catalog.json",
            file: "catalog.json",
            spec: catalog,
        },
    ];
    (router.merge(catalog_router), docs)
}

/// OpenAPI-документы приложения (то же, что отдаётся на `/api-docs/*.json`).
pub fn openapi_docs() -> Vec<ApiDocument> {
    api().1
}

pub fn build_app(state: AppState) -> Router {
    let (router, docs) = api();
    // WebSocket в OpenAPI не описывается.
    let mut router = router.merge(realtime::router());
    if state.config.api_docs {
        let urls = docs
            .into_iter()
            .map(|doc| (Url::new(doc.name, doc.url), doc.spec))
            .collect();
        // persist_authorization: токен из Authorize сохраняется при переключении вкладок и перезагрузке.
        let config = utoipa_swagger_ui::Config::default().persist_authorization(true);
        router = router.merge(SwaggerUi::new("/docs").urls(urls).config(config));
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
