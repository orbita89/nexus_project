# Что нужно знать другому модулю (catalog, social, realtime, ...)

Короткая выжимка: как новый модуль пользуется авторизацией и как его встроить в проект.
Подробно про авторизацию — [modules/auth.md](modules/auth.md), про архитектуру — [architecture.md](architecture.md).

## Главное

- Это **модульный монолит**, а не микросервисы: один процесс, одна БД. Модуль — crate в `modules/<name>/`.
- **Модули не зависят друг от друга**, только от `libs/shared` (CI проверяет). Даже от `auth` зависеть нельзя:
  всё нужное для авторизации лежит в `shared`.
- Пользователь — это `users.id` (`uuid`). Ссылайтесь на него внешним ключом, данные пользователя
  (username, имя) берите не JOIN'ом в чужую таблицу, а через справочник (см. ниже).

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

`AuthUser { id, role, session_id }`. Роли: `user < author < admin`. Для «гостю — публичное, владельцу —
ещё и своё» есть `Option<AuthUser>`: без заголовка — `None`, с недействительным токеном — 401.

| Роль | Права |
|---|---|
| гость | читать каталог и форум |
| `user` | отзывы, оценки, коллекции, ответы в темах |
| `author` | + создавать форумы и темы |
| `admin` | + админские эндпоинты (`/api/v1/<модуль>/admin/...`) |

Ошибки — `AppError` (`NotFound`, `BadRequest(msg)`, `Unauthorized`, `Forbidden`, `Conflict(msg)`,
`Unavailable(msg)` → 503, ...), тело ответа всегда `{"error": "..."}`. Ошибки БД наружу не уходят, только в лог.
Параметры запроса — через `shared::extract`, а не axum: JSON-тело — `JsonBody<T>` вместо `axum::Json<T>`,
путь — `Path<T>`, query — `Query<T>`. Тогда и неразобранный запрос (`/x/not-a-uuid`, `?year=abc`, тело
без поля) даёт `400 {"error": "..."}`, а не текст или 422. `axum::extract::{Path, Query}` в модулях
запрещены через `clippy.toml` (`make lint` упадёт); `axum::Json` запретить нельзя — он же тип ответа.

## Чужие данные: справочники

Нужны название сущности или имя пользователя — не JOIN в чужую таблицу, а трейты из
`shared::directory`, которые реализуют модули-владельцы и которые лежат в `AppState`:

```rust
let entity = state.entities.by_slug("dune-2021").await?.ok_or(AppError::NotFound)?; // EntityRef
let authors = state.users.by_ids(&ids).await?;                                       // HashMap<Uuid, UserRef>
```

`by_ids` пакетный: соберите id со страницы и сделайте один вызов. Нужен новый метод — добавьте его в
трейт и в реализацию у владельца (`catalog/src/directory.rs`, `auth/src/directory.rs`). В тестах
справочник подменяется: `state.entities = Arc::new(Fake)`. Образец — модуль `social` (`refs.rs`).

Пагинация: `shared::pagination::{Page, page_bounds}` — `{items, total, limit, offset}`.

## Как встроить модуль

1. `modules/<name>/src/lib.rs` отдаёт `pub fn router() -> OpenApiRouter<AppState>` (`utoipa_axum`).
   Каждый хендлер описан `#[utoipa::path(...)]` и добавлен через `.routes(routes!(handler))` — тогда он
   сам появится в Swagger. Защищённым эндпоинтам — `security(("bearer" = []))`. Образец: `modules/auth/src/admin.rs`.
2. Модуль уже смонтирован в `app/src/lib.rs` под `/api/v1/<name>`. Путь в `#[utoipa::path]` — без префикса.
3. Тег модуля добавить в `tags(...)` в `app/src/lib.rs`. Большому модулю можно дать отдельную
   вкладку в Swagger (свой OpenAPI-документ): образец — `CatalogDoc` и список документов в `api()`.
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
- Внешние сервисы: в тестах поиск выключен (`state.search` — `Search::disabled()`);
  `test_utils::with_search(state)` включает Meilisearch со своим префиксом индексов. Пример — `app/tests/catalog.rs`.
