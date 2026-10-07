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

## События для realtime

Что-то изменилось и это стоит показать открытым вкладкам (новое сообщение, рецензия) — опубликуйте
событие в шину **после** `commit`. Модуль `realtime` сам разошлёт его подписанным WebSocket-клиентам:

```rust
use shared::events::{Channel, Event};

tx.commit().await?;
state.events.publish(Event::new(
    "post.created",
    vec![Channel::Thread(thread_id), Channel::Entity(entity_id)],
    json!({ "thread_id": thread_id, "post_id": post_id }),   // только id, тексты клиент возьмёт по API
));
```

Каналы: `Thread` (тема), `Entity` (сущность — для подписчиков и интересов), `User` (личное).
`publish` не возвращает ошибку и не ждёт клиентов. В тестах: `let mut rx = state.events.subscribe();`
до запроса, потом `rx.try_recv()`. Образец — `modules/social/src/events.rs`, протокол — `modules/realtime.md`.

## Как встроить модуль

1. `modules/<name>/src/lib.rs` отдаёт `pub fn router() -> OpenApiRouter<AppState>` (`utoipa_axum`).
   Каждый хендлер описан `#[utoipa::path(...)]` и добавлен через `.routes(routes!(handler))` — тогда он
   сам появится в Swagger. Защищённым эндпоинтам — `security(("bearer" = []))`. Образец: `modules/auth/src/admin.rs`.
   `operationId` в документе должен быть уникален (по нему фронтенд генерирует клиент): если имя
   функции не уникально (`create`, `get`, `list`), задайте `operation_id = "create_entity"`.
   Enum из query-параметров (`IntoParams`) utoipa в схемы сам не кладёт — добавьте его в
   `components(schemas(...))` документа. Оба правила проверяет тест `openapi_specs_are_valid_for_client_generators`.
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
| WebSocket | `ws://localhost/ws`, ручная проверка — `http/realtime.http` в IDE |

## Definition of Done

Тесты на каждый эндпоинт зелёные · эндпоинты в Swagger · `documents/api/*.json` обновлены ·
`http/<модуль>.http` · миграция только новая · `documents/modules/<модуль>.md` обновлён · `make ci` проходит.

---

## Промт для новой сессии

Скопируйте в новую сессию Claude Code, открытую в `/home/dev/nexus_project`.

### catalog: трёхуровневый кэш карточек (готов, для истории)

