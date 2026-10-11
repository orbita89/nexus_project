# Модуль `catalog` — LOD 2

**Summary (EN):** content core — entities (movies/series/books/games) with `metadata jsonb`, people & credits, tags, relations, rating summaries, Meilisearch search, admin publishing (search + ISR), zero-downtime reindex, media (posters/trailers).
**Отвечает за:** контентное ядро — сущности (фильмы, сериалы, книги, игры), людей и их роли,
теги, поиск через Meilisearch.
**URL:** `/api/v1/catalog` · **Код:** `modules/catalog/` · **Источник (LOD 3):** `documents/modules/catalog.md`

**Статус:** ✅ чтение (список с сортировками, карточки из PostgreSQL со связанными произведениями,
похожие, люди, теги и жанры), поиск через Meilisearch, админка с публикацией правок (поиск, ISR
фронтенда) и потоком шагов (SSE), перестройка поиска без простоя с версиями индексов, проверкой
размера и потоком прогресса, мини-дамп, импорт фильмов из TMDB и Wikidata (`scripts/catalog-import`).
🕓 фильтры по полям `metadata`, поиск людей в Meilisearch.

Почему всё лежит в одной таблице `entities` — в [architecture.md](../../documents/architecture.md#абстрактная-сущность).

<tables>

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
| | `trailers` | трейлер: до 5 источников по приоритету, `[{"provider": "youtube", "id": "n9xhJrPXop4"}, {"provider": "rutube", "id": "0ab1…"}]`. `provider` — `youtube` (id 11 символов) или `rutube` (32 hex). На входе можно дать и просто ссылку на видео (`youtube.com/watch?v=…`, `youtu.be/…`, `rutube.ru/video/…/`, `/embed/…`) — провайдер и id определятся сами; другие сайты — 400. Повторы убираются |
| `series` | `seasons` | целое 1–10000 |
| | `episodes` | целое 1–100000 |
| | `status` | `ongoing` (идёт), `ended` (завершён), `canceled` (закрыт) |
| | `countries` | как у `movie` |
| | `trailers` | как у `movie` |
| `book` | `isbn` | ISBN-10 или ISBN-13 с проверкой контрольной цифры, хранится без дефисов |
| | `pages` | целое 1–100000 |
| | `publisher` | строка до 200 символов |
| `game` | `platforms` | до 30 строк по 50 символов: `["pc", "ps5", "switch"]` |
| | `developer`, `publisher` | строки до 200 символов |
| | `trailers` | как у `movie` |
| `movie`, `series` | `studios` | студии производства: до 20 строк по 1–120 символов (у игр — `developer`/`publisher`, у книг — `publisher`) |
| все типы | `awards` | награды и номинации, до 50: `[{"title": "«Оскар»: лучший звук", "year": 2022, "won": true}]`. `title` 1–200 символов, `year` 1880–2100 или `null`, `won` — получена (`false` — номинация, по умолчанию) |
| | `trivia` | факты: до 30 строк по 1–1000 символов |

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
| `kind` | `text` | NOT NULL, default `tag`, `genre` / `tag` | Жанр («Драма», «Космическая опера») или остальная метка («Экранизация», «Открытый мир»). Миграция `20261015110000_tag_kinds.sql` |

Жанры — те же теги с `kind = genre`: фильтр `?tag=`, поиск и похожие работают одинаково, а фронтенд
показывает жанры в шапке карточки, остальные теги — над «О фильме». В карточке `tags` — все теги,
жанры первыми. Какие теги жанры, решает админка (`kind` в `/admin/tags`); сидовые размечены
списком в `seeds/catalog.sql`.

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

### `entity_relations` ✅ — связанные произведения

Сиквелы и ремейки. Миграция `20261015100000_entity_relations.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `from_id` | `uuid` | PK, FK → `entities`, CASCADE | |
| `to_id` | `uuid` | PK, FK → `entities`, CASCADE | Сиквел (ремейк) `from` |
| `kind` | `text` | PK, `sequel` / `remake` | |
| `position` | `integer` | NOT NULL, default 0 | Порядок при одинаковой дате выхода |
| `created_at` | `timestamptz` | NOT NULL | |

Храним одно направление: приквелы — обратное чтение `sequel`, ремейк виден с обеих сторон. Связь с
собой запрещена (`CHECK`), одна и та же пара одного вида — только в одну сторону (проверяет код).
**Индексы:** PK — сиквелы и ремейки сущности; `to_id` — приквелы и оригиналы.

### `entity_ratings` ✅ — сводки оценок для сортировок

Миграция `20261015130000_entity_ratings.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | |
| `average` | `double precision` | 1–10 или NULL | Средняя оценка без округления |
| `count` | `bigint` | NOT NULL, ≥ 0 | Сколько оценок |
| `updated_at` | `timestamptz` | NOT NULL | |

**Решение:** рецензии — таблица `social`, читать её каталогу нельзя. Поэтому сортировки `popular` и
`rating` идут по денормализованной копии в каталоге, которую пишет `social` через трейт
`shared::directory::EntityRatings` (реализация — `catalog::directory::PgEntityRatings`): после
каждой записи или удаления рецензии — сводку этой сущности, при старте приложения — все сводки
целиком (`replace_all`). Значения абсолютные, поэтому сбой отправки исправляет следующая запись или
перезапуск. Карточку и статику сводки не меняют (оценка на карточке — `GET /social/.../rating`).
**Индексы:** `count DESC`; (`average DESC NULLS LAST`, `count DESC`).

</tables>

<deps>

## Зависимости: справочники и интеграции

| Что | Где | Назначение |
|---|---|---|
| `EntityDirectory` (реализует `catalog`: `PgEntityDirectory`) | `shared::directory`, `state.entities` | `social`/другие читают сущности без JOIN к `entities` |
| `EntityRatings` (реализует `catalog`: `PgEntityRatings`) | `shared::directory`, `state.ratings` | `social` пишет сводки оценок для сортировок `popular`/`rating` |
| Поиск | `shared::search` (HTTP API Meilisearch через reqwest, без SDK) | индекс `entities`, `update_index`, перестройка |
| ISR-вебхук фронтенда | `shared::isr` (`POST {ISR_URL}/_isr/revalidate`) | пересборка статики после правок |
| Блокировки перестройки | Redis (`shared::cache`): `nexus:v1:catalog:reindex-running`, `nexus:v1:catalog:reindex-lock` | «одна перестройка за раз» между инстансами |

</deps>

<logic>

## Поиск

Сущности индексируются в Meilisearch (индекс `entities`): опечатки, ранжирование, фильтры по `kind`,
тегу и году. Ищется по названию, оригинальному названию, именам участников, названиям тегов и
описанию, в таком порядке важности. Клиент — тонкая обёртка над HTTP API в `shared::search`, без SDK.

Источник правды — PostgreSQL, индекс производный:

- **После записи в админке** затронутые сущности сразу переотправляются в индекс: сама сущность,
  все сущности с изменённым тегом, все работы переименованного человека. Meilisearch применяет
  изменения асинхронно, обычно за доли секунды. Если он недоступен, запись в БД всё равно проходит,
  а в лог пишется warning.
- **Полная перестройка** без простоя, с версиями индексов: при старте приложения (в фоне), по
  расписанию и из админки — [«Перестройка поиска»](#перестройка-поиска-).
- Данные, загруженные в БД в обход API (`make seed`), попадают в индекс только после перестройки:
  `make seed` запускает её сам (`scripts/reindex-search.sh`).

Если Meilisearch недоступен, `GET /search` отвечает `503`. Если индекс ещё не построен, ответ —
пустая страница. Карточки, списки, теги и справочники для `social` от Meilisearch не зависят.

### Что лежит в индексе

Документ индекса `entities` — поля для карточки в выдаче (`id`, `slug`, `kind`, `title`,
`original_title`, `description`, `release_date`, `cover_url`) и служебные поля для поиска и фильтров:
`year`, `tags` (slug'и тегов), `tag_names` (названия тегов), `people` (имена участников) и `card` —
полная карточка сущности. `card` больше никто не читает (карточка — из БД и статики фронтенда), поле
пока оставлено; не ищется, поиском не отдаётся.

Индекса людей нет: он хранил только карточки, теперь они из БД. Оставшийся от прежних версий
индекс `people` ни на что не влияет, его можно удалить: `curl -X DELETE localhost:7700/indexes/people -H "$KEY"`.

Ключ в dev — `nexus_dev_master_key` (`MEILI_MASTER_KEY` в `infra/docker-compose.yml`).

**Веб-панель Meilisearch:** http://localhost:7700 (работает при `MEILI_ENV=development`). Ввести
ключ, слева выбрать индекс `entities`: пустой поиск показывает все документы, поиск работает на лету.
Индексы `entities_v<ms>` — предыдущая версия и (во время перестройки) теневая. Индексы `test_…` — остатки тестов,
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

## Карточки ✅

`GET /entities/{slug}` и `GET /people/{slug}` читают PostgreSQL: без кэша, без Redis и без
Meilisearch. Публичные страницы отдаёт фронтенд статикой (SSG/ISR), сюда приходят её пересборка и
админка, поэтому кэш не нужен. После правки бэкенд просит фронтенд пересобрать страницы
([«Публикация»](#публикация-правок-)). `moka` и `shared::cache` остаются в проекте; каталог использует
Redis только для блокировок перестройки.

## Публикация правок ✅

Код — `publish.rs`, клиент вебхука — `shared::isr`. После коммита каждой админской записи (и
`nexus media`) `publish::run` выполняет шаги по порядку:

1. **search** — документы затронутого в Meilisearch, с ожиданием применения (`search::update_index`);
   во время перестройки — и в теневой индекс.
2. **isr** — `POST {ISR_URL}/_isr/revalidate`, `Authorization: Bearer {ISR_SECRET}`,
   тело `{"paths": ["/films/dune-2021", "/people/denis-villeneuve"]}`, пачками по 100 путей.
   Пути — старые и новые адреса затронутых сущностей (`/films|series|books|games/{slug}`, как
   `KINDS` фронтенда) и людей (`/people/{slug}`).

Ошибка шага запись не отменяет и следующий шаг не останавливает: warning в лог. Meilisearch
выключен или не заданы `ISR_URL`/`ISR_SECRET` — шаг пропускается (`skipped`).

**Поток для админки:** `PATCH /admin/entities/{id}/stream` — то же тело, что у `PATCH /admin/entities/{id}`.
Ошибки ввода (`400`, `404`, `409`) — обычный JSON до потока. Если запись прошла — `200 text/event-stream`:

```
event: plan
data: {"type":"plan","steps":[{"step":"db","title":"Сохранение в БД"},{"step":"search",...},{"step":"isr",...}]}

event: step
data: {"type":"step","step":"db","status":"done","message":"БД обновлена","duration_ms":4}

event: step
data: {"type":"step","step":"search","status":"running","message":"Обновление поиска (Meilisearch)"}

event: step
data: {"type":"step","step":"search","status":"done","message":"Meilisearch обновлён","duration_ms":61}

event: step
data: {"type":"step","step":"isr","status":"running","message":"Пересборка статики (ISR)"}

event: step
data: {"type":"step","step":"isr","status":"done","message":"Статический HTML/JSON пересобран: /films/dune-2021, ...","duration_ms":120}

event: done
data: {"type":"done","ok":true,"entity":{ ...карточка как в GET /entities/{slug}... }}
```

`status`: `running`, `done`, `skipped`, `failed`; `ok: false` — какой-то шаг упал. Схема —
`JobEvent` в `documents/api/catalog.json` (общая с перестройкой). `EventSource` не умеет PATCH и заголовки: читать
через `fetch` и `response.body`. Публикация доводится до конца, даже если вкладку закрыли. Для nginx
ответ помечен `X-Accel-Buffering: no`, иначе поток придёт одним куском.

Что затронуто, собирает `publish::Touched`: связи раскрываются сами (у сущности — её участники и
связанные произведения, у человека — его работы), а slug'и запоминаются **до** записи, поэтому удаление и смена slug пересобирают
и старые адреса.

| Запись | Что обновляется в Meilisearch и пересобирается на фронтенде |
|---|---|
| сущность: создать | она |
| сущность: изменить, удалить, теги, участники | она, все её участники (их фильмография показывает сущность) и связанные произведения (их `related` показывает сущность) |
| связь: добавить, удалить | обе сущности |
| участник: добавить, удалить | сущность и человек |
| тег: изменить, удалить | все сущности с тегом и их участники |
| человек: создать | он |
| человек: изменить (любое поле), удалить | он и все его работы (в карточке сущности есть slug, имя и фото) |
| `nexus media` (отдельный процесс) | изменённые сущности и их участники |
| `make seed` (SQL в обход API) | перестройка |

## Перестройка поиска ✅

Код — `search.rs` (`reindex`), события — `jobs.rs`. Прямая очистка живого индекса не используется
никогда. Поиск всегда читает индекс `entities` — он играет роль алиаса (в Meilisearch 1.15 алиасов
нет), а версии подменяют его атомарным `swap-indexes`:

1. **create_index** — теневой `entities_v<unix ms>` с настройками; `entities` создаётся, если его нет.
2. **fill** — все сущности из PostgreSQL пачками по 500 (keyset по `id`, карточки пачки — двумя
   запросами) в теневой индекс. Пока Meilisearch применяет пачку, читается следующая. Событие
   `progress` после каждой пачки: `{"done": 1000, "total": 100000}`.
3. **check** — новый индекс должен быть **не меньше `SEARCH_REINDEX_MIN_PERCENT`%** текущего
   (`numberOfDocuments`; по умолчанию и в проде — 80, 0–100, иначе приложение не стартует). Меньше —
   значит, выборка сломана или неполная: поиск не переключается, теневой индекс удаляется, ответ `409`
   (в потоке — `check: failed`). Если сущности удалены намеренно — `?force=true`. Текущий пуст —
   проверка проходит. **В dev** (`infra/docker-compose.yml`) — `0`, проверки нет: каталог часто
   перезаливается целиком (импорт из TMDB/Wikidata, откат), и перестройка при старте не должна
   упираться в старый большой индекс.
4. **swap** — `entities ⇄ entities_v<ms>`: поиск на всех инстансах сразу видит новые документы, а в
   `entities_v<ms>` остаётся предыдущая версия («на всякий случай»; откат — swap обратно).
5. **rotate** — хранятся строго две версии: текущая (`entities`) и предыдущая (`entities_v<ms>` только
   что). Все `entities_v*` старше удаляются: «Удалены устаревшие индексы: …».
6. **isr** — `POST {ISR_URL}/_isr/revalidate` с `{"all": true}`: фронтенд отвечает `202` и
   пересобирает всю статику в фоне, поэтому шаг значит «запущено», а не «готово» (ход — в логе фронтенда). Только при
   ручном запуске; при старте и по расписанию — нет.

- **Одна за раз.** В процессе — набор идущих перестроек, между инстансами — блокировка
  `nexus:v1:catalog:reindex-running` в Redis (TTL 2 ч на случай падения, снимается в конце). Занято —
  `409` до потока. Без Redis перестраивает каждый (warning).
- **Правки во время перестройки** пишутся и в живой, и в теневой индекс, поэтому после swap не
  теряются. Ограничение: только правки на том же инстансе, где идёт перестройка.
- **По расписанию** (`SEARCH_REINDEX_INTERVAL_SECS=86400`, `0` — только при старте и вручную)
  перестраивает один инстанс: блокировка `nexus:v1:catalog:reindex-lock` на интервал.
- **Ошибка шага** — `failed` с текстом, следующие шаги не выполняются, недостроенный теневой индекс
  удаляется (не удалось — его уберёт следующая ротация).

**Поток для админки:** `POST /admin/search/reindex/stream[?force=true]` → `text/event-stream`:

```
event: plan      data: {"type":"plan","steps":[{"step":"create_index",...},{"step":"fill",...},{"step":"check",...},{"step":"swap",...},{"step":"rotate",...},{"step":"isr",...}]}
event: step      data: {"type":"step","step":"create_index","status":"done","message":"Создан теневой индекс entities_v1759912345123","duration_ms":35}
event: progress  data: {"type":"progress","step":"fill","done":500,"total":100000}
event: step      data: {"type":"step","step":"check","status":"done","message":"Новый индекс: 100000 документов, текущий: 99870 (100%)"}
event: step      data: {"type":"step","step":"rotate","status":"done","message":"Удалены устаревшие индексы: entities_v1759800000000"}
event: done      data: {"type":"done","ok":true,"reindex":{"indexed":100000,"previous_count":99870,"previous_version":"entities_v1759912345123","deleted":["entities_v1759800000000"]}}
```

Без потока — `POST /admin/search/reindex[?force=true]`, ответ — тот же `reindex` (`ReindexResult`).

**Тесты** — `app/tests/catalog.rs`: хранятся ровно две версии, отказ переключаться при 20% и
`force=true`, ровно 80% — переключение, `SEARCH_REINDEX_MIN_PERCENT=0` — переключение без `force`
(тесты задают 80 явно, независимо от окружения), поток (шаги, `progress`, ротация, `{"all": true}` во
фронтенд), отказ в потоке (`check: failed`, поиск прежний), `409` при идущей перестройке.

</logic>

<endpoints>

## Эндпоинты

Все пути — от `/api/v1/catalog`. В Swagger (http://localhost/docs) каталог — отдельная вкладка:
**Select a definition** → **Каталог**, схема `/api-docs/catalog.json`, контракт в репозитории —
`documents/api/catalog.json`. Токен для админки берётся во вкладке **Nexus API**. Ошибки — `{"error": "...", "code": "..."}`, в том числе для неразобранного
JSON-тела (400, экстрактор `shared::extract::JsonBody`).

### Чтение (без авторизации)

| Метод | Путь | Что делает |
|---|---|---|
| GET | `/entities` | Список. Фильтры: `kind`, `tag` (slug), `year`, `q` (подстрока в названии или оригинальном, без учёта регистра). `sort`: `new` (по умолчанию: новинки сверху, без даты — в конце), `popular` (больше оценок), `rating` (выше средняя; без оценок — в конце), `title` |
| GET | `/entities/{slug}` | Карточка: все поля, теги, участники в порядке титров (`position`), `related`. Из PostgreSQL |
| GET | `/entities/{slug}/similar` | Похожие: `?limit=` 1–24 (по умолчанию 12) → `Page<EntitySummary>` ([«Похожие»](#похожие-)) |
| GET | `/people` | Список людей по алфавиту, `q` — подстрока в имени |
| GET | `/people/{slug}` | Человек и все его работы любых типов, новые сверху. Из PostgreSQL |
| GET | `/tags` | Все теги с `kind` и `entities_count`, без пагинации (тегов немного); `?kind=genre\|tag` — только жанры или только теги |
| GET | `/tags/{slug}` | Тег: `id`, `slug`, `name`, `kind`, `entities_count`; нет — 404 |
| GET | `/search` | Поиск через Meilisearch: `q`, фильтры `kind`, `tag`, `year`. По релевантности |

**`related`** в карточке — одна лента для карусели: `[{...EntitySummary, "relation": "prequel"|"sequel"|"remake"}, ...]`, хронологически по `release_date` (без даты — в конце, затем `position`, затем заголовок), пусто — `[]`. Сиквелы, ремейки и приквелы идут **в одной полосе**, тип связи — у каждой карточки. Связи — в самой карточке, а не отдельным запросом: карточка — статика фронтенда, они попадают в HTML.

**Пагинация** списков и поиска: `?limit=20&offset=0` (`limit` 1–100, по умолчанию 20; значения вне
диапазона приводятся к границам). Ответ:

```json
{ "items": [ ... ], "total": 137, "limit": 20, "offset": 0 }
```

В поиске `total` — оценка Meilisearch (`estimatedTotalHits`).

### Похожие ✅

`GET /entities/{slug}/similar` — первая версия без ML, один SQL-запрос (`entities::similar`):

- тот же `kind`; кандидаты — сущности с общими тегами, счёт — сумма весов общих тегов: жанр — 2,
  остальной тег — 1;
- выше — больше общих тегов, при равенстве — новее (`release_date DESC NULLS LAST`);
- без самой сущности и её связанных (`related` и так на карточке);
- нет тегов — пустая страница (`total: 0`), не 404; `offset` нет, `total` — сколько кандидатов всего.

План (`EXPLAIN`): теги сущности — по PK `entity_tags`, сущности с этими тегами — по
`entity_tags_tag_id_idx`, затем агрегат по кандидатам. На очень частых тегах кандидатов много;
когда это станет заметно — предрасчёт или Meilisearch.

### Админка (только `admin`, иначе 401/403)

| Метод | Путь | Что делает |
|---|---|---|
| POST | `/admin/entities` | Создать сущность (можно сразу с `tags`: slug'и) → 201, карточка |
| PATCH | `/admin/entities/{id}` | Изменить: переданные поля заменяются, `null` очищает необязательное, `metadata` заменяется целиком. `kind` не меняется |
| PATCH | `/admin/entities/{id}/stream` | То же, ход публикации потоком `text/event-stream` ([«Публикация»](#публикация-правок-)) |
| DELETE | `/admin/entities/{id}` | Удалить (каскадно теги, участники, рецензии, элементы коллекций) → 204 |
| PUT | `/admin/entities/{id}/tags` | Заменить набор тегов: `{"tags": ["sci-fi"]}` |
| POST | `/admin/entities/{id}/credits` | Добавить участника: `person_id`, `role`, `character_name`, `position` → 201 |
| DELETE | `/admin/entities/{id}/credits/{credit_id}` | Удалить участника (`credits[].id` из карточки) → 204 |
| PUT | `/admin/entities/{id}/relations` | Связать: `{"kind": "sequel" \| "prequel" \| "remake", "to_id": "...", "position"?: 0}` — `to_id` сиквел, приквел или ремейк сущности → 200, `related` сущности. С собой — 409 `self_relation`; такая связь уже есть (в любую сторону) — 409; нет `to_id` — 400 |
| DELETE | `/admin/entities/{id}/relations/{to_id}?kind=...` | Убрать связь того же вида (`prequel` — со стороны сиквела, ремейк — с любой) → 204, нет — 404 |
| POST | `/admin/people` | Добавить человека → 201 |
| PATCH / DELETE | `/admin/people/{id}` | Изменить (`null` очищает) / удалить вместе с участием |
| POST | `/admin/tags` | Создать тег: `slug`, `name`, `kind` (`genre` / `tag`, по умолчанию `tag`) → 201 |
| PATCH / DELETE | `/admin/tags/{id}` | Переименовать, сменить slug или `kind` / удалить (снимается со всех сущностей). Карточки с тегом пересобираются |
| POST | `/admin/search/reindex` | Перестроить поисковый индекс без простоя (`?force=true` — без проверки размера, `SEARCH_REINDEX_MIN_PERCENT`: 80, в dev 0) → `ReindexResult`; `409` — уже идёт или новый индекс слишком мал |
| POST | `/admin/search/reindex/stream` | То же потоком `text/event-stream` ([«Перестройка поиска»](#перестройка-поиска-)) |

**Проверки:** `slug` — `a-z`, `0-9`, одиночные дефисы, до 100 символов; занятый slug — 409.
Названия и имена обязательны, до 300 символов, пробелы по краям обрезаются. `cover_url` и
`photo_url` — только `http(s)://`. `role` — `a-z` и `_` (`actor`, `voice_actor`, `director`,
`writer`, `author`, `composer`, `creator`, ...). Дубль участия — 409, неизвестный `person_id` — 400,
неизвестный тег — 400 со списком таких тегов.

</endpoints>

<logic>

## Постеры и трейлеры: `nexus media`

Медиа — ссылки: `cover_url` и `metadata.trailers` (см. таблицу выше). Ищет и проверяет их
команда приложения (код — `modules/catalog/src/media/`):

```bash
make media                                   # fill: заполнить пустое у всех сущностей
make media MODE=check                        # проверить ссылки, мёртвые заменить
make media ARGS="--dry-run --slug it-2017"   # только показать, что изменилось бы
```

В проде — `nexus media check` по cron раз в сутки ([deploy.md](../../documents/deploy.md#постеры-и-трейлеры-по-расписанию)).

| Тип | Постер | Трейлер YouTube | Запасной трейлер |
|---|---|---|---|
| `movie`, `series` | TMDB: русский постер, затем английский, затем без текста; внутри — по голосам | TMDB: только `official`, тип `Trailer`, самый ранний | Rutube |
| `game` | IGDB (`t_cover_big_2x`) | IGDB: релизный / кинематографический трейлер | Rutube |
| `book` | Open Library: по ISBN, иначе по названию и автору | — | — |

Ключи (`infra/.env`, в `.gitignore`): `TMDB_API_KEY` — API key или Read Access Token
(themoviedb.org → Settings → API); `TWITCH_CLIENT_ID`, `TWITCH_CLIENT_SECRET` — приложение на
dev.twitch.tv/console (IGDB принадлежит Twitch). Без ключа источник пропускается, отчёт об этом
предупреждает. Книги и Rutube ключей не требуют.

**Правила:**
- **fill** заполняет только пустое: постер, если нет `cover_url`; YouTube и Rutube, если такого
  источника нет. Подобранное вручную не перезаписывается.
- **check** сначала проверяет текущие ссылки: картинка — `GET` (200 и `image/*`), YouTube —
  oEmbed (отвечает только для существующих видео, которые разрешено встраивать), Rutube —
  `api/video/{id}`. Удаляется только **точно мёртвое** (404, 410, видео скрыто или удалено);
  сеть, 5xx и 403 — «неизвестно», ссылка остаётся: иначе один сбой связи стёр бы все постеры.
  Потом — как fill. Мёртвое без замены просто удаляется: лучше заглушка, чем битая картинка.
- Всё найденное проверяется перед записью. Запись — через ту же проверку `metadata`, что у
  админки, затем `publish::run` (постер есть в поисковом индексе и на странице фронтенда).
- **Rutube** — официальных каналов там нет, поэтому фильтр строгий (`media/pick.rs`): 30 с –
  6 мин; в названии «трейлер»/«тизер», название произведения и только «служебные» слова (год,
  «русский», «дубляж», «4K», слова своего типа — «фильм», «сериал», «игра» и платформы); не
  сиквел («Оно 2»); год, если указан, совпадает. Если в каталоге есть одноимённое произведение
  того же типа (две «Дюны»), год в названии ролика обязателен. Найденное попадает в раздел
  отчёта «Проверить глазами». Тесты фильтра — на настоящих выдачах Rutube, включая его прошлые
  ошибки (трейлер игры вместо фильма, мод, ремастер, фильм целиком).

</logic>

<events>

## События для EventBus

`catalog` **не публикует** событий в `shared::events::EventBus`: он сообщает об изменениях напрямую —
`search::update_index` (Meilisearch) и `shared::isr` (пересборка статики фронтенда) внутри
`publish::run`. Очередь событий модулю не нужна.

</events>

<seed>

## Мини-дамп

`seeds/catalog.sql` (загружает `make seed`): 46 сущностей всех четырёх типов, 38 людей, 18 тегов,
связи: `dune-1984` ↔ `dune-2021` (ремейк), сиквелы «Дюны», «Властелина колец» (с «Хоббитом» 2012
приквелом), «Бегущего по лезвию», S.T.A.L.K.E.R. и «Метро».
Теги-жанры (`sci-fi`, `drama`, `space-opera`, `horror`, …) размечены `kind = genre`, `based-on-book`,
`open-world`, `classic`, `philosophy` — теги. Подобраны франшизы, где одно произведение есть в нескольких типах: «Дюна» (книги и три фильма),
«Ведьмак» (книга, сериал, игра), Средиземье, Кинг, «Пикник на обочине» → «Сталкер» (фильм и игры),
«Метро 2033», «Одни из нас» и др. Повторная загрузка безопасна (`ON CONFLICT DO NOTHING`).

**Производство, награды, факты** (`studios`, `awards`, `trivia`) — у `dune-2021`; награда — у
«Ведьмака 3».

**Медиа** — у «Дюны», «Ведьмака» и Средиземья: постеры (TMDB), обложки книг (Open Library), игра
(Steam) и трейлеры: сначала YouTube с канала правообладателя, затем Rutube — для стран, где YouTube
заблокирован. Официальных каналов на Rutube нет, там загрузки сторонних каналов: для прода их
нужно заменить официальными (российский дистрибьютор) — достаточно поменять id. Фронтенд
проверяет, какой источник доступен зрителю, и показывает первый доступный; нет ни одного —
только постер. Это ссылки на CDN, файлы не хранятся (своего
хранилища пока нет). Пишутся отдельным `UPDATE` по slug, поэтому повторный `make seed` дополняет и
уже загруженную базу. У фильмов без трейлера (`dune-1984`, `lotr-two-towers-2002`) — только постер;
у остальных сущностей нет и постера — фронтенд рисует заглушку. В проде TMDB требует указать
источник (атрибуцию) на сайте.

Тест `seed_catalog_is_valid` загружает дамп в чистую БД и проверяет, что вся `metadata` проходит
валидацию API и уже нормализована. Связи в дампе пишутся через подзапросы по slug, поэтому
опечатка в slug роняет загрузку, а не теряет строку молча.

## Импорт фильмов из TMDB

`scripts/catalog-import/import_tmdb.py` (Python: `aiohttp`, `asyncpg`, `requirements.txt` рядом) —
массовая загрузка фильмов прямо в PostgreSQL, в обход API. Запуск:
`TMDB_API_KEY=… DATABASE_URL=postgres://… python3 scripts/catalog-import/import_tmdb.py`, затем
`scripts/reindex-search.sh` (Meilisearch о новых строках не знает).

- **Extract:** ежедневный дамп `movie_ids_MM_DD_YYYY.json.gz` с `files.tmdb.org/p/exports` (нет
  сегодняшнего — до 3 дней назад, кэш `~/.cache/nexus-catalog-import/tmdb`), без `adult`/`video`, `popularity > 15`
  (`--min-popularity`), не больше 50 000 самых популярных (`--limit`). `--ids` — свой список.
- **Transform:** `GET /3/movie/{id}?append_to_response=credits&language=ru-RU`, 32 воркера
  (`--workers`), общий лимит 40 запросов/с (`--rate`; TMDB пускает ~50 с IP), повторы на 429
  (`Retry-After`), 5xx и сетевые ошибки. Ключ v3 — параметром `api_key`, токен v4 (`eyJ…`) — `Bearer`.
  `entities`: `kind = movie`, `title` (русское), `original_title`, `description` (overview),
  `release_date`, `cover_url` (`image.tmdb.org/t/p/w780…`); `metadata` — только `runtime_min`
  (1–10000), `countries` (alpha-2, верхний регистр), `studios` (до 20 по ≤120 символов), без
  дублей — как нормализует `metadata.rs`. Титры: режиссёры (`director`, позиции с 0), затем первые
  15 актёров по порядку TMDB (`actor`, `character_name`); `people.photo_url` — `…/h632…`. Жанры —
  теги `kind = genre`; id жанра TMDB сопоставлен slug сидовых тегов (`878 → sci-fi`,
  `9648 → detective`, …), существующие теги не меняются.
- **Slug:** латинский `original_title` (как в сидах: `dune-2021`), иначе транслит русского названия
  (`brat-1997`), плюс год. Людей — по имени (`timothee-chalamet`); имя без латиницы и кириллицы —
  `person-<tmdb_id>`.
- **Load:** один писатель, пачка 500 фильмов (`--batch-size`) — одна транзакция, вставка через
  `unnest(массивы)` (нет лимита в 65 535 параметров) с `ON CONFLICT DO NOTHING` во всех таблицах;
  id — `DEFAULT uuidv7()` базы.
- **Перезапуск** безопасен: служебная схема `tmdb_import` (создаёт скрипт, приложение и миграции её
  не используют; удалить — `DROP SCHEMA tmdb_import CASCADE`): `movies` (`tmdb_id` → `entity_id`,
  `created`, `details`), `people` (`tmdb_id` → `person_id`), `skipped` (404 и фильмы без названия). Загруженные
  и пропущенные id не запрашиваются повторно; оборванная пачка не закоммичена и загрузится заново.
  FK с `ON DELETE CASCADE`: удалённая в админке сущность при следующем запуске вернётся.
- **Существующие данные не перезаписываются.** Свободный slug — фильм создаётся. Занятый фильмом,
  ещё не привязанным к TMDB (сиды, админка), — это тот же фильм, он привязывается (`created = false`)
  без изменений, его титры и жанры не трогаются. Занятый другим фильмом TMDB или сущностью другого
  типа — slug `<slug>-<tmdb_id>` (`dune-2021-1001`). Люди — так же: тёзка получает `john-smith-502`.
- **Без ключа API** (`--export-only`, нужен только `DATABASE_URL`): все фильмы дампа, кроме
  `adult`, по убыванию popularity — только `title` = `original_title` и `original_title`; ни года,
  ни описания, постера, людей и жанров (их отдаёт лишь API с ключом). Slug — из оригинального
  названия без года (`hamlet`, следующие — `hamlet-10264`), сиды с этим не совпадают: в базе будут
  и `dune-2021`, и `dune`. Строки помечены `tmdb_import.movies.details = false`; убрать —
  `DELETE FROM entities WHERE id IN (SELECT entity_id FROM tmdb_import.movies WHERE created AND
  NOT details)`. Обычный запуск с ключом такие фильмы считает загруженными и не дополняет.
- Не загружаются: `birth_date`/`bio` людей (нужен отдельный запрос на человека), трейлеры,
  возрастной рейтинг. В проде TMDB требует атрибуцию на сайте.

## Импорт фильмов из Wikidata (без ключей)

`scripts/catalog-import/import_wikidata.py` (те же `aiohttp`, `asyncpg`; общие функции slug — из
`import_tmdb.py`) — 100 000 фильмов с людьми, жанрами и студиями. Запуск:
`DATABASE_URL=postgres://… python3 scripts/catalog-import/import_wikidata.py`, затем
`scripts/reindex-search.sh`. Этапы (`--stage download|wiki|load`, по умолчанию все), кэш
`~/.cache/nexus-catalog-import/wikidata` (~230 МБ; не в репозитории — `cargo-watch` перезапускал бы
приложение на каждую запись, обрывая перестройку поиска):

- **download** — 6 SPARQL-запросов к QLever (`qlever.cs.uni-freiburg.de/api/wikidata`, полная
  Wikidata, сотни тысяч строк за секунды; официальный `query.wikidata.org` режет запросы на 60 с и
  на 100 фильмов с актёрами не укладывается). Фильмы — `P31` ∈ {Q11424 фильм, Q24869
  полнометражный, Q202866 анимационный, Q29168811 полнометражный мультфильм}, топ `--limit`
  (100 000) по числу статей в Википедиях (sitelinks; при 100 000 — от 3 статей). Ответы — CSV:
  `films`, `basics`, `credits`, `people`, `characters`, `facets`; всё — около 35 с.
  У QLever sitelinks — `xsd:int`, сравнивается только через `xsd:integer(STR(?l))`.
- **wiki** — MediaWiki API: ru.wikipedia — вступление статьи (`description`) и картинка статьи
  (постер, `pilicense=any`), без постера — из en.wikipedia. 20/50 заголовков на запрос. Нужен контакт
  в User-Agent: `WIKIMEDIA_CONTACT=<почта или URL>` — без него Википедия после пары запросов отвечает
  429 с `Retry-After: 100`. С контактом — `--wiki-concurrency 4 --wiki-rate 4` (по умолчанию 1 и
  1/с; ответ на 20 статей идёт ~6 с, так что последовательно — ~2 ч). Около 3 300 запросов, кэш
  `wiki.jsonl` докачивается после остановки.
- **load** — пачка 500 фильмов = одна транзакция, `unnest` + `ON CONFLICT DO NOTHING`, id —
  `uuidv7()`. Описания и постеры, докачанные позже, вписываются в пустые поля уже созданных
  импортом фильмов при следующем `--stage load`.

Поля: `title` — подпись ru (иначе en / P1476), `original_title` — P1476 (иначе en),
`release_date` — самая ранняя P577 с точностью до дня (год для slug — с любой точностью),
`metadata.runtime_min` — P2047 в минутах (часы и секунды пересчитываются), `countries` — P495 →
P297, `studios` — P272 (до 20 по ≤120 символов). Жанры — P136 и P31 по таблице `GENRES`
(`Q130232 драматический фильм → drama`, `Q1200678 фильм-тайна → detective`, `Q202866 → animation`, …,
26 тегов `kind = genre`, новые: comedy, action, crime, romance, documentary, musical, biography,
war, western, history, family, sport). Титры: режиссёры P57, затем 15 актёров P161 и актёров
озвучивания P725 — по порядку P1545, без него самые известные первыми (sitelinks); персонаж —
подпись P453 (ru, иначе en) или P4633. Люди: `full_name` — ru, иначе en/`mul` (с 2024 имена
людей в Wikidata часто только в `mul`), `birth_date` — P569 с точностью до дня, `photo_url` —
P18 через `commons.wikimedia.org/wiki/Special:FilePath/…?width=400`, `bio` — краткое описание ru
(«Американский режиссёр.»).

Slug: латинский оригинал, иначе английское название (как в сидах: `stalker-1979`), иначе транслит
русского, плюс год (`inception-2010`); занят — `<slug>-q<qid>`. Люди — по имени
(`christopher-nolan`), известные первыми. **Существующие данные не перезаписываются:** готовый
непривязанный фильм с тем же slug (с годом или без — `interstellar`) или с тем же названием и годом
(`lotr-fellowship-2001`) привязывается без изменений, его титры и жанры не трогаются; люди — по
slug. Служебная схема `wikidata_import` (скрипт создаёт сам, приложение не читает): `movies`
(`qid` → `entity_id`, `created`, `tmdb_id` — P4947, для будущего обогащения из TMDB), `people`
(`qid` → `person_id`). Убрать загруженное — `DELETE FROM entities WHERE id IN (SELECT entity_id
FROM wikidata_import.movies WHERE created)`, затем `DELETE FROM people WHERE id IN (SELECT
person_id FROM wikidata_import.people) AND NOT EXISTS (SELECT 1 FROM entity_credits c WHERE
c.person_id = people.id)` и `DROP SCHEMA wikidata_import CASCADE`.

**Покрытие (почему не все фильмы).** Каталог берёт 100 000 самых известных фильмов Wikidata — это
`--limit`, а не предел источника:

| Срез Wikidata (QLever, 2026-10-12) | Фильмов |
|---|---|
| `P31` ∈ наши 4 типа (фильм, полнометражный, анимационный, полнометражный мультфильм) | 354 685 |
| из них со статьёй хотя бы в 2 Википедиях | 204 456 |
| загружено (топ-100 000, от 3 статей) | 99 616 + 14 сидов |
| не берём: короткометражные `Q24862` / анимационные короткометражки `Q17517379` | 49 780 / 5 647 |
| не берём: телефильмы `Q506240` | 22 722 |

- Остальные ~250 тыс. фильмов наших типов — с 0–2 статьями: малоизвестные, часто без постера,
  описания и состава. Взять больше — `--limit 200000` (или `--limit 400000 --min-links 0`), всё
  остальное (кэш, перезапуск, привязка) работает так же; QLever отдаёт такие объёмы за секунды,
  дольше всего — Википедия.
- Сама Wikidata знает не все фильмы: у TMDB ~1,25 млн записей (вместе с любительскими видео),
  в Wikidata — около 355 тыс.; в неё попадают в основном фильмы со статьёй в Википедии.
- Типы: фильм с `P31` = «немое кино», «аниме-фильм» и т. п. без одного из наших 4 типов не
  выбирается (у большинства есть и `Q11424`). Короткометражки и телефильмы — осознанно не берём.
- Русское название (подпись `ru`) — у 36 421 из 99 616 загруженных, у остальных `title` —
  английское; описание (статья ru.wikipedia) — у ~31 тыс., постер — у ~83 тыс.
- Пропущено 370 фильмов без подписи ни `ru`, ни `en`.

Первый прогон (2026-10-11, dev): 99 616 фильмов (+14 сидов привязано, 370 без названия на ru/en
пропущено), 137 538 людей, 765 003 титра, 135 781 связь с жанрами, 0 конфликтов slug; download
35 с, load 2 мин 40 с; wiki (4 параллельных запроса) — ~40 мин, описания/постеры у 83 400 фильмов. Лицензии: данные Wikidata — CC0, тексты Википедии — CC BY-SA (нужна
атрибуция), фото Commons — свободные с атрибуцией, постеры из статей обычно несвободные
(добросовестное использование) — для прода нужно решение по правам.

</seed>

<code_map>

## Код

| Файл | Что внутри |
|---|---|
| `lib.rs` | Роутер |
| `models.rs` | `EntityKind`, DTO; `Page<T>` реэкспортируется из `shared::pagination` |
| `directory.rs` | `PgEntityDirectory`: справочник сущностей для других модулей (`shared::directory::EntityDirectory`) |
| `metadata.rs` | Схемы `metadata` по типам и их проверка |
| `validate.rs` | Проверка slug, строк, URL, ролей; UNIQUE → 409 |
| `entities.rs`, `people.rs`, `tags.rs` | Чтение |
| `search.rs` | Индекс `entities`, `update_index`, перестройка с версиями (в т.ч. по расписанию), `GET /search` |
| `publish.rs` | Публикация правки: `Touched`, поиск, ISR, поток `.../stream` |
| `jobs.rs` | Шаги и события потоков (`JobEvent`), `Reporter`, ответ `text/event-stream` |
| `admin/` | Админские эндпоинты (`relations.rs` — связанные произведения) |
| `media/` | `nexus media` (`media/pick.rs` — фильтр Rutube) |

Тесты — `app/tests/catalog.rs` (данные — `app/tests/fixtures/catalog.sql`). Тесты поиска ходят в
настоящий Meilisearch из `make up` (в CI — сервис в `ci.yml`), у каждого теста свой префикс
индексов. Остальные тесты идут с выключенным поиском. Живые проверки — `http/catalog.http`.

</code_map>
