//! nexus — модульный монолит. Здесь собирается HTTP-приложение из роутеров модулей;
//! `main.rs` только поднимает ресурсы и запускает сервер. Разделение нужно, чтобы тесты
//! могли собрать то же приложение без сети.
//! Модули друг от друга не зависят: всё общее приходит через `shared::AppState`.

use axum::{Json, Router};
use shared::directory::Directories;
use shared::{AppState, API_PREFIX};
use sqlx::PgPool;
use std::sync::Arc;
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
        (name = "profile", description = "Свой профиль: имя, аватар, email, привязанные провайдеры"),
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

/// Отдельная вкладка Swagger UI для социального модуля.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Nexus API: социальное",
        description = "Рецензии и оценки, подписки на пользователей, коллекции, форум, интересы, лента.\n\n\
            Сущности адресуются по `slug` (как в каталоге), пользователи — по `username`, коллекции, темы \
            и сообщения — по `id`. Ошибки — всегда `{\"error\": \"...\"}`. Чтение публичного — без авторизации; рецензии, подписки, свои коллекции \
            и ответы на форуме — любой вошедший; темы форума — роль `author` и выше; модерация (`/admin/...`) — только роль `admin`. Токен: во вкладке **Nexus API** \
            выполните `POST /api/v1/auth/login` (`user` / `author` / `admin`, пароль `password123`) и вставьте \
            `access_token` в **Authorize**."
    ),
    modifiers(&BearerAuth),
    tags(
        (name = "users", description = "Социальный профиль: счётчики и подписан ли вошедший"),
        (name = "reviews", description = "Рецензии и оценки"),
        (name = "follows", description = "Подписки на пользователей"),
        (name = "collections", description = "Коллекции: приватные видит только владелец"),
        (name = "feed", description = "Лента: записи тех, на кого подписан, затем по интересам, затем популярные темы. Пагинация курсором"),
        (name = "interests", description = "Интересы: на какие сущности подписан пользователь. Список личный; по нему realtime (/ws) присылает события сущностей"),
        (name = "forum", description = "Форум: темы, привязанные к нескольким сущностям, и сообщения с ветками. Темы создаёт роль author и выше, отвечает любой вошедший"),
        (name = "social-admin", description = "Модерация. Только роль admin"),
    )
)]
struct SocialDoc;

/// Справочники модулей-владельцев данных (`shared::directory`): через них модули читают чужие
/// данные, не зная друг о друге.
pub fn directories(db: PgPool) -> Directories {
    Directories {
        entities: Arc::new(catalog::directory::PgEntityDirectory::new(db.clone())),
        users: Arc::new(auth::directory::PgUserDirectory::new(db.clone())),
        interests: Arc::new(social::directory::PgInterestDirectory::new(db)),
    }
}

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
        .split_for_parts();
    let (catalog_router, catalog) = OpenApiRouter::with_openapi(CatalogDoc::openapi())
        .nest(&format!("{API_PREFIX}/catalog"), catalog::router())
        .split_for_parts();
    let (social_router, social) = OpenApiRouter::with_openapi(SocialDoc::openapi())
        .nest(&format!("{API_PREFIX}/social"), social::router())
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
        ApiDocument {
            name: "Социальное",
            url: "/api-docs/social.json",
            file: "social.json",
            spec: social,
        },
    ];
    (router.merge(catalog_router).merge(social_router), docs)
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