```
Проект Nexus — модульный монолит на Rust (axum 0.8, sqlx 0.8, PostgreSQL 17, Meilisearch 1.15,
Redis 7). Перед работой прочитай: documents/architecture.md («Кэш чтения» — решение),
documents/modules/catalog.md («Кэш карточек» — уровни, ключи, ошибки, таблица записи; «Поиск» —
как сейчас устроен индекс), documents/for-other-modules.md (правила, тесты, DoD).
Работаем в новой ветке от main.

Задача: трёхуровневый кэш для карточек GET /catalog/entities/{slug} и GET /catalog/people/{slug}.

РЕШЕНИЯ ПРИНЯТЫ, НЕ ПЕРЕСМАТРИВАТЬ И НЕ ПРЕДЛАГАТЬ АЛЬТЕРНАТИВЫ:
- цепочка строго L1 moka → L2 Redis → L3 Meilisearch. В PostgreSQL чтение карточки НЕ ходит:
  * есть в L1 → ответ;
  * нет в L1, есть в L2 → записать в L1, ответ;
  * нет в L2, есть в L3 → записать в L2 и L1, ответ;
  * нет в L3 → 404 (не кэшировать); Meilisearch недоступен/ошибка/поиск выключен → 503
    (AppError::Unavailable), при этом попадания в L1/L2 отдаются как обычно;
- L1: moka::future::Cache, TTL 30 с, до 10 000 записей. L2: Redis, JSON, TTL 24 ч;
- запись в Meili после правки в админке — как сейчас через search::sync, плюс редкий полный
  реиндекс по расписанию как страховка;
- кэшируем только карточки сущностей и людей; списки, теги и EntityDirectory читают из БД;
- схема ответа карточек и GET /search не меняются (в OpenAPI только новый ответ 503).

L3 — Meilisearch:
- в документ индекса entities (search.rs: SearchDoc, load_docs) добавить поле card — полный
  EntityDetail, собранный теми же запросами, что сейчас entities::get + detail (SQL не дублировать,
  вынести общую функцию); card не в searchableAttributes; slug добавить в filterableAttributes;
- GET /search запрашивает только поля выдачи (attributesToRetrieve), card в ответ не попадает;
- чтение карточки: POST /indexes/{index}/documents/fetch, filter slug = "…", limit 1,
  fields ["card"] (экранировать кавычки в slug);
- люди — отдельный индекс people: документ { id, slug, full_name, card }, где card — PersonDetail
  (как GET /people/{slug}: человек и фильмография); slug filterable, full_name searchable;
  ключ кэша nexus:v1:catalog:person:{slug};
- связи: карточка человека показывает его работы, карточка сущности — участников, поэтому
  правка сущности обновляет и карточки её участников, правка человека — карточки его работ.

shared (libs/shared/src/cache.rs, по образцу shared::search: Option<Arc<Inner>>, disabled(),
is_enabled()):
- трейты для подмены в тестах (через async-trait, см. shared::directory;
  #[cfg_attr(test, mockall::automock)]): L2Store { get, set_ex, del, del_pattern } и источник L3
  (в catalog — трейт EntityProvider { get_entity(slug) -> Result<Option<EntityDetail>, _> },
  реализация MeiliEntityProvider);
- RedisStore на deadpool-redis, каждая операция с таймаутом ~100 мс; ошибка, таймаут или битый
  JSON → tracing::warn! и переход к L3 (битый ключ — DEL), без паник и без 5xx из-за Redis;
- запись в L1 сразу, в L2 — tokio::spawn через TaskTracker (ответ не ждёт Redis; в тестах
  wait_pending() вместо sleep); одновременные промахи по ключу схлопываются (try_get_with);
- invalidate(keys): L1 + DEL в Redis, повторный DEL через ~2 с в фоне;
- ключи nexus:v1:catalog:entity:{slug}; префикс ключей настраивается (для тестов);
- Config через var_or: CACHE_L1_TTL_SECS=30, CACHE_L1_CAPACITY=10000, CACHE_L2_TTL_SECS=86400,
  SEARCH_REINDEX_INTERVAL_SECS=86400 (0 — выключено); redis_url уже есть; поле cache в AppState.
Зависимости: moka (future), deadpool-redis, tokio-util (TaskTracker), dev — mockall.

Запись (порядок обязателен): commit в БД → search::sync обновляет Meili и ЖДЁТ применения задачи
(call_and_wait, сейчас sync задачу не ждёт) → invalidate L1/L2 → ответ. Иначе промах между
сбросом и применением снова положит в L2 старую карточку из Meili на сутки. Ошибка sync — warning,
запись остаётся (как сейчас). Точки — таблица в catalog.md; особо:
- смена slug → сбросить и старый ключ; при удалении slug'и взять до удаления;
- человек: sync и сброс при ЛЮБОМ update (сейчас sync только при смене имени, а slug и фото
  человека есть в card);
- nexus media (отдельный процесс) — так же через sync + invalidate.

Реиндекс: при старте (уже есть), по расписанию (tokio interval, SEARCH_REINDEX_INTERVAL_SECS) и
POST /admin/search/reindex. По расписанию — только один инстанс: блокировка в Redis SET NX EX.
После swap — del_pattern nexus:v1:catalog:entity:* в Redis и очистка L1.

Тесты (главное — доказать, что цепочка работает):
- unit на моках mockall (MockL2Store, MockEntityProvider), проверять число вызовов (times):
  1) холодный старт: L1 пусто, L2 get → None, L3 вызван ровно 1 раз; после wait_pending ключ
     в L1 и вызван set_ex в L2 с этим JSON;
  2) L1 hit: второй запрос — ни L2, ни L3 не вызываются;
  3) L1 истёк, L2 hit: TTL L1 50 мс из конфига + sleep (tokio::time::advance на часы moka НЕ
     влияет); L2 get вызван, L3 НЕ вызван, значение снова в L1;
  4) L2 miss → L3 hit → записано и в L2, и в L1; следующий запрос из L1;
  5) L3 None → 404, ничего не записано ни в L1, ни в L2;
  6) L3 ошибка → 503; L1/L2 hit при упавшем L3 → 200;
  7) L2 ошибка/таймаут → идём в L3, 200, без паники; битый JSON в L2 → L3 + DEL;
  8) N параллельных промахов → L3 ровно 1 раз;
  9) invalidate → следующий запрос снова в L2/L3.
- интеграционные в app/tests/catalog.rs с настоящими Postgres, Redis и Meili (test-utils:
  with_cache с префиксом test_{uuid}_ как with_search; все тесты, читающие карточку, теперь
  через Ctx::with_search, иначе 503):
  создание в админке → карточка сразу 200; admin update → свежая карточка (не из L1/L2);
  смена slug → старый 404; правка фото человека видна в карточке; удаление → 404;
  данные в Redis под ключом после первого запроса; Redis по недоступному адресу → 200 из Meili;
  поиск выключен → 503; ответ GET /search не содержит card; реиндекс сбрасывает ключи Redis.
- CI: сервис redis:7-alpine и REDIS_URL в .github/workflows/ci.yml, значение по умолчанию в Makefile.

Готово, когда: DoD этого файла, make ci проходит, documents/api/*.json без изменений, в
architecture.md и catalog.md 🕓 заменены на ✅, описано, что получилось, и поправлена строка
«Остальной каталог от Meilisearch не зависит». Перед кодом покажи план и спроси о неясном
(в рамках принятых решений).
```

### social: форум (готов, для истории)

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
