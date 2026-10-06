# Модуль `auth`

**Отвечает за:** регистрацию и вход, токены и сессии, пароли, роли, блокировку пользователей.
**URL:** `/api/v1/auth` · **Код:** `modules/auth/` (выдача токенов), `libs/shared/src/auth.rs`
(проверка токенов и ролей — ей пользуются все модули), `libs/shared/src/mail.rs` (письма).

**Статус:** ✅ готов. Интерактивная документация — **http://localhost/docs** (Swagger UI).

## Способы входа

| Способ | Как | Email подтверждён? |
|---|---|---|
| **Email + пароль** | `register` → письмо → `email/verify` → вход | Обязательно: до подтверждения `login` даёт 403 |
| **Ссылка из письма** (без пароля) | `email/login` → письмо → `email/login/confirm` | Да — переход по ссылке и есть подтверждение |
| **OAuth-провайдер** (Google, GitHub, Яндекс) | `oauth/{provider}/start` → провайдер → `oauth/exchange` | Если провайдер подтвердил адрес |
| **Dev login** (только разработка) | `dev/login` с любым email/username | Не проверяется |

Все способы ведут к одному аккаунту, если email совпадает: зарегистрировался по паролю, потом
вошёл по ссылке или через Google с тем же адресом — это тот же пользователь.

### Email + пароль

```
POST /register {email, username, password}  →  201, аккаунт не подтверждён, письмо со ссылкой
                                                {APP_BASE_URL}/auth/verify-email?token=...
POST /email/verify {token}                  →  200, email подтверждён + пара токенов (сразу вход)
POST /login {login, password}               →  200 пара токенов   (до подтверждения — 403)
```
Письмо потерялось: `POST /email/verify/resend {email}`. Новое письмо гасит ссылку из старого.

### Вход по ссылке из письма (без пароля)

```
POST /email/login {email}            →  202, письмо со ссылкой {APP_BASE_URL}/auth/email-login?token=...
POST /email/login/confirm {token}    →  200 пара токенов. Адреса не было — аккаунт создан
```
У созданного так пользователя нет пароля. Задать его можно через «забыл пароль».
username генерируется из email (`neo.anderson_3f9a1c`).

### OAuth-провайдеры

```
браузер → GET /oauth/{provider}/start             → 303 на страницу входа провайдера
провайдер → GET /oauth/{provider}/callback?code=…  → 303 на {APP_BASE_URL}/auth/oauth/callback?code=…
фронтенд  → POST /oauth/exchange {code}            → 200 пара токенов
```
- `start` открывается в браузере (ссылкой или `window.location`), а не через `fetch`.
- Токены не попадают в адресную строку: фронтенд получает одноразовый код на 2 минуты и меняет его на токены.
- **Защита:** `state` против подделки запроса, PKCE против перехвата кода.
- **Привязка аккаунтов.** Аккаунт провайдера привязывается к существующему пользователю с тем же
  email, только если провайдер **подтвердил** адрес. Иначе — ошибка `email_in_use`: так нельзя
  захватить чужой аккаунт, указав у провайдера чужой адрес.
- **Ошибки** приходят на фронтенд как `?error=`: `access_denied` (пользователь отменил вход),
  `invalid_state` (ссылка устарела), `provider_error`, `email_required` (провайдер не дал email),
  `email_in_use`, `account_blocked`, `internal_error`.
- `GET /oauth/providers` — список включённых провайдеров, чтобы показывать нужные кнопки.

#### Как подключить провайдера

Провайдер включается, когда заданы оба ключа. Ключи кладутся в `infra/.env` (он в `.gitignore`),
в репозиторий и в чат их не отправлять. После изменения: `make up`.

**Redirect URI**, который указывается у провайдера: `{PUBLIC_URL}/api/v1/auth/oauth/<provider>/callback`.
В dev это `http://localhost/api/v1/auth/oauth/google/callback`.

| Провайдер | Где создать приложение | Переменные | Права (scopes) |
|---|---|---|---|
| Google | console.cloud.google.com → APIs & Services → Credentials → OAuth client ID (Web application) | `OAUTH_GOOGLE_CLIENT_ID`, `OAUTH_GOOGLE_CLIENT_SECRET` | `openid email profile` |
| GitHub | github.com → Settings → Developer settings → OAuth Apps → New | `OAUTH_GITHUB_CLIENT_ID`, `OAUTH_GITHUB_CLIENT_SECRET` | `read:user user:email` |
| Яндекс ID | oauth.yandex.ru → Создать приложение (веб-сервисы), доступы: email, имя, аватар | `OAUTH_YANDEX_CLIENT_ID`, `OAUTH_YANDEX_CLIENT_SECRET` | `login:email login:info login:avatar` |

