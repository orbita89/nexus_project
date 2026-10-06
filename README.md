# Nexus

Бэкенд платформы для каталогизации и обсуждения контента: фильмов, сериалов, книг и игр.
Пользователи смогут оценивать и рецензировать произведения, собирать коллекции и подписываться друг на друга.

Написан на Rust (axum + sqlx) как **модульный монолит**: один процесс и одна БД, но код разделён на модули с явными границами.

> **Статус: ранняя стадия.** Инфраструктура, dev-окружение и схема БД готовы. Работает авторизация
> (JWT, роли `user` / `author` / `admin`). Каталог, форум и рецензии ещё не реализованы.

## Документация

Подробности — в [`documents/`](documents/README.md):

- [Главная задумка](documents/vision.md) — что строим и зачем
- [Технологии и архитектура](documents/architecture.md) — модульный монолит, абстрактная сущность, инфраструктура
- Модули и их таблицы: [auth](documents/modules/auth.md), [catalog](documents/modules/catalog.md),
  [social](documents/modules/social.md), [realtime](documents/modules/realtime.md)

## Что готово

- **Cargo workspace**: приложение, четыре модуля и общая библиотека; версии зависимостей заданы в одном месте.
- **Общая библиотека `libs/shared`**:
  - `Config` — конфигурация из переменных окружения; дефолты позволяют запускать приложение через `cargo run` без Docker;
  - `db::connect` — пул соединений к PostgreSQL с ленивым подключением, так что приложение стартует, даже если БД ещё не поднялась;
  - `AppState` — общее состояние (конфиг и пул БД), которое передаётся во все модули;
  - `AppError` / `AppResult` — единый тип ошибки для хендлеров, который превращается в JSON `{"error": "..."}` с нужным HTTP-статусом (подробности ошибок БД пишутся только в лог);
  - `telemetry::init` — логирование через `tracing`, уровень задаётся через `RUST_LOG`.
- **Приложение `nexus`**: `/health`, HTTP-трассировка запросов и подключение модулей:

  | Модуль     | Префикс        | Назначение (план)                                         | Готово сейчас          |
  |------------|----------------|-----------------------------------------------------------|------------------------|
  | `auth`     | `/api/v1/auth`    | регистрация и вход (пароль, ссылка из письма, OAuth), токены, роли | ✅ готово |
  | `catalog`  | `/api/v1/catalog` | контент (`entities`), люди, теги, поиск через Meilisearch          | пустой роутер |
  | `social`   | `/api/v1/social`  | подписки, рецензии, оценки, коллекции                              | пустой роутер |
  | `realtime` | `/ws`          | WebSocket: уведомления, присутствие, чат                  | echo-WebSocket         |

- **Схема БД**: первая миграция `migrations/20260916022500_init_schema.sql` (подробнее ниже).
- **Dev-окружение в Docker Compose**: PostgreSQL 17, Redis 7, Meilisearch 1.15, приложение с hot-reload через `cargo watch` и nginx в качестве reverse proxy.

## Структура

```
.
├── Cargo.toml              # workspace и общие зависимости
├── app/                    # бинарник nexus: сборка модулей в одно приложение
├── modules/
│   ├── auth/
│   ├── catalog/
│   ├── social/
│   └── realtime/
├── libs/
│   ├── shared/             # общий код модулей
│   └── test-utils/         # хелперы для тестов (только dev-dependency)
├── documents/              # документация проекта
├── migrations/             # SQL-миграции (формат sqlx), применяются при старте
├── scripts/                # проверки для CI
├── .github/                # GitHub Actions и Dependabot
├── Makefile                # команды разработки
├── deny.toml               # правила cargo-deny
└── infra/
    ├── docker-compose.yml  # dev-окружение
    ├── Dockerfile.dev      # dev-образ: Rust toolchain + cargo-watch
    ├── Dockerfile          # production-образ
    ├── nginx.conf          # reverse proxy
    └── proxy_common.conf   # общие proxy-заголовки
```

## Запуск

Нужны Docker и Docker Compose.

```bash
make up      # то же, что docker compose -f infra/docker-compose.yml up -d --build
make logs
make seed    # тестовые пользователи: admin / author / user, пароль password123
```

Тестовые пользователи и как устроены токены и роли — в [documents/modules/auth.md](documents/modules/auth.md).

При первом запуске приложение компилируется внутри контейнера, это займёт несколько минут.
Потом изменения в исходниках подхватываются автоматически, `cargo watch` пересобирает и перезапускает приложение.

Без Docker (нужны запущенные PostgreSQL, Redis и Meilisearch, адреса по умолчанию — `localhost`):

```bash
cargo run -p nexus   # слушает 0.0.0.0:8080, меняется через BIND_ADDR
```

Доступ снаружи:

| Адрес                         | Что это                        |
|-------------------------------|--------------------------------|
| `http://localhost/`           | nginx (единая точка входа)     |
| `localhost:5432`              | PostgreSQL                     |
| `localhost:6379`              | Redis                          |
| `http://localhost:7700`       | Meilisearch                    |
| **`http://localhost/docs`**   | **Swagger UI** — документация API, запросы прямо из браузера |
| **`http://localhost:8025`**   | **Mailpit** — все письма, которые отправляет приложение |

