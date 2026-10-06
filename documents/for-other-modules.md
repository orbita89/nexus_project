# Что нужно знать другому модулю (catalog, social, realtime, ...)

Короткая выжимка: как новый модуль пользуется авторизацией и как его встроить в проект.
Подробно про авторизацию — [modules/auth.md](modules/auth.md), про архитектуру — [architecture.md](architecture.md).

## Главное

- Это **модульный монолит**, а не микросервисы: один процесс, одна БД. Модуль — crate в `modules/<name>/`.
- **Модули не зависят друг от друга**, только от `libs/shared` (CI проверяет). Даже от `auth` зависеть нельзя:
  всё нужное для авторизации лежит в `shared`.
- Пользователь — это `users.id` (`uuid`). Ссылайтесь на него внешним ключом, данные пользователя
  (email, имя) берите не JOIN'ом в чужую таблицу, а через публичный интерфейс, когда он понадобится.

## Авторизация в хендлере

Токен: `Authorization: Bearer <access_token>` (JWT, 15 минут). Проверку делает extractor:

```rust
use shared::{AdminUser, AppError, AppResult, AppState, AuthUser, Role};

async fn list(State(state): State<AppState>) -> AppResult<...>            // гость: токен не нужен
async fn create_review(user: AuthUser, ...) -> AppResult<...>               // любой вошедший, иначе 401
async fn create_thread(user: AuthUser, ...) -> AppResult<...> {
    user.require(Role::Author)?;                                            // author или admin, иначе 403
}
async fn admin_only(AdminUser(admin): AdminUser, ...) -> AppResult<...>     // только admin
```

`AuthUser { id, role, session_id }`. Роли: `user < author < admin`.

| Роль | Права |
|---|---|
| гость | читать каталог и форум |
| `user` | отзывы, оценки, коллекции, ответы в темах |
| `author` | + создавать форумы и темы |
| `admin` | + админские эндпоинты (`/api/v1/<модуль>/admin/...`) |

Ошибки — `AppError` (`NotFound`, `BadRequest(msg)`, `Unauthorized`, `Forbidden`, `Conflict(msg)`, ...),
тело ответа всегда `{"error": "..."}`. Ошибки БД наружу не уходят, только в лог.

## Как встроить модуль

1. `modules/<name>/src/lib.rs` отдаёт `pub fn router() -> OpenApiRouter<AppState>` (`utoipa_axum`).
   Каждый хендлер описан `#[utoipa::path(...)]` и добавлен через `.routes(routes!(handler))` — тогда он
   сам появится в Swagger. Защищённым эндпоинтам — `security(("bearer" = []))`. Образец: `modules/auth/src/admin.rs`.
2. Модуль уже смонтирован в `app/src/lib.rs` под `/api/v1/<name>`. Путь в `#[utoipa::path]` — без префикса.
3. Тег модуля добавить в `tags(...)` в `app/src/lib.rs`.
4. DTO: `#[derive(Serialize/Deserialize, utoipa::ToSchema)]`. Ошибки в описании: `body = shared::error::ErrorBody`.

## База данных

- Новая миграция — новый файл `migrations/<YYYYMMDDHHMMSS>_<name>.sql`. Применяется при старте.
  **Применённые файлы не редактировать**, только новая миграция.
- Модуль работает **только со своими таблицами**. Чьи таблицы чьи — в `documents/modules/*.md`.
- Соглашения: `uuid` PK (`gen_random_uuid()`), `timestamptz`, триггер `set_updated_at()`, `slug` для URL,
  поиск по подстроке — `pg_trgm`. citext-колонки в `SELECT` приводить к `::text`, а в `WHERE` сравнивать с `$1::citext`.

## Тесты (обязательны на каждый эндпоинт)

- `#[sqlx::test(migrator = "nexus::MIGRATOR")]` — отдельная чистая БД с миграциями на каждый тест.
- HTTP без сети: `let (state, _outbox) = test_utils::state(pool)`, `nexus::build_app(state)`,
  `test_utils::request(app, Method::POST, uri, Some(json), Some(token))`.
- Пользователь с ролью в тесте: включить dev login (`config.dev_login = true`, `test_utils::state_with_config`)
  и `POST /api/v1/auth/dev/login {"login": "x@example.com", "role": "author"}`. Пример — `app/tests/oauth.rs`.
- Живые проверки: `http/<модуль>.http` (HTTP Client JetBrains), подключить в `make http`.
- После изменения API: `UPDATE_OPENAPI=1 cargo test -p nexus openapi` (обновит `documents/api/openapi.json`).
- Перед коммитом: `make ci` (fmt, clippy, границы модулей, тесты, cargo deny).

## Окружение

| | |
|---|---|
| Запуск | `make up`, `make seed` (admin / author / user, пароль `password123`) |
| Swagger | http://localhost/docs |
| Письма | http://localhost:8025 (Mailpit) |
| БД | `postgres://nexus_user:nexus_password@localhost:5432/nexus_db` |
| Тесты | `make test` (нужен Postgres из `make up`) |

## Definition of Done

Тесты на каждый эндпоинт зелёные · эндпоинты в Swagger · `documents/api/openapi.json` обновлён ·
`http/<модуль>.http` · миграция только новая · `documents/modules/<модуль>.md` обновлён · `make ci` проходит.

---

## Промт для новой сессии

Скопируйте в новую сессию Claude Code, открытую в `/home/dev/nexus_project`. Пример для каталога:

```
Проект Nexus — модульный монолит на Rust (axum 0.8, sqlx 0.8, PostgreSQL 17, utoipa).
Перед работой прочитай: documents/for-other-modules.md (правила для модулей и авторизация),
documents/vision.md (замысел), documents/architecture.md, documents/modules/catalog.md (таблицы
и план эндпоинтов). Модуль auth готов — его не трогать; для прав используй shared::{AuthUser,
AdminUser, Role}.

Задача: реализовать модуль catalog (modules/catalog): эндпоинты из documents/modules/catalog.md
(список сущностей с фильтрами и пагинацией, карточка по slug, люди, теги; создание/изменение —
только admin). Чтение — без авторизации.

Требования: каждый эндпоинт с #[utoipa::path] и интеграционным тестом (#[sqlx::test]);
http/catalog.http с проверками; обновить documents/modules/catalog.md и documents/api/openapi.json;
make ci должен проходить. Перед началом покажи план и спроси о неясном (например, формат пагинации
и структуру metadata по типам).
```
