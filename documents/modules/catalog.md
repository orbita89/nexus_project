# Модуль `catalog`

**Отвечает за:** контентное ядро — сущности (фильмы, сериалы, книги, игры), людей и их роли,
теги, поиск через Meilisearch.
**URL:** `/api/catalog` · **Код:** `modules/catalog/`

**Статус:** таблицы есть, эндпоинтов и индексации в Meilisearch нет.

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

**`metadata` по типам** — договорённость, проверяется кодом `catalog` 🕓:

| `kind` | Пример полей |
|---|---|
| `movie` | `runtime_min`, `countries`, `age_rating` |
| `series` | `seasons`, `episodes`, `status` (идёт / завершён) |
| `book` | `isbn`, `pages`, `publisher` |
| `game` | `platforms`, `developer`, `publisher` |

**Новый тип** (комиксы, искусство): `ALTER TYPE entity_kind ADD VALUE '...'` + описание его `metadata`.

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

## Поиск 🕓

Сущности и люди индексируются в Meilisearch: опечатки, ранжирование, фильтры по `kind` и тегам.
Источник правды — PostgreSQL. Meilisearch — производный индекс, его можно перестроить с нуля.

## Планируемые эндпоинты 🕓

| Метод | Путь | Что делает |
|---|---|---|
| GET | `/api/catalog/entities` | Список с фильтрами (`kind`, тег, год) и пагинацией |
| GET | `/api/catalog/entities/{slug}` | Карточка: сущность, теги, участники |
| GET | `/api/catalog/search?q=` | Поиск через Meilisearch |
| GET | `/api/catalog/people/{slug}` | Человек и его фильмография |
| GET | `/api/catalog/tags` | Список тегов |
| POST/PATCH/DELETE | `/api/catalog/...` | Управление контентом (для модераторов) |