Приложение слушает `:8080` внутри Docker-сети и наружу не публикуется, к нему обращаются через nginx.
nginx проксирует все запросы в приложение как есть, а маршрутизацию по модулям делает само приложение.
Для `/ws` nginx дополнительно включает апгрейд до WebSocket и таймаут 1 час.

Проверка:

```bash
curl localhost/health   # {"service":"nexus","status":"ok"}
```

### Для тестировщиков

1. Откройте **http://localhost/docs**.
2. Войдите: `POST /api/v1/auth/dev/login` с `{"login": "admin"}` (или `author`, `user`). Пароль не нужен.
3. Скопируйте `access_token` → кнопка **Authorize** → вставьте. Токен живёт 15 минут.
4. Письма (подтверждение email, вход по ссылке, сброс пароля) — в **http://localhost:8025**,
   токен — параметр `token=` в ссылке.

### Конфигурация

Значения по умолчанию заданы прямо в `docker-compose.yml`, поэтому окружение поднимается без дополнительных файлов.
Чтобы их переопределить, скопируйте `infra/.env.example` в `infra/.env` (он в `.gitignore`) и поменяйте нужное.

Приложение читает переменные `BIND_ADDR`, `DATABASE_URL`, `REDIS_URL`, `MEILI_URL` и `MEILI_MASTER_KEY`.

### Миграции

Миграции из `migrations/` встраиваются в бинарник и применяются **при старте приложения**.
Новая миграция — новый файл `migrations/<YYYYMMDDHHMMSS>_<name>.sql`. Уже применённые файлы не редактируются:
sqlx сверяет контрольные суммы и не даст стартовать.

Если схему в вашей dev-базе раньше накатывали вручную, sqlx о ней не знает и упадёт на `already exists`.
Данных там пока нет, проще пересоздать том: `docker compose -f infra/docker-compose.yml down -v`.

## Разработка

| Команда           | Что делает                                                    |
|-------------------|---------------------------------------------------------------|
| `make fmt`        | форматирование                                                |
| `make lint`       | `fmt --check` + `clippy -D warnings`                          |
| `make test`       | все тесты (нужен Postgres из `make up`, или задайте `DATABASE_URL`) |
| `make boundaries` | проверка, что модули не зависят друг от друга                 |
| `make deny`       | уязвимости и лицензии зависимостей (`cargo install cargo-deny`) |
| `make ci`         | всё перечисленное — то же, что проверяет CI                   |
| `make image`      | собрать production-образ                                      |
| `make seed`       | загрузить тестовых пользователей в dev-базу (`seeds/dev.sql`) |
| `make http`       | HTTP-проверки против поднятого окружения (после `make seed`)  |

### Тесты

- **Unit** — `#[cfg(test)]` рядом с кодом, для чистой логики.
- **С БД** — `#[sqlx::test(migrator = "nexus::MIGRATOR")]`: каждый тест получает отдельную чистую базу
  с применёнными миграциями, моки не нужны.
- **HTTP** — приложение собирается через `nexus::build_app(state)` и вызывается без сети;
  хелперы `test_utils::{state, get, send}`. Пример — `app/tests/app.rs`.

### HTTP-проверки

`http/*.http` — запросы для HTTP Client в IDE JetBrains с автопроверками ответов (`client.test`).
Работают против поднятого окружения (`make up`), окружение в IDE — `dev`.

- `health.http` — smoke: приложение отвечает, модули смонтированы. Из консоли: `make http`
- `auth.http` — вход под тестовыми пользователями, токены, роли, админка (нужен `make seed`)
  (запускает `ijhttp` в Docker, ставить ничего не нужно).
- `realtime.http` — ручная проверка WebSocket echo.

Новые эндпоинты добавляйте в `http/<модуль>.http` вместе с проверками. Секреты (токены и т. п.) —
в `http/http-client.private.env.json`, он в `.gitignore`.

### CI

GitHub Actions (`.github/workflows/ci.yml`) на каждый PR и push в `main`:
fmt и clippy, проверка границ модулей, тесты с Postgres, `cargo deny`, сборка production-образа.
Dependabot раз в неделю предлагает обновления зависимостей, actions и базовых образов.

## Схема БД

Таблицы, их структура и назначение описаны по модулям в [`documents/modules/`](documents/modules/).

## Дальнейшие шаги

- [ ] `catalog`: CRUD сущностей, людей и тегов, индексация и поиск в Meilisearch
- [ ] `social`: подписки, рецензии, коллекции
- [ ] `realtime`: авторизация сокетов и рассылка событий вместо echo (внутри одного процесса
      хватит `tokio::sync::broadcast`, Redis pub/sub понадобится при нескольких инстансах)
- [ ] Логи: JSON-формат, request id, `/health/live` и `/health/ready`, graceful shutdown
- [ ] Документация: CONTRIBUTING, architecture, ADR
- [ ] Публикация образа и деплой