- Живые проверки: `http/<модуль>.http` (HTTP Client JetBrains), подключить в `make http`.
- После изменения API: `UPDATE_OPENAPI=1 cargo test -p nexus openapi` (обновит `documents/api/*.json`: файл на вкладку Swagger).
- Перед коммитом: `make ci` (fmt, clippy, границы модулей, тесты, cargo deny).

## Окружение

| | |
|---|---|
| Запуск | `make up`, `make seed` (admin / author / user, пароль `password123`, мини-каталог и social) |
| Swagger | http://localhost/docs |
| Письма | http://localhost:8025 (Mailpit) |
| БД | `postgres://nexus_user:nexus_password@localhost:5432/nexus_db` |
| Тесты | `make test` (нужен Postgres из `make up`) или `make test-docker` / `make ci-docker` без Rust на машине |

## Definition of Done

Тесты на каждый эндпоинт зелёные · эндпоинты в Swagger · `documents/api/*.json` обновлены ·
`http/<модуль>.http` · миграция только новая · `documents/modules/<модуль>.md` обновлён · `make ci` проходит.

---

## Промт для новой сессии

Скопируйте в новую сессию Claude Code, открытую в `/home/dev/nexus_project`.

### social: форум (следующий)

```
Проект Nexus — модульный монолит на Rust (axum 0.8, sqlx 0.8, PostgreSQL 17, utoipa, Meilisearch).
Работаем в ветке feature/social-forum от feature/infra (в ней catalog, первая часть social и
инфраструктура). Перед работой прочитай: documents/for-other-modules.md (правила для модулей,
авторизация, справочники, экстракторы), documents/vision.md (замысел: гибридные обсуждения),
documents/architecture.md («Правила границ»), documents/modules/social.md (готовая первая часть
и набросок форума), documents/modules/realtime.md (как realtime позже будет рассылать сообщения).
Модули auth, catalog и первая часть social готовы — их поведение не менять. Образец кода, тестов,
http-проверок и документации — modules/social, app/tests/social.rs, http/social.http.

Задача: форум в модуле social. Интересы и ленту НЕ делаем — это следующая задача.
- темы, привязанные сразу к нескольким сущностям (тема про «Дюну» — книга и фильм): создать,
  изменить, удалить; темы сущности; свежие и активные темы; темы пользователя;
- сообщения в теме с ответами на сообщения (ветки): тема с сообщениями, ответить, изменить,
  удалить;
- права: читать — без авторизации; создавать темы — роль author и выше
  (user.require(Role::Author)), отвечать — user и выше; автор правит и удаляет своё;
- модерация: admin удаляет любые тему и сообщение (и, если решим, закрывает тему для ответов).
Счётчики форума (сколько тем и сообщений) — по желанию добавить в профиль GET /users/{username}.

Как в первой части: названия сущностей и авторов — только через shared::directory
(state.entities / state.users, пакетно by_ids), в чужие таблицы SQL не ходит. Параметры —
shared::extract::{JsonBody, Path, Query} (axum Path/Query запрещены clippy.toml), пагинация —
shared::pagination::Page, ошибки {"error": ...}, новые теги в существующей вкладке Swagger
«Социальное» (SocialDoc в app/src/lib.rs).

Требования: каждый эндпоинт с #[utoipa::path] и интеграционным тестом (#[sqlx::test]);
http/forum.http с проверками (подключить в make http; всё, что файл создаёт, он же удаляет, и
остатки прерванного прогона не мешают следующему); дополнить seeds/social.sql темами и
сообщениями тестовых пользователей на сущности из seeds/catalog.sql и тест seed_social_is_valid;
таблицы и индексы — только новой миграцией; обновить documents/modules/social.md и
documents/api/social.json; make ci (или make ci-docker) должен проходить.

Перед началом покажи план (таблицы, индексы, эндпоинты) и спроси о неясном, например:
сколько сущностей можно привязать к теме (от 1 до N?) и можно ли менять привязку; глубина веток
и как отдавать сообщения (дерево или плоский список с parent_id, пагинация длинных тем); что
видно при удалении сообщения с ответами («сообщение удалено» или каскад); сортировка тем (по
last_post_at или созданию); правка (пометка «изменено», ограничение по времени); закрытие и
закрепление тем; лимиты длины заголовка и текста; нужен ли поиск по темам в Meilisearch сейчас;
как social позже сообщит realtime о новых сообщениях (трейт в shared — только предложить, не
реализовывать).

Заметки по окружению: проверки без Rust на машине — make ci-docker / make test-docker (сервис
tools, documents/deploy.md; первая сборка в нём долгая, дальше из кеша). make http не гонять чаще
раза в несколько минут: вход ограничен (10 попыток на логин, потом 1 в 90 секунд), иначе 429 и
каскад падений.
```

