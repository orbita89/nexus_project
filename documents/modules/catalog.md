# Модуль `catalog`

**Отвечает за:** контентное ядро — сущности (фильмы, сериалы, книги, игры), людей и их роли,
теги, поиск через Meilisearch.
**URL:** `/api/v1/catalog` · **Код:** `modules/catalog/`

**Статус:** ✅ чтение (список, карточки, люди, теги), поиск через Meilisearch, админка, мини-дамп.
🕓 сортировка списка, фильтры по полям `metadata`, поиск людей в Meilisearch.

Почему всё лежит в одной таблице `entities` — в [architecture.md](../architecture.md#абстрактная-сущность).

## Таблицы

### `entities` ✅ — сущности (абстрактная сущность)

Главная таблица проекта. Одна строка — одно произведение любого типа. Форумы, рецензии,
коллекции и интересы ссылаются сюда.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | Идентификатор |
| `kind` | `entity_kind` | NOT NULL | Тип: `movie`, `series`, `book`, `game` |
| `slug` | `text` | NOT NULL, UNIQUE | Человекочитаемый id для URL: `dune-2021` |
| `title` | `text` | NOT NULL | Название (локализованное) |
| `original_title` | `text` | | Оригинальное название |
| `description` | `text` | | Описание |
| `release_date` | `date` | | Дата выхода или публикации |
| `cover_url` | `text` | | Постер или обложка |
| `metadata` | `jsonb` | NOT NULL, default `{}` | Поля, специфичные для типа (см. ниже) |
| `created_at` | `timestamptz` | NOT NULL | |
| `updated_at` | `timestamptz` | NOT NULL | Обновляется триггером |

**Индексы:** по `kind`; по `release_date` (новинки сверху); GIN по `metadata` (фильтры по полям
типа); триграммный GIN по `title` (поиск по подстроке, `ILIKE '%дюн%'`).

**`metadata` по типам** проверяет код `catalog` (`modules/catalog/src/metadata.rs`). Все поля
необязательны, поля чужого типа и неизвестные поля дают 400. Значения нормализуются: обрезаются
пробелы, пустые строки и списки отбрасываются, дубли в списках убираются.

| `kind` | Поле | Тип и проверка |
|---|---|---|
| `movie` | `runtime_min` | целое 1–10000, минуты |
| | `countries` | коды ISO 3166-1 alpha-2, приводятся к верхнему регистру: `["US", "CA"]` |
| | `age_rating` | строка до 16 символов: `PG-13`, `16+` |
| `series` | `seasons` | целое 1–10000 |
| | `episodes` | целое 1–100000 |
| | `status` | `ongoing` (идёт), `ended` (завершён), `canceled` (закрыт) |
| | `countries` | как у `movie` |
| `book` | `isbn` | ISBN-10 или ISBN-13 с проверкой контрольной цифры, хранится без дефисов |
| | `pages` | целое 1–100000 |
| | `publisher` | строка до 200 символов |
| `game` | `platforms` | до 30 строк по 50 символов: `["pc", "ps5", "switch"]` |
| | `developer`, `publisher` | строки до 200 символов |

**Новый тип** (комиксы, искусство): миграция `ALTER TYPE entity_kind ADD VALUE '...'`, вариант в
`EntityKind` (`models.rs`), структура и ветка `validate` в `metadata.rs`.

### `tags` ✅ — теги

Жанры, темы и метки: «научная фантастика», «киберпанк», «по реальным событиям». Общие для всех
типов, так что фильм и книга с одним тегом находятся вместе.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `slug` | `text` | NOT NULL, UNIQUE | Для URL: `sci-fi` |
| `name` | `text` | NOT NULL | Отображаемое название |

### `entity_tags` ✅ — теги сущностей

Связь многие-ко-многим между `entities` и `tags`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | |
| `tag_id` | `uuid` | PK, FK → `tags`, CASCADE | |

**Индексы:** PK (`entity_id`, `tag_id`) — теги сущности; отдельный по `tag_id` — все сущности с тегом.

### `people` ✅ — люди

Актёры, режиссёры, писатели, композиторы, разработчики игр. Один человек может участвовать в
произведениях разных типов: Стивен Кинг — автор книг и сценарист фильмов.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `slug` | `text` | NOT NULL, UNIQUE | Для URL: `denis-villeneuve` |
| `full_name` | `text` | NOT NULL | Имя |
| `birth_date` | `date` | | Дата рождения |
| `photo_url` | `text` | | Фото |
| `bio` | `text` | | Биография |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |

**Индексы:** триграммный GIN по `full_name` — поиск людей по подстроке.

### `entity_credits` ✅ — участие людей в произведениях

Кто и в какой роли участвовал: режиссёр фильма, актёр и его персонаж, автор книги.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `entity_id` | `uuid` | NOT NULL, FK → `entities`, CASCADE | Произведение |
| `person_id` | `uuid` | NOT NULL, FK → `people`, CASCADE | Человек |
| `role` | `text` | NOT NULL | `actor`, `director`, `author`, `composer`, ... |
| `character_name` | `text` | | Персонаж (для актёров) |
| `position` | `integer` | NOT NULL, default 0 | Порядок в титрах |

**Уникальность:** `UNIQUE NULLS NOT DISTINCT` (`entity_id`, `person_id`, `role`, `character_name`).
Один человек может сыграть двух персонажей, но одна и та же роль не дублируется. `NULLS NOT DISTINCT`
нужен для ролей без персонажа (режиссёр, автор): обычный `UNIQUE` считает `NULL`'ы разными и
пропустил бы дубль. Исправлено миграцией `20261006120000_entity_credits_unique_nulls.sql`.
**Индексы:** (`entity_id`, `position`) — титры по порядку; `person_id` — фильмография человека.

## Поиск

Сущности индексируются в Meilisearch (индекс `entities`): опечатки, ранжирование, фильтры по `kind`,
тегу и году. Ищется по названию, оригинальному названию, именам участников, названиям тегов и
описанию, в таком порядке важности. Клиент — тонкая обёртка над HTTP API в `shared::search`, без SDK.

Источник правды — PostgreSQL, индекс производный:

- **После записи в админке** затронутые сущности сразу переотправляются в индекс: сама сущность,
  все сущности с изменённым тегом, все работы переименованного человека. Meilisearch применяет
  изменения асинхронно, обычно за доли секунды. Если он недоступен, запись в БД всё равно проходит,
  а в лог пишется warning.
- **Полная перестройка**: при старте приложения (в фоне) и `POST /admin/search/reindex` (ждёт
  завершения). Новый индекс строится во временном `entities_reindex` и подменяет старый через
  swap, так что поиск не пустеет. Изменения, сделанные во время перестройки, могут потеряться:
  перестройка нужна редко, в этом случае её можно повторить.
- Данные, загруженные в БД в обход API (`make seed`), попадают в индекс только после перестройки:
  `make seed` запускает её сам (`scripts/reindex-search.sh`).

Если Meilisearch недоступен, `GET /search` отвечает `503`. Если индекс ещё не построен, ответ —
пустая страница. Остальной каталог от Meilisearch не зависит.

### Что лежит в индексе

Документ индекса `entities` — поля для карточки в выдаче (`id`, `slug`, `kind`, `title`,
`original_title`, `description`, `release_date`, `cover_url`) и служебные поля для поиска и фильтров:
`year`, `tags` (slug'и тегов), `tag_names` (названия тегов), `people` (имена участников).

Ключ в dev — `nexus_dev_master_key` (`MEILI_MASTER_KEY` в `infra/docker-compose.yml`).

**Веб-панель Meilisearch:** http://localhost:7700 (работает при `MEILI_ENV=development`). Ввести
ключ, слева выбрать индекс `entities`: пустой поиск показывает все документы, поиск работает на лету.
Индекс `entities_reindex` существует только во время перестройки. Индексы `test_…` — остатки тестов,
упавших до очистки; на приложение не влияют, их можно удалить.

**Через curl:**

```bash
KEY='Authorization: Bearer nexus_dev_master_key'

curl -s localhost:7700/indexes -H "$KEY" | jq                                 # индексы
curl -s localhost:7700/indexes/entities/stats -H "$KEY" | jq                  # numberOfDocuments
curl -s 'localhost:7700/indexes/entities/documents?limit=20' -H "$KEY" | jq   # документы
curl -s localhost:7700/indexes/entities/documents/<uuid> -H "$KEY" | jq       # документ по id сущности
curl -s localhost:7700/indexes/entities/settings -H "$KEY" | jq               # поля поиска и фильтров
curl -s 'localhost:7700/tasks?limit=10' -H "$KEY" | jq                        # задачи, ошибки индексации
```

**Через API Nexus** — то, что увидит фронтенд: http://localhost/api/v1/catalog/search?limit=100
(пустой `q` возвращает всё, `total` — сколько документов в индексе) или вкладка «Каталог» в Swagger.

Если индекс разошёлся с БД — перестроить: `scripts/reindex-search.sh` или
`POST /admin/search/reindex` в Swagger.

## Эндпоинты

Все пути — от `/api/v1/catalog`. В Swagger (http://localhost/docs) каталог — отдельная вкладка:
**Select a definition** → **Каталог**, схема `/api-docs/catalog.json`, контракт в репозитории —
`documents/api/catalog.json`. Токен для админки берётся во вкладке **Nexus API**. Ошибки — `{"error": "..."}`, в том числе для неразобранного
JSON-тела (400, экстрактор `shared::extract::JsonBody`).

### Чтение (без авторизации)

| Метод | Путь | Что делает |
|---|---|---|
| GET | `/entities` | Список. Фильтры: `kind`, `tag` (slug), `year`, `q` (подстрока в названии или оригинальном, без учёта регистра). Новинки сверху, без даты — в конце |
| GET | `/entities/{slug}` | Карточка: все поля, теги, участники в порядке титров (`position`) |
| GET | `/people` | Список людей по алфавиту, `q` — подстрока в имени |
| GET | `/people/{slug}` | Человек и все его работы любых типов, новые сверху |
| GET | `/tags` | Все теги с `entities_count`, без пагинации (тегов немного) |
| GET | `/search` | Поиск через Meilisearch: `q`, фильтры `kind`, `tag`, `year`. По релевантности |

**Пагинация** списков и поиска: `?limit=20&offset=0` (`limit` 1–100, по умолчанию 20; значения вне
диапазона приводятся к границам). Ответ:

```json
{ "items": [ ... ], "total": 137, "limit": 20, "offset": 0 }
```

В поиске `total` — оценка Meilisearch (`estimatedTotalHits`).

### Админка (только `admin`, иначе 401/403)

| Метод | Путь | Что делает |
|---|---|---|
| POST | `/admin/entities` | Создать сущность (можно сразу с `tags`: slug'и) → 201, карточка |
| PATCH | `/admin/entities/{id}` | Изменить: переданные поля заменяются, `null` очищает необязательное, `metadata` заменяется целиком. `kind` не меняется |
| DELETE | `/admin/entities/{id}` | Удалить (каскадно теги, участники, рецензии, элементы коллекций) → 204 |
| PUT | `/admin/entities/{id}/tags` | Заменить набор тегов: `{"tags": ["sci-fi"]}` |
| POST | `/admin/entities/{id}/credits` | Добавить участника: `person_id`, `role`, `character_name`, `position` → 201 |
| DELETE | `/admin/entities/{id}/credits/{credit_id}` | Удалить участника (`credits[].id` из карточки) → 204 |
| POST | `/admin/people` | Добавить человека → 201 |
| PATCH / DELETE | `/admin/people/{id}` | Изменить (`null` очищает) / удалить вместе с участием |
| POST | `/admin/tags` | Создать тег → 201 |
| PATCH / DELETE | `/admin/tags/{id}` | Переименовать или сменить slug / удалить (снимается со всех сущностей) |
| POST | `/admin/search/reindex` | Перестроить поисковый индекс → `{"indexed": 46}` |

**Проверки:** `slug` — `a-z`, `0-9`, одиночные дефисы, до 100 символов; занятый slug — 409.
Названия и имена обязательны, до 300 символов, пробелы по краям обрезаются. `cover_url` и
`photo_url` — только `http(s)://`. `role` — `a-z` и `_` (`actor`, `voice_actor`, `director`,
`writer`, `author`, `composer`, `creator`, ...). Дубль участия — 409, неизвестный `person_id` — 400,
неизвестный тег — 400 со списком таких тегов.

## Мини-дамп

`seeds/catalog.sql` (загружает `make seed`): 46 сущностей всех четырёх типов, 38 людей, 18 тегов.
Подобраны франшизы, где одно произведение есть в нескольких типах: «Дюна» (книги и три фильма),
«Ведьмак» (книга, сериал, игра), Средиземье, Кинг, «Пикник на обочине» → «Сталкер» (фильм и игры),
«Метро 2033», «Одни из нас» и др. Повторная загрузка безопасна (`ON CONFLICT DO NOTHING`).

Тест `seed_catalog_is_valid` загружает дамп в чистую БД и проверяет, что вся `metadata` проходит
валидацию API и уже нормализована. Связи в дампе пишутся через подзапросы по slug, поэтому
опечатка в slug роняет загрузку, а не теряет строку молча.

## Код

| Файл | Что внутри |
|---|---|
| `lib.rs` | Роутер |
| `models.rs` | `EntityKind`, DTO; `Page<T>` реэкспортируется из `shared::pagination` |
| `directory.rs` | `PgEntityDirectory`: справочник сущностей для других модулей (`shared::directory::EntityDirectory`) |
| `metadata.rs` | Схемы `metadata` по типам и их проверка |
| `validate.rs` | Проверка slug, строк, URL, ролей; UNIQUE → 409 |
| `entities.rs`, `people.rs`, `tags.rs` | Чтение |
| `search.rs` | Индексация, перестройка, `GET /search` |
| `admin/` | Админские эндпоинты |

Тесты — `app/tests/catalog.rs` (данные — `app/tests/fixtures/catalog.sql`). Тесты поиска ходят в
настоящий Meilisearch из `make up` (в CI — сервис в `ci.yml`), у каждого теста свой префикс
индексов. Остальные тесты идут с выключенным поиском. Живые проверки — `http/catalog.http`.