Проверка: `GET /api/v1/auth/oauth/providers` должен вернуть провайдера, а открытие
`http://localhost/api/v1/auth/oauth/<provider>/start` в браузере — привести на страницу входа.

**Новый провайдер.** Если он поддерживает OpenID Connect, достаточно добавить пресет
в `OAuthProviderConfig::presets()` (`libs/shared/src/config.rs`). Если нет (VK ID и т. п.) —
нужен ещё разбор профиля в `modules/auth/src/oauth/provider.rs`.

### Dev login (только для разработки)

`POST /dev/login {"login": "author"}` — вход под любым пользователем без пароля и письма.
- Неизвестный email — пользователь создаётся.
- `"role": "admin"` — роль выставляется перед входом.
- Работает только при `DEV_LOGIN=true` (в dev-окружении включено). Иначе отвечает 404, как будто
  эндпоинта нет. **В проде не включать.** Каждый такой вход пишется в лог с уровнем WARN.

## Роли

| Роль | Кто это | Что может |
|---|---|---|
| гость | без токена | Читать каталог и форум |
| `user` | любой зарегистрированный | + отзывы и оценки, коллекции, **ответы в темах форума** |
| `author` | назначает админ | + **создавать форумы и темы** |
| `admin` | назначает админ | + админские эндпоинты (`/api/v1/<модуль>/admin/...`) |

Роли упорядочены: `user < author < admin`. Новый пользователь всегда получает `user`.

### Как проверять права в коде

Проверка лежит в `shared`, поэтому любой модуль проверяет права, не завися от `auth`.

```rust
use shared::{AdminUser, AuthUser, Role};

// Нужен любой вошедший пользователь (иначе 401)
async fn create_review(user: AuthUser, ...) -> AppResult<...> { ... }

// Нужна роль не ниже author (иначе 403)
async fn create_thread(user: AuthUser, ...) -> AppResult<...> {
    user.require(Role::Author)?;
    ...
}

// Только админ: 401 без токена, 403 с другой ролью
async fn admin_action(AdminUser(admin): AdminUser, ...) -> AppResult<...> { ... }
```
В описании эндпоинта для Swagger: `security(("bearer" = []))`, тогда в UI появится замок.

## Токены и сессии

| | Access-токен | Refresh-токен |
|---|---|---|
| Что это | JWT (HS256), подписан `JWT_SECRET` | Случайная строка, 64 hex-символа |
| Живёт | 15 минут | 30 дней |
| На сервере | не хранится, проверяется по подписи | хеш SHA-256 в `refresh_tokens` |
| Как передаётся | `Authorization: Bearer <token>` | в теле `/refresh` и `/logout` |
| Содержимое | `sub` (id пользователя), `role`, `sid` (id сессии), `iat`, `exp` | — |

- **Ротация.** `refresh` выдаёт новую пару, старый refresh-токен отзывается и запоминает, чем заменён.
- **Кража.** Заменённый токен пришёл снова — значит, его украли (или клиент сломан):
  отзываются **все** сессии пользователя. Отозванный выходом токен просто даёт 401.
  Следствие для фронтенда: два параллельных `refresh` с одним токеном разлогинят пользователя,
  запросы на обновление нужно выстраивать в очередь.
- **Сессии.** `GET /sessions` — активные устройства, текущее помечено `current: true`.
  `DELETE /sessions/{id}` — выйти на одном устройстве, `POST /logout-all` — на всех.
- **Смена роли и блокировка** вступают в силу при следующем refresh, то есть не позже чем через
  15 минут. При блокировке все сессии отзываются сразу.

**`JWT_SECRET`:** не короче 32 символов, иначе приложение не стартует. В проде обязательно свой
(`openssl rand -base64 48`). Смена секрета делает недействительными все access-токены.

## Пароль

- `POST /password/forgot {email}` → 202 и письмо со ссылкой `{APP_BASE_URL}/auth/reset-password?token=...` (живёт 1 час).
- `POST /password/reset {token, password}` → 204. Все устройства разлогинены, email считается подтверждённым.
- `POST /password/change {current_password, new_password}` (с токеном) → 204. Остальные устройства
  разлогинены, текущее — нет.

Пароли хешируются Argon2id в отдельном пуле потоков. Правила: 8–128 символов.

## Защита