### social, первая часть (готов, для истории)

```
Проект Nexus — модульный монолит на Rust (axum 0.8, sqlx 0.8, PostgreSQL 17, utoipa, Meilisearch).
Перед работой прочитай: documents/for-other-modules.md (правила для модулей и авторизация),
documents/vision.md (замысел), documents/architecture.md (особенно «Правила границ»),
documents/modules/social.md (таблицы и план эндпоинтов), documents/modules/catalog.md (как
устроен готовый модуль). Модули auth и catalog готовы — их поведение не менять. Образец кода,
тестов, http-проверок и документации — modules/catalog, app/tests/catalog.rs, http/catalog.http.

Задача: реализовать модуль social (modules/social), первая часть — без форума, интересов и ленты:
- рецензии и оценки: своя рецензия на сущность (создать/изменить/удалить), список рецензий
  сущности, рецензии пользователя, сводка оценок сущности (средняя, количество);
- подписки на пользователей: подписаться/отписаться, подписчики и подписки;
- коллекции: CRUD своих коллекций и их содержимого (порядок, заметки), публичные коллекции
  других, в каких коллекциях есть сущность; приватные видит только владелец;
- модерация: admin может удалить любую рецензию и коллекцию.
Чтение публичного — без авторизации, запись — AuthUser (роль user и выше).

Главный архитектурный вопрос: social нельзя читать таблицы entities и users (они catalog и auth),
а в ответах нужны название/slug сущности и имя автора, и при записи нужно проверять, что сущность
существует. По правилам это явный интерфейс в shared (трейт), который реализует модуль-владелец
и который передаётся через AppState. Предложи вариант (трейты, где реализация, как подключается,
как подменяется в тестах) до того, как писать код.

Соглашения как в catalog: пагинация {items, total, limit, offset} (тип Page<T> перенеси в shared,
если нужен обоим модулям), JSON-тело через shared::extract::JsonBody, ошибки {"error": ...},
отдельная вкладка Swagger «Социальное» (как CatalogDoc в app/src/lib.rs).

Требования: каждый эндпоинт с #[utoipa::path] и интеграционным тестом (#[sqlx::test]);
http/social.http с проверками (подключить в make http); мини-дамп seeds/social.sql (рецензии,
оценки, подписки и коллекции тестовых пользователей на сущности из seeds/catalog.sql, загрузка
в make seed) с тестом, что он загружается; обновить documents/modules/social.md и
documents/api/*.json; новые таблицы/индексы — только новой миграцией; make ci должен проходить.

Перед началом покажи план и спроси о неясном, например: адресация сущностей в URL (slug или id),
формат и округление сводки оценок, отдавать ли сводку в карточке каталога, сортировки списков
рецензий, может ли рецензия быть без текста в списке, лимиты длины текста, видимость подписок.
```

### catalog (готов, для истории)

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