- **Не раскрываем, есть ли аккаунт.** Неверный логин, неверный пароль и блокировка дают одинаковый 401;
  пароль проверяется даже для несуществующего логина, чтобы время ответа было одинаковым.
  `forgot`, `resend`, `email/login` всегда отвечают 202.
- **Rate limiting** (ответ 429):

  | Что ограничено | Лимит |
  |---|---|
  | Вход, регистрация, письма, OAuth с одного IP | 30 в минуту |
  | Попытки входа в один аккаунт (с любых IP) | 10 подряд, дальше 1 в 90 с |
  | Письма на один адрес | 5 подряд, дальше 1 в 12 минут |

  Счётчики в памяти процесса. При нескольких экземплярах приложения их нужно перенести в Redis.
  IP берётся из `X-Real-IP` от nginx.
- **Одноразовые ссылки.** В БД хранится только хеш. Новая ссылка гасит предыдущие того же назначения.
- **Фоновая чистка** раз в час: просроченные refresh-токены, ссылки из писем, незавершённые OAuth-входы.

## Письма

В dev все письма ловит **Mailpit**: **http://localhost:8025**. Ссылки в письмах ведут на фронтенд
(`APP_BASE_URL`), токен — параметр `token=`. Пока фронтенда нет, токен копируют из письма и
отправляют через Swagger.

| Переменная | Что это | dev |
|---|---|---|
| `SMTP_URL` | SMTP-сервер. Не задан — письма только в лог | `smtp://mailpit:1025` |
| `MAIL_FROM` | Отправитель | `Nexus <no-reply@nexus.local>` |
| `APP_BASE_URL` | Адрес фронтенда для ссылок | `http://localhost` |

В проде: `SMTP_URL=smtps://user:password@smtp.example.com` (TLS) или `smtp://...?tls=required`.

## Эндпоинты

Полный список с описанием полей и примерами — в Swagger: http://localhost/docs.
Контракт API в репозитории: `documents/api/openapi.json`. Его проверяет тест, и любое изменение
API видно в PR. Обновить: `UPDATE_OPENAPI=1 cargo test -p nexus openapi`.

| Метод | Путь (`/api/v1/auth/...`) | Доступ | Что делает |
|---|---|---|---|
| POST | `register` | все | Регистрация по паролю → 201, письмо |
| POST | `email/verify` | все | Подтвердить email по токену → токены |
| POST | `email/verify/resend` | все | Письмо с подтверждением ещё раз → 202 |
| POST | `login` | все | Вход по email/username и паролю → токены |
| POST | `email/login` | все | Ссылка для входа без пароля → 202 |
| POST | `email/login/confirm` | все | Вход по ссылке → токены |
| GET | `oauth/providers` | все | Включённые провайдеры |
| GET | `oauth/{provider}/start` | все | Редирект к провайдеру |
| GET | `oauth/{provider}/callback` | провайдер | Возврат от провайдера → редирект на фронтенд |
| POST | `oauth/exchange` | все | Код с фронтенда → токены |
| POST | `refresh` | все | Новая пара токенов |
| POST | `logout` | все | Выйти (отозвать refresh) → 204 |
| POST | `logout-all` | вошедший | Выйти на всех устройствах → 204 |
| GET | `sessions` | вошедший | Мои активные сессии |
| DELETE | `sessions/{id}` | вошедший | Завершить сессию → 204 |
| GET | `me` | вошедший | Мой профиль |
| POST | `password/forgot` | все | Письмо для сброса пароля → 202 |
| POST | `password/reset` | все | Новый пароль по токену → 204 |
| POST | `password/change` | вошедший | Сменить пароль → 204 |
| GET | `admin/users?limit=&offset=` | admin | Список пользователей |
| PATCH | `admin/users/{id}/role` | admin | Сменить роль (себя понизить нельзя) |
| PATCH | `admin/users/{id}/status` | admin | Заблокировать/разблокировать (себя нельзя) |
| POST | `dev/login` | все, при `DEV_LOGIN=true` | Вход без пароля |

**Ошибки:** тело `{"error": "..."}`.

| Код | Когда |
|---|---|
| 400 | Невалидные поля, недействительный или просроченный токен из письма |
| 401 | Нет токена, токен недействителен, неверный логин или пароль, аккаунт заблокирован |
| 403 | Роль ниже нужной; `email not verified` при входе по паролю |
| 404 | Не найдено; `dev/login` при выключенном `DEV_LOGIN` |
| 409 | Email или username заняты |
| 429 | Слишком много запросов |

## Тестовые пользователи (dev)

`make seed` загружает `seeds/dev.sql`. Пароль у всех **`password123`**, email у всех подтверждён.

| username | email | Роль | |
|---|---|---|---|
| `admin` | admin@nexus.local | admin | |
| `author` | author@nexus.local | author | |
| `user` | user@nexus.local | user | |
| `blocked` | blocked@nexus.local | user | заблокирован, вход → 401 |

Быстрее всего в Swagger: `POST /dev/login {"login": "admin"}`. Хеш пароля для своих сидов:
`cargo run -p auth --example hash_password -- 'пароль'`.

Дамп из прода позже заменит эти сиды, но только **после обезличивания**: email, имена, хеши
паролей и IP не должны попасть в репозиторий.

## Таблицы

### `users` ✅ — пользователи

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | Идентификатор |
| `email` | `citext` | NOT NULL, UNIQUE | Email; уникален без учёта регистра |
| `username` | `citext` | NOT NULL, UNIQUE | Логин, тоже без учёта регистра |
| `password_hash` | `text` | | Хеш Argon2id. `NULL` — пароль не задан (вход по ссылке или через провайдера) |
| `display_name` | `text` | | Отображаемое имя |
| `avatar_url` | `text` | | Ссылка на аватар |
| `role` | `user_role` | NOT NULL, default `user` | `user`, `author`, `admin` |
| `is_active` | `boolean` | NOT NULL, default `true` | `false` — заблокирован админом |
| `email_verified_at` | `timestamptz` | | Когда подтверждён email. `NULL` — вход по паролю запрещён |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |

**`user_role`** — enum (`user`, `author`, `admin`). Порядок значений задаёт старшинство, в SQL
работает `role >= 'author'`.

### `refresh_tokens` ✅ — сессии

Каждая запись — одна выдача refresh-токена. При обновлении создаётся новая, старая отзывается.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | id сессии (он же `sid` в access-токене) |
| `user_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | Чья сессия |
| `token_hash` | `text` | NOT NULL, UNIQUE | SHA-256 токена |
| `expires_at` | `timestamptz` | NOT NULL | Выдача + 30 дней |
| `revoked_at` | `timestamptz` | | Когда отозван. `NULL` — активен |
| `replaced_by` | `uuid` | FK → `refresh_tokens`, SET NULL | Чем заменён при обновлении. Задан и токен пришёл снова — кража |
| `user_agent` | `text` | | Браузер или устройство (для списка сессий) |
| `ip` | `inet` | | IP (из `X-Real-IP`) |
| `created_at` | `timestamptz` | NOT NULL | Когда выдан |

### `email_tokens` ✅ — одноразовые ссылки

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `purpose` | `email_token_purpose` | NOT NULL | `verify_email` (24 ч), `login` (15 мин), `reset_password` (1 ч), `oauth_login` (2 мин, код с OAuth-провайдера) |
| `email` | `citext` | NOT NULL | Куда отправлена |
| `user_id` | `uuid` | FK → `users`, CASCADE | `NULL` — вход по ссылке для адреса, у которого ещё нет аккаунта |
| `token_hash` | `text` | NOT NULL, UNIQUE | SHA-256 токена |
| `expires_at` | `timestamptz` | NOT NULL | |
| `used_at` | `timestamptz` | | Когда использован или погашен новым письмом |
| `created_at` | `timestamptz` | NOT NULL | |

### `oauth_accounts` ✅ — привязанные аккаунты провайдеров

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `provider` | `text` | PK | `google`, `github`, `yandex` |
| `provider_user_id` | `text` | PK | id пользователя у провайдера |
| `user_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | Наш пользователь |
| `email` | `citext` | | Email от провайдера на момент привязки |
| `created_at` | `timestamptz` | NOT NULL | |

### `oauth_states` ✅ — начатые входы через провайдера

`state_hash` (PK, SHA-256 параметра `state`), `provider`, `code_verifier` (PKCE), `expires_at` (10 минут).
Запись удаляется при возврате от провайдера или фоновой чисткой.

### `user_interests` 🕓 — интересы пользователя

Основа столпа «реалтайм по интересам» (см. [vision](../vision.md)): на какие сущности подписан
пользователь. Предлагается держать в модуле `social`: таблица ссылается на `entities`, и ею
пользуются лента и `realtime`.

## Дальше 🕓

- Профиль: смена username, display_name, аватара, email (с подтверждением нового адреса).
- Привязка и отвязка провайдеров из профиля (сейчас привязка автоматическая, по email).
- VK ID (нестандартный OAuth, нужен отдельный разбор профиля).
- Rate limiting в Redis — когда экземпляров приложения станет несколько.
