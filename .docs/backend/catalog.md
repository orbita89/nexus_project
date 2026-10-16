# Модуль `catalog` — LOD 2

**Summary (EN):** content core — entities (movies/series/books/games) with `metadata jsonb`, people & credits, tags, relations, rating summaries, Meilisearch search, admin publishing (search + ISR), zero-downtime reindex, media (posters/trailers).
**Отвечает за:** контентное ядро — сущности (фильмы, сериалы, книги, игры), людей и их роли,
теги, поиск через Meilisearch.
**URL:** `/api/v1/catalog` · **Код:** `modules/catalog/` · **Источник (LOD 3):** `documents/modules/catalog.md`

**Статус:** ✅ чтение (список с сортировками, карточки из PostgreSQL со связанными произведениями,
похожие, люди, теги и жанры), поиск через Meilisearch, админка с публикацией правок (поиск, ISR
фронтенда) и потоком шагов (SSE), перестройка поиска без простоя с версиями индексов, проверкой
размера и потоком прогресса, мини-дамп, каталог фильмов из TMDB + Wikidata со слиянием источников (`scripts/catalog-import`).
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
| `active` | `boolean` | NOT NULL, default `true` | Видна на сайте. Миграция `20261016100000_entity_active.sql`, см. ниже |
| `active_manual` | `boolean` | NOT NULL, default `false` | `active` переключали в админке (`PATCH`, `POST` с `active`) или это сид, привязанный импортом: «Пересобрать активные» не трогает. Миграция `20261017100000_entity_tmdb.sql` |
| `created_at` | `timestamptz` | NOT NULL | |
| `updated_at` | `timestamptz` | NOT NULL | Обновляется триггером |

**Индексы:** по `kind`; по `release_date` (новинки сверху); частичный
`(release_date DESC NULLS LAST, title, id) WHERE active` — список по умолчанию; GIN по `metadata`
(фильтры по полям типа); триграммный GIN по `title` (поиск по подстроке, `ILIKE '%дюн%'`).

**Горячий слой и архив.** В `entities` — только горячий слой. С 2026-10-14 это **все** фильмы
TMDB с русским названием (121 371) и 500 самых популярных иностранных — 121 871 строка; сидов,
сериалов, книг, игр и фильмов без TMDB на dev больше нет (удалены 2026-10-14, `make seed` вернёт
сиды). Остальные ~1,07 млн фильмов — «скелетом» в холодном архиве (Tantivy,
[«Холодный архив»](#холодный-архив-)); полные данные TMDB по горячему слою —
[«Полные данные TMDB»](#полные-данные-tmdb-). Текст ниже про «~1,2 млн» описывает каталог до архива.

**Активные и скрытые.** Каталог из импорта TMDB + Wikidata — ~1,2 млн фильмов, на сайте —
отобранная часть (~80 тыс.: топ по баллу «Рекомендуемые» и «Скоро премьера», см. «Сортировка и
активные»; невышедшие фильмы из TMDB скрыты и без флага). Скрытая
(`active = false`) хранится в БД, но на сайте её нет нигде: ни в `/entities`, ни в поиске (не
индексируется), ни в похожих, связанных и фильмографиях, не считается в `entities_count` тегов;
карточка `/entities/{slug}` — 404 для всех, кроме админа (админу — и скрытые связанные). Справочник
для других модулей по slug её не находит — `social` отвечает 404 на рецензии, оценки, отметки,
коллекции и темы по такому slug; по id (уже сохранённое) — находит. Причины: «тонкие» страницы без
русского названия и описания вредят SEO всего сайта, а поиск и статика на миллион страниц — это
гигабайты. Включить — `PATCH /admin/entities/{id}` с `active: true` (публикация: поиск добавит
документ, фронтенд по ISR соберёт страницу) или скриптом импорта. Скрыть — `active: false`: поиск
удалит документ, ISR получит 404 и удалит статику.

**`metadata` по типам** проверяет код `catalog` (`modules/catalog/src/metadata.rs`). Все поля
необязательны, поля чужого типа и неизвестные поля дают 400. Значения нормализуются: обрезаются
пробелы, пустые строки и списки отбрасываются, дубли в списках убираются.

| `kind` | Поле | Тип и проверка |
|---|---|---|
| `movie` | `runtime_min` | целое 1–10000, минуты |
| | `countries` | коды ISO 3166-1 alpha-2, приводятся к верхнему регистру: `["US", "CA"]` |
| | `age_rating` | строка до 16 символов: `PG-13`, `16+` |
| | `popularity`, `vote_count` | устарели (теперь `entity_stats`): принимаются и отбрасываются |
| | `trailers` | трейлер: до 5 источников по приоритету, `[{"provider": "youtube", "id": "n9xhJrPXop4"}, {"provider": "rutube", "id": "0ab1…"}]`. `provider` — `youtube` (id 11 символов) или `rutube` (32 hex). На входе можно дать и просто ссылку на видео (`youtube.com/watch?v=…`, `youtu.be/…`, `rutube.ru/video/…/`, `/embed/…`) — провайдер и id определятся сами; другие сайты — 400. Повторы убираются |
| `series` | `seasons` | целое 1–10000 |
| | `episodes` | целое 1–100000 |
| | `status` | `ongoing` (идёт), `ended` (завершён), `canceled` (закрыт) |
| | `countries` | как у `movie` |
| | `trailers` | как у `movie` |
| `book` | `isbn` | ISBN-10 или ISBN-13 с проверкой контрольной цифры, хранится без дефисов |
| | `pages` | целое 1–100000 |
| | `publisher` | строка до 200 символов |
| | `libgen` | источник LibGen (пишет `nexus books import`): `{"work": "мастер и маргарита\|булгаков михаил афанасьевич", "files": [{"md5": "<32 hex, нижний регистр>", "extension": "fb2", "id": 1}]}`. `work` 1–1000 символов (ключ произведения), `files` 1–200 (`MAX_LIBGEN_FILES`) без повторов md5, `extension` 1–20 символов, `id` — `ID` строки дампа. Сохранение `metadata` без `libgen` его удаляет — повторный импорт создаст книгу заново |
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
| `entities_count` | `integer` | NOT NULL, default 0 | Сколько **активных** сущностей с тегом (`entities_count` в API). Держат триггеры: на `entity_tags` (±1, если сущность активна) и на смену/удаление активной сущности. Миграция `20261016100000_entity_active.sql` |

Жанры — те же теги с `kind = genre`: фильтр `?tag=`, поиск и похожие работают одинаково, а фронтенд
показывает жанры в шапке карточки, остальные теги — над «О фильме». В карточке `tags` — все теги,
жанры первыми. Какие теги жанры, решает админка (`kind` в `/admin/tags`); жанры TMDB импорт
создаёт сразу с `kind = genre`.

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
| `original_name` | `text` | | Имя латиницей (английский перевод TMDB), если отличается от `full_name`: поиск людей по «brad pitt». Миграция `20261021100000_person_stats.sql` заполнила из `person_tmdb_data` (dev: 189 840 из 886 993; у остальных `full_name` уже латиницей или перевода нет); дальше — `tmdb_full.py --people` и админка |
| `birth_date` | `date` | | Дата рождения |
| `photo_url` | `text` | | Фото |
| `bio` | `text` | | Биография |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |

**Индексы:** триграммный GIN по `full_name` — поиск людей по подстроке; `(full_name, id)` — список
по алфавиту.

**Публичный человек** — с работой в активной сущности или вовсе без работ (заведён в админке):
`/people` и `/people/{slug}` показывают только таких (админу карточка открыта всегда),
фильмография — только активные сущности.

### `studios` ✅ — студии

Миграция `20261024100000_studios.sql`. Страница студии ([«Студии»](#студии-)). Фильмы студии —
по названию в `entities.metadata.studios` (GIN `entities_metadata_gin`), таблицы связей нет.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK, default `uuidv7()` | |
| `slug` | `text` | NOT NULL, UNIQUE | URL: `warner-bros-pictures` (транслит названия, `enrich::slugify`; занят — `-<tmdb_id>`, затем `-2`, `-3`…; пустой — `studio-<tmdb_id>`) |
| `name` | `text` | NOT NULL, UNIQUE | Как в `metadata.studios` (TMDB отдаёт по-английски: `Mosfilm`) |
| `tmdb_id` | `integer` | UNIQUE | Компания TMDB (из `production_companies` сырья); одна компания под двумя названиями — у первого |
| `logo_url` | `text` | | `https://image.tmdb.org/t/p/w300…` |
| `origin_country` | `text` | | ISO 3166-1 alpha-2 |
| `headquarters`, `homepage`, `description` | `text` | | Из `GET /3/company/{id}` при первом открытии страницы |
| `parent_tmdb_id` | `integer` | | Материнская компания (страница — если она тоже студия каталога) |
| `details_fetched_at` | `timestamptz` | | Когда спросили TMDB (и при 404); NULL — спросить при открытии |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |

Dev (2026-10-15): 53 125 названий студий в `metadata.studios`, 53 962 компании в сырье.

### `catalog_import.people` ✅ — связь людей с TMDB и Wikidata

Таблица импорта (`merge.py`, `STATE_SCHEMA`): `person_id` (PK, FK → `people`, CASCADE), `tmdb_id`
(UNIQUE), `qid` (UNIQUE), `created`, `written`. Миграция `20261023100000_catalog_import_people.sql`
создаёт её с тем же определением (`IF NOT EXISTS`): возврат фильма из архива находит по ней людей
из титров TMDB и записывает связь для новых.

### `person_stats` ✅ — известность людей

Миграция `20261021100000_person_stats.sql`. Для ранжирования поиска людей ([«Поиск людей»](#поиск-людей-)).

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `person_id` | `uuid` | PK, FK → `people`, CASCADE | |
| `fame` | `double precision` | NOT NULL, default 0 | Известность: Σ по работам в активных сущностях «вес роли × `entity_stats.tmdb_votes`» |
| `known_for` | `text` | | Основная роль: с наибольшим вкладом в `fame` (при равенстве — больше работ, затем по алфавиту); работ нет — NULL |
| `updated_at` | `timestamptz` | NOT NULL | триггер `set_updated_at` |

Индекс `(fame DESC, person_id)`. Вес роли: режиссёр и первые 3 актёра по титрам — 1, актёры 4–10 —
0.4, остальные актёры — 0.1, `writer` и `composer` — 0.3, прочие — 0.15; за фильм берётся
наибольший вес человека (режиссёр-сценарист считается один раз). Номер актёра — `row_number()` по
`position` среди `actor` сущности. Пишет `people_search::refresh_fame`: всех — в начале шага
`fill_people` перестройки (dev: 71 с при первом заполнении 886 993 строк; дальше переписываются только
изменившиеся), выбранных людей — при публикации правок. TMDB popularity человека не годится: это
«тренд недели» (наверху случайные люди, Брэд Питт — 49-й), а число оценок фильмов копится годами
(dev: Сэмюэл Л. Джексон, Роберт Дауни-младший, Том Хэнкс, Брэд Питт, Леонардо Ди Каприо — топ-5).
Ограничение: российские фильмы набирают мало оценок TMDB — русские актёры ниже зарубежных.

### `person_credit_counts` ✅ — счётчики работ людей

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `person_id` | `uuid` | PK, FK → `people`, CASCADE | Строки нет — работ нет |
| `credits` | `integer` | NOT NULL, default 0 | Всего строк в `entity_credits` |
| `active_credits` | `integer` | NOT NULL, default 0 | Из них в активных сущностях |

Миграция `20261016100000_entity_active.sql`. Держат триггеры: `entity_credits_count` (вставка,
удаление, смена `entity_id`/`person_id` титров) и `entities_active_counts` (смена `active`,
удаление активной сущности — её работы вычитаются до каскада). Отдельная узкая таблица, а не
колонки `people`: заполнить — один `INSERT … GROUP BY` (у 2,2 млн людей перезапись строк вместе с
триграммным индексом имён шла 15+ мин); проверка «есть ли активная работа» запросом по 7 млн титров
— 9 с на каждый список. Смена `active` — операторный триггер `entities_active_changed`
(`20261017100000_entity_tmdb.sql`, transition tables `old_rows`/`new_rows`): один пересчёт
счётчиков на весь `UPDATE`. Построчный (первая версия) при массовом переключении миллион раз
переписывал бы одни и те же строки тегов в одной транзакции, а обход через `ALTER TABLE … DISABLE
TRIGGER` держал бы `entities` эксклюзивно — сайт стоял бы минуты.

### `entity_tmdb` ✅ — связь с TMDB

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | |
| `tmdb_id` | `integer` | NOT NULL, UNIQUE | id фильма в TMDB |

Миграция `20261017100000_entity_tmdb.sql` (перенос связей из `catalog_import.movies`, если импорт
был). Пишет импорт (`merge.py`), читают операции админки «Обновить popularity» и «Пересобрать
активные» ([`catalog::tmdb`](#данные-tmdb-в-админке-)).


### `entity_stats` ✅ — статистика и балл

| Колонка | Тип | Описание |
|---|---|---|
| `entity_id` | `uuid` PK, FK → `entities`, CASCADE | |
| `imdb_id` | `text` (индекс) | `tt…`, из TMDB (`merge.py`) |
| `tmdb_popularity` | `double precision` | «смотрят сейчас», ежедневный дамп TMDB |
| `tmdb_votes` | `integer` | число оценок TMDB |
| `imdb_rating`, `imdb_votes` | `double precision`, `integer` | IMDb `title.ratings` |
| `quality` | `double precision` | байесовский рейтинг IMDb (сортировка `top_rated`) |
| `rank_score` | `double precision` (индекс `DESC NULLS LAST`) | балл «Рекомендуемые»; `NULL` — не вышел или мало голосов |
| `updated_at` | `timestamptz` | триггер |

Миграция `20261018100000_entity_stats.sql` (перенос `metadata.popularity`/`vote_count`). Узкая
таблица, а не `metadata`: обновление ~1 млн строк `metadata` перестраивает GIN-индексы (25 мин), а
пересчёт балла после правки формулы должен идти минуты.

### `catalog_ranking` ✅ — настройки сортировки

Одна строка (`id = true`): `weight_quality` (0.5), `weight_popularity` (0.3), `weight_recency` (0.2),
`trust_votes` (5000), `half_life_years` (4), `min_votes` (1000), `soon_days` (30), `updated_at`,
`updated_by` (id админа). Правит `PUT /admin/ranking/stream`, см. [«Сортировка и
активные»](#сортировка-и-активные-).

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

### `entity_tmdb_data`, `person_tmdb_data` ✅ — полный ответ TMDB

Миграция `20261020100000_tmdb_data.sql`. Сырьё для горячего слоя: всё, что отдаёт API TMDB по
фильму и человеку ([«Полные данные TMDB»](#полные-данные-tmdb-)). Приложение пока не читает.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `entity_id` / `person_id` | `uuid` | PK, FK → `entities` / `people`, CASCADE | |
| `tmdb_id` | `integer` | NOT NULL | Id в TMDB |
| `data` | `jsonb` | NOT NULL, сжатие `lz4` | Ответ `GET /movie/{id}` / `/person/{id}` с `append_to_response` |
| `fetched_at` | `timestamptz` | NOT NULL, default `now()` | |

Удалить сырьё — `TRUNCATE entity_tmdb_data, person_tmdb_data`: разложенные поля остаются.

### `archived_tmdb` ✅ — фильмы TMDB в холодном архиве

Миграция `20261019100000_archived_tmdb.sql`. Строки `entities` этих фильмов удалены (они в индексе
Tantivy, [«Холодный архив»](#холодный-архив-)); здесь только id, чтобы импорт их не вернул.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `tmdb_id` | `integer` | PK | Id фильма в TMDB |
| `archived_at` | `timestamptz` | NOT NULL, default `now()` | |
| `manual` | `boolean` | NOT NULL, default `false` | Перенесён вручную из админки (`POST /admin/archive/demote`): сборка горячего слоя его не возвращает. Миграция `20261022100000_archived_tmdb_manual.sql` |

Пишут `nexus archive build` (строка ушла в архив) и ручной перенос (`manual = true`, в том числе
поверх строки сборки); удаляют промоушен и сборка (фильм вернулся в горячий слой; `manual` сборка не
возвращает). `merge.py` пропускает фильмы с этими id (в логе — `archived`).

### Книги из LibGen — без своих таблиц ✅

Миграция `20261026100000_books_libgen.sql` — только индексы. Книга — строка `entities`
(`kind = 'book'`), источник — `metadata.libgen` (см. поля `metadata` выше), авторы — `people` +
`entity_credits` (`role = 'author'`). Пишет `nexus books import` ([«Книги из LibGen»](#книги-из-libgen-)).
Сами файлы книг не хранятся, ссылок на скачивание нет.

| Индекс | Определение | Зачем |
|---|---|---|
| `entities_libgen_md5_idx` | GIN `jsonb_path_query_array(metadata, '$.libgen.files[*].md5')` WHERE `kind = 'book'` | известные md5 пачки: `... ?| $md5s` |
| `entities_libgen_work_idx` | `(metadata #>> '{libgen,work}')` WHERE `kind = 'book' AND metadata ? 'libgen'` | другой файл того же произведения находит книгу |
| `people_name_key_idx` | `(replace(lower(full_name), 'ё', 'е'))` | автор из дампа находит человека, уже записанного автором (`entity_credits.role = 'author'`) |

</tables>

<deps>

## Зависимости: справочники и интеграции

| Что | Где | Назначение |
|---|---|---|
| `EntityDirectory` (реализует `catalog`: `PgEntityDirectory`) | `shared::directory`, `state.entities` | `social`/другие читают сущности без JOIN к `entities`. `by_slug`, `by_slugs` — только активные (адрес пришёл с сайта: скрытая → 404), `by_ids` — все (уже сохранённое: коллекции, ленты, интересы не теряют элементы) |
| `EntityRatings` (реализует `catalog`: `PgEntityRatings`) | `shared::directory`, `state.ratings` | `social` пишет сводки оценок для сортировок `popular`/`rating` |
| Поиск | `shared::search` (HTTP API Meilisearch через reqwest, без SDK) | индекс `entities`, `update_index`, перестройка |
| ISR-вебхук фронтенда | `shared::isr` (`POST {ISR_URL}/_isr/revalidate`) | пересборка статики после правок |
| Дампы TMDB и IMDb | `reqwest` + `flate2` (распаковка `.gz`): `TMDB_EXPORTS_URL` (`https://files.tmdb.org/p/exports`), `IMDB_DATASETS_URL` (`https://datasets.imdbws.com`), без ключей | «Обновить рейтинги» в админке |
| TMDB API (возврат из архива) | `reqwest`; `TMDB_API_KEY` (v3 — параметром, v4 JWT — заголовком), `TMDB_API_URL` (по умолчанию `https://api.themoviedb.org/3`, в тестах — подставной сервер), `ARCHIVE_RESTORE_PER_HOUR` (300; `governor`); `unicode-normalization` — slug людей | `archive::enrich`: полные данные фильма при `restore` и `promote` |
| Холодный архив | `tantivy` 0.26 (`mmap`, `lz4-compression`, без zstd) + `csv`; каталог `ARCHIVE_DIR` (`Config::archive_dir`, dev/prod — том `archivedata` в `/var/lib/nexus/archive`; не задан — эндпоинты архива 503) | `catalog::archive`: индекс, поиск, страница фильма, промоушен, `nexus archive dump\|build` |
| `InterestDirectory::entities_in_use` (реализует `social`) | `shared::directory`, `state.interests` | какие сущности нужны пользователям (интересы, рецензии, коллекции, отметки, реакции, темы форума) — их сборка архива не удаляет |
| Дамп LibGen | локальный файл (`flate2` для `.gz`); обложки — `https://libgen.is/covers/` (`books::COVERS`) | `nexus books import` |
| Блокировки перестройки | Redis (`shared::cache`): `nexus:v1:catalog:reindex-running`, `nexus:v1:catalog:reindex-lock` | «одна перестройка за раз» между инстансами |

</deps>

<logic>

## Поиск

Активные сущности индексируются в Meilisearch (индекс `entities`; скрытые — нет: правка `active`
добавляет или удаляет документ, перестройка берёт только `active`): опечатки, ранжирование, фильтры
по `kind`, тегу, году (точно или диапазоном), студии; сортировки — см. ниже. Ищется по названию,
оригинальному названию и студиям, в таком порядке важности. Клиент — тонкая обёртка над HTTP API в
`shared::search`, без SDK.

**Весь поиск на сайте — только через Meilisearch** (решение пользователя 2026-10-14): `GET /search`
и подсказки `GET /search/studios`. Публичный `GET /entities` поиска по названию не делает (`q` →
400 `use_search`); подстрока по БД (`ILIKE`) осталась только в админке `GET /admin/entities?q=`
(скрытых сущностей нет в индексе) и в `GET /people?q=` (список по алфавиту). Люди ищутся через
`GET /search/people` ([«Поиск людей»](#поиск-людей-)).

**Фильтры `GET /search`** (все через AND, `filters_of` в `search.rs`):

| Параметр | Фильтр Meilisearch | Проверка |
|---|---|---|
| `kind` | `kind = movie` | enum |
| `tag` | `tags = "<slug>"` | slug, иначе 400 |
| `year` | `year = 2001` | 1–9998; вместе с `year_from`/`year_to` — 400 |
| `year_from`, `year_to` | `year 2000 TO 2010` (включительно), одна граница — `year >= …` / `year <= …` | 1–9998; `from > to` — 400 |
| `studio` | `studios = "<название>"` — точное, без учёта регистра (так сравнивает Meilisearch), любая из студий | 1–200 символов после trim; `\` и `"` экранируются |
| `studio_slug` | `studio_slugs = "<slug>"` — студия по slug страницы `/studios/{slug}` (2026-10-15); неизвестный — пусто | slug, иначе 400 |
| `person` | `people = "<slug>"` — человек в титрах в любой роли (2026-10-15) | slug, иначе 400 |
| `person` + `role` | `credits = "<role>:<slug>"` — человек именно в этой роли | `person` — slug, `role` — `a-z`/`_`; `role` без `person` — 400 |

Сортировки поиска: без `sort` — релевантность; `recommended` / `trending` / `top_rated` —
`rank_score` / `popularity` / `quality` по убыванию; `new` — `release_ts:desc` + фильтр
`release_ts <= сегодня` (только вышедшие); `soon` — `release_ts:asc` + `release_ts > сегодня`;
`title` — `title:asc`. Без значения поля — в конце.

**Подсказки студий** `GET /search/studios?q=&kind=&limit=` — facet search Meilisearch
(`POST /indexes/entities/facet-search`, `facetName: studios`): по части названия с опечатками, без
`q` — самые частые (`faceting.sortFacetValuesBy.studios = count`). Ответ `[{name, slug, count}]`,
`name` — значение для `studio`, `slug` — страница студии и значение для `studio_slug` (из `studios`
одним запросом на ответ, `studios::refs`; `null` — строки студии ещё нет, 2026-10-15).

Источник правды — PostgreSQL, индекс производный:

- **После записи в админке** затронутые сущности сразу переотправляются в индекс: сама сущность,
  все сущности с изменённым тегом, все работы переименованного человека. Meilisearch применяет
  изменения асинхронно, обычно за доли секунды. Если он недоступен, запись в БД всё равно проходит,
  а в лог пишется warning.
- **Полная перестройка** без простоя, с версиями индексов: при старте приложения (в фоне), по
  расписанию и из админки — [«Перестройка поиска»](#перестройка-поиска-).
- Данные, загруженные в БД в обход API (`make seed`), попадают в индекс только после перестройки:
  `make seed` запускает её сам (`scripts/reindex-search.sh`).

**Точный `total`** (2026-10-15, `search_exact` в `search.rs`, им же пользуется `GET /search/people`):
`total` в `GET /search` и `GET /search/people` — точное число совпадений (`totalHits`), а не оценка
`estimatedTotalHits`. Точное число Meilisearch считает только в режиме `page` + `hitsPerPage`:
`offset`, кратный `limit` (фронтенд шлёт только такие), — один запрос
`page = offset / limit + 1`, `hitsPerPage = limit`; иначе выдача по `offset`/`limit` и параллельно
второй запрос счёта (`page 1`, `hitsPerPage 0`, без сортировки). Потолок счёта и листания —
`pagination.maxTotalHits = 500000` (`MAX_TOTAL_HITS`) у индексов `entities` и `people` (по умолчанию
Meilisearch считал и листал только 1000: `total` был 1000, `offset ≥ 1000` — пусто). Настройка
ставится при перестройке: после выкладки старый индекс отдаёт не больше 1000, пока не пройдёт
перестройка. Время ответа на dev (119 773 фильма) ещё не замерено.

**Оценки Nexus в индексе** (2026-10-15, `spawn_ratings_sync` / `sync_ratings` в `search.rs`).
Оценки — в `social`; сводки он присылает через `EntityRatings` в `entity_ratings` каталога (после
каждой рецензии и `replace_all` при старте). В индекс они попадают пачкой: раз в 60 с
(`RATINGS_SYNC_INTERVAL`) фоновая задача берёт `entity_ratings` с `updated_at` позже прошлого
прохода (нахлёст окна 60 с; первый проход — все сводки) и переотправляет документы этих сущностей
целиком из БД (`update_entities`: и в теневой индекс, скрытые удаляет). Выбрано вместо обновления
по событию: `social` не знает о поиске, трейт и события не меняются; задержка — до ~1 мин.
`replace_all` теперь не удаляет пропавшие сводки, а обнуляет (`count = 0`, `average = NULL`, новое
`updated_at`), чтобы обнуление дошло до индекса; для сортировок `GET /entities` это то же самое.
Повторная отправка безопасна; на нескольких инстансах каждый шлёт сам.

Если Meilisearch недоступен, `GET /search` отвечает `503`. Если индекс ещё не построен, ответ —
пустая страница. Карточки, списки, теги и справочники для `social` от Meilisearch не зависят; «Похожие»
(`/entities/{slug}/similar`) — зависят (503).

### Что лежит в индексе

Документ индекса `entities` — компактный (2026-10-14, `SearchDoc` в `search.rs`): `id`, `kind`,
`slug`, `title`, `original_title`, `release_date`, `cover_url`, `year`, `release_ts`
(`release_date` в unix-секундах полуночи UTC: сортировки `new`/`soon`, Meilisearch сравнивает только
числа), `studios` (все из `metadata.studios`), `tags` (slug'и тегов, фильтр), `rank_score`,
`popularity`, `quality` (сортировки), `imdb_rating`, `imdb_votes`, `release_status` (выдача),
`nexus_ratings_count`, `nexus_rating_avg` (оценки Nexus из `entity_ratings`; без оценок — `0` и
`null`; сортировки `popular`/`rating`, 2026-10-15), `people`, `credits` (участники, фильтр
`person`/`role`, 2026-10-15), `studio_slugs` (slug'и строк `studios` по названиям
`metadata.studios`, одним запросом на пачку; фильтр `studio_slug`, 2026-10-15; slug студии не
меняется, а `studios::sync` идёт до документов — в перестройке перед `fill`, в `publish::run` перед
индексом).
Настройки (`entity_settings`): `searchableAttributes` — `title`, `original_title`, `studios`;
`filterableAttributes` — `kind`, `tags`, `year`, `slug`, `studios`, `release_ts`;
`sortableAttributes` — `rank_score`, `popularity`, `quality`, `release_ts`, `title`,
`nexus_ratings_count`, `nexus_rating_avg`; в `filterableAttributes` также `people`, `credits`, `studio_slugs`;
`faceting.sortFacetValuesBy` — `studios: count`; `pagination.maxTotalHits` — 500000. Настройки ставятся на теневой индекс при
перестройке: после выкладки фильтры `studio`/диапазон/`new` работают, когда пройдёт перестройка
(при старте — в фоне). Карточки (`card`), описания,
имён участников и названий тегов в индексе больше нет: поиск по имени актёра или названию тега не
работает (по решению пользователя), а перестройка при 2,75 млн титров шла ~45 мин вместо минут.
Документы пачки строятся из уже прочитанных строк и запросов на пачку (теги, оценки Nexus,
участники), без карточек.

**Участники в документе** (2026-10-15, фильтр `person`/`role`): `people` — slug'и всех людей из
`entity_credits` сущности, `credits` — пары `"роль:slug"` (`"director:denis-villeneuve"`), без
дублей. Оба только `filterableAttributes`: поиск по имени в `q` по-прежнему не работает (имён в
документе нет), только фильтр по slug. Строятся одним запросом `entity_credits JOIN people` на
пачку (обе таблицы каталога). Правки человека (slug) и титров переотправляют затронутые сущности
через `Touched` (работы человека, состав сущности). Объём на dev (~2,75 млн титров) и время
перестройки после выкладки ещё не замерены.

### Поиск людей ✅

Индекс `people` (2026-10-14, `people_search.rs`) — публичные люди (с работой в активной сущности
или без работ). Документ `PersonDoc`: `id`, `slug`, `full_name`, `original_name`, `birth_date`,
`photo_url`, `known_for`, `fame` (из `person_stats`). Настройки: `searchableAttributes` —
`full_name`, `original_name`; `filterableAttributes` — `known_for`; `sortableAttributes` — `fame`,
`full_name`; `rankingRules` — `words, typo, proximity, attribute, sort, exactness, fame:desc`; `pagination.maxTotalHits` — 500000:
известность решает среди одинаково подходящих совпадений («Брэд» → Брэд Питт, затем Брэдли Купер,
Брэд Бёрд …), точное совпадение имени важнее славы; без `q` — по известности.

`GET /search/people?q=&known_for=&limit=&offset=` → `Page<PersonHit>` (`id`, `slug`, `full_name`,
`original_name`, `birth_date`, `photo_url`, `known_for`; `fame` не отдаётся). `known_for` —
`a-z`/`_` (иначе 400), фильтр `known_for = <role>`.

Индекс строится и переключается вместе с `entities` ([«Перестройка поиска»](#перестройка-поиска-),
шаг `fill_people`, пачки по 2000). Публикация правок (`publish::run` → `search::update_index`)
пересчитывает `fame` затронутых людей (`Touched.people`: изменённый человек и состав изменённых
сущностей) и переотправляет их документы, скрытых удаляет. Фильтр `person` в `GET /search` —
по участникам в документе `entities` (см. «Участники в документе»).

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

## Студии ✅

Решение пользователя (2026-10-15): клик по студии в карточке фильма открывает страницу студии —
краткая информация и список её фильмов. Код — `studios.rs`.

- **Какие у фильма студии** — по-прежнему `entities.metadata.studios` (TMDB, админка, поиск
  `studio` в Meilisearch). Карточка (`EntityDetail.studios`) отдаёт их в том же порядке как
  `[{slug, name, logo_url}]` — ссылки на `/studios/{slug}`; названия без строки в `studios`
  пропускаются.
- **Синхронизация** `studios::sync(db, None | Some(ids))`: новые названия из `metadata.studios`
  (`NOT EXISTS` по `name`) → TMDB id, логотип и страна из `entity_tmdb_data.production_companies`
  (`metadata -> 'studios' ?| names`, только фильмы с этими названиями) → slug → вставка пачками по
  5000 (`ON CONFLICT DO NOTHING`). Полная — в начале шага `fill` перестройки поиска (первая на dev
  — 53 тыс. студий, ~1 мин из-за сырья; дальше — только новые названия), точечная — в
  `publish::run` для затронутых сущностей и при возврате фильма из архива (до карточки).
  Студии без фильмов не удаляются.
- **Страница** `GET /studios/{slug}` → `StudioDetail`: `slug`, `name`, `logo_url`,
  `origin_country`, `headquarters`, `homepage`, `description`, `parent` (`StudioRef` материнской,
  если она есть в каталоге), `entities_count` (видимые вышедшие, как список). Нет
  `details_fetched_at` и есть `tmdb_id` — `GET {TMDB_API_URL}/company/{id}` (`enrich::tmdb_get`,
  таймаут 8 с): поля сохраняются (пустые не затирают старые), 404 — запоминается; нет ключа или TMDB
  не ответил — страница из того, что есть, попытка при следующем открытии.
- **Фильмы студии в поиске** — `GET /search?studio_slug=<slug>` (один запрос со страницы студии,
  без запроса названия), подсказки `GET /search/studios` отдают `slug` (2026-10-15).
- **Фильмы студии** — `GET /entities?studio=<slug>` (и `/admin/entities?studio=`): фильтр
  `e.metadata @> {"studios": [name]}` вдобавок к остальным, все сортировки и пагинация; неизвестный
  slug — пустая страница.
- **ISR**: пути затронутых сущностей включают `/studios/{slug}` их студий (`publish::studio_path`).

Тест — `app/tests/studios.rs` (`studio_pages_and_their_films`: синхронизация, ссылки в карточке,
фильтр и сортировка, страница без ключа и с подставным TMDB — один запрос на студию, 404, новая
студия с кириллицей после правки в админке).

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
   `KINDS` фронтенда), людей (`/people/{slug}`) и студий этих сущностей (`/studios/{slug}`). Перед
   шагами — `studios::sync` затронутых сущностей (новые студии из `metadata`).

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
никогда. Поиск всегда читает индексы `entities` и `people` — они играют роль алиасов (в Meilisearch
1.15 алиасов нет), а версии подменяют их атомарным `swap-indexes`. Оба индекса перестраиваются одной
перестройкой с общей меткой версии:

1. **create_index** — теневые `entities_v<unix ms>` и `people_v<unix ms>` с настройками; `entities`
   и `people` создаются, если их нет.
2. **fill** — все сущности из PostgreSQL пачками по 500 (keyset по `id`, карточки пачки — двумя
   запросами) в теневой индекс. Пока Meilisearch применяет пачку, читается следующая. Событие
   `progress` после каждой пачки: `{"done": 1000, "total": 100000}`.
2б. **fill_people** — `refresh_fame` всех людей, затем все публичные люди пачками по 2000 в
   `people_v<ms>` (`progress` со `step: fill_people`). Dev: 886 993 человека.
3. **check** — **каждый** новый индекс должен быть **не меньше `SEARCH_REINDEX_MIN_PERCENT`%** текущего
   (`numberOfDocuments`; по умолчанию и в проде — 80, 0–100, иначе приложение не стартует). Меньше —
   значит, выборка сломана или неполная: поиск не переключается, теневые индексы удаляются, ответ `409`
   (в потоке — `check: failed`). Если сущности удалены намеренно — `?force=true`. Текущий пуст —
   проверка проходит. **В dev** (`infra/docker-compose.yml`) — `0`, проверки нет: каталог часто
   перезаливается целиком (импорт из TMDB/Wikidata, откат), и перестройка при старте не должна
   упираться в старый большой индекс.
4. **swap** — `entities ⇄ entities_v<ms>` и `people ⇄ people_v<ms>` одной задачей: поиск на всех
   инстансах сразу видит новые документы, а в `*_v<ms>` остаётся предыдущая версия («на всякий
   случай»; откат — swap обратно).
5. **rotate** — хранятся строго две версии каждого: текущая и предыдущая (`*_v<ms>` только что). Все
   `entities_v*` и `people_v*` старше удаляются: «Удалены устаревшие индексы: …».
6. **isr** — `POST {ISR_URL}/_isr/revalidate` с `{"all": true}`: фронтенд отвечает `202` и
   пересобирает всю статику в фоне, поэтому шаг значит «запущено», а не «готово» (ход — в логе фронтенда). Только при
   ручном запуске; при старте и по расписанию — нет.

- **Одна за раз.** В процессе — набор идущих перестроек, между инстансами — блокировка
  `nexus:v1:catalog:reindex-running` в Redis (TTL 2 мин, работающая перестройка продлевает его каждые 30 с: убитый процесс — перезапуск, деплой — не держит блокировку дольше 2 мин; снимается в конце). Занято —
  `409` до потока. Без Redis перестраивает каждый (warning).
- **Правки во время перестройки** пишутся и в живой, и в теневой индекс, поэтому после swap не
  теряются. Ограничение: только правки на том же инстансе, где идёт перестройка.
- **По расписанию** (`SEARCH_REINDEX_INTERVAL_SECS=86400`, `0` — только при старте и вручную)
  перестраивает один инстанс: блокировка `nexus:v1:catalog:reindex-lock` на интервал.
- **Ошибка шага** — `failed` с текстом, следующие шаги не выполняются, недостроенный теневой индекс
  удаляется (не удалось — его уберёт следующая ротация).

**Поток для админки:** `POST /admin/search/reindex/stream[?force=true]` → `text/event-stream`:

```
event: plan      data: {"type":"plan","steps":[{"step":"create_index",...},{"step":"fill",...},{"step":"fill_people",...},{"step":"check",...},{"step":"swap",...},{"step":"rotate",...},{"step":"isr",...}]}
event: step      data: {"type":"step","step":"create_index","status":"done","message":"Созданы теневые индексы entities_v1759912345123, people_v1759912345123","duration_ms":35}
event: progress  data: {"type":"progress","step":"fill","done":500,"total":100000}
event: progress  data: {"type":"progress","step":"fill_people","done":2000,"total":880000}
event: step      data: {"type":"step","step":"check","status":"done","message":"Новые индексы — entities: 100000 документов, текущий: 99870 (100%); people: 880000 документов, текущий: 879000 (100%)"}
event: step      data: {"type":"step","step":"rotate","status":"done","message":"Удалены устаревшие индексы: entities_v1759800000000, people_v1759800000000"}
event: done      data: {"type":"done","ok":true,"reindex":{"indexed":100000,"people_indexed":880000,"previous_count":99870,"previous_version":"entities_v1759912345123","deleted":["entities_v1759800000000","people_v1759800000000"]}}
```

Без потока — `POST /admin/search/reindex[?force=true]`, ответ — тот же `reindex` (`ReindexResult`:
`indexed`, `people_indexed`, `previous_count` и `previous_version` — индекса `entities`, `deleted` —
устаревшие версии обоих).

**Тесты** — `app/tests/catalog.rs`: хранятся ровно две версии, отказ переключаться при 20% и
`force=true`, ровно 80% — переключение, `SEARCH_REINDEX_MIN_PERCENT=0` — переключение без `force`
(тесты задают 80 явно, независимо от окружения), поток (шаги, `progress`, ротация, `{"all": true}` во
фронтенд), отказ в потоке (`check: failed`, поиск прежний), `409` при идущей перестройке; поиск
людей — `people_search_ranks_by_fame` (`fame`/`known_for` в `person_stats`, ранжирование тёзок,
пустой запрос, латиница с опечаткой, фильтр `known_for`, правка `original_name` в админке).

## Полные данные TMDB ✅

`scripts/catalog-import/tmdb_full.py` (2026-10-14; `aiohttp`, `asyncpg`; ключ `TMDB_API_KEY` из
окружения или `infra/.env`) — всё из TMDB для горячего слоя (строки с `entity_tmdb`; архив не
трогается). TMDB — источник правды горячего слоя: поля перезаписываются.

- **Фильм:** `GET /3/movie/{id}?language=ru-RU&append_to_response=credits,release_dates,keywords,
  translations,alternative_titles,images,videos,external_ids,watch/providers,recommendations,
  similar,reviews,lists&include_image_language=ru,en,null&include_video_language=ru,en,null` —
  ответ целиком в `entity_tmdb_data` (популярный фильм ~400 КБ JSON, обычный ~50–120 КБ; lz4
  ~2,4×). Раскладка: `title` (русское; латиницей из TMDB кириллицу не заменяем), `original_title`,
  `description` (overview ru, пусто — прежнее), `release_date`, `cover_url` (`w780`);
  `metadata`: `runtime_min`, `countries`, `studios`, `age_rating` (RU, иначе US; `NR`/`Unrated`
  не пишутся), `trailers` (YouTube `Trailer`: официальные, русские, затем английские; до 5);
  `entity_stats`: `tmdb_popularity`, `tmdb_votes`, `imdb_id`; жанры → теги `genre` (как
  `import_tmdb.py`, прочие теги не трогаются); `entity_credits` заменяются целиком: режиссёры
  (`director`), сценаристы (`writer`: Screenplay, Writer, Story, Novel, Author), продюсеры
  (`producer`), композиторы (`composer`), операторы (`cinematographer`), монтажёры (`editor`), затем
  **весь** актёрский состав по порядку TMDB (`actor`, `character_name`). Остальные должности — только
  в сырье. Люди — по TMDB id (`catalog_import.people`), новые: `slug` из имени (транслит), занят —
  `<slug>-<tmdb_id>`, фото `h632`.
- **Коллекции → сиквелы:** в конце соседние по дате выхода фильмы одной коллекции TMDB
  (`belongs_to_collection`) связываются `sequel`, если связи (в любую сторону) ещё нет.
- **`--people`** — люди из титров горячего слоя: `GET /3/person/{id}?language=ru-RU&
  append_to_response=external_ids,translations,images` → `person_tmdb_data`; `bio` (русская
  биография, иначе прежняя), `birth_date`, `photo_url`, `original_name` (английский перевод, если
  отличается от `full_name`). Фильмография (`combined_credits`, ~200 КБ)
  не берётся — дублирует титры.
- Только в сырье (в API пока нет): ключевые слова, переводы и альтернативные названия, картинки
  (постеры, фоны, логотипы), все видео, внешние id, где смотреть, рекомендации и похожие TMDB,
  рецензии TMDB, списки, `tagline`, `budget`, `revenue`, `vote_average`, `homepage`, полная
  съёмочная группа.
- Пачка 100 фильмов — одна транзакция; 32–96 параллельных запросов, общий лимит `--rate` (40/с);
  перезапуск продолжает (скачанные пропускаются, `--refresh` — заново). Затем перестройка поиска.

**Первый прогон (dev, 2026-10-14).** Фильмы: 121 843 из 121 871 (28 — 404, удалены из TMDB),
~1 ч 15 мин (12–32 фильма/с; без построчных триггеров — вдвое быстрее). Описание — у 98 760,
постер — 112 875, длительность — 117 256, трейлеры — 51 500, возрастной рейтинг — 30 439. Титров
2 750 312 (актёры 2 021 347, сценаристы 180 623, продюсеры 151 169, режиссёры 133 026,
композиторы 89 688, операторы 88 332, монтажёры 86 127), людей 886 993, сиквелов из коллекций
+6 862. Сырьё `entity_tmdb_data` — 3,4 ГБ (lz4), база — 5,3 ГБ. Meilisearch — 119 884 документа
(перестройка ~45 мин с полной карточкой в документе; после ужатия документа — см. «Что лежит в индексе»). Люди (`--people`,
887 тыс. запросов, ~6,5 ч при 40/с) — в процессе.

## Холодный архив ✅

Двухуровневое хранение (2026-10-13, `modules/catalog/src/archive/`):

- **Горячий слой** — `entities` в PostgreSQL (полные карточки, сайт, Meilisearch): по умолчанию
  **все** фильмы с русским названием (`has_ru_locale`: в `title` есть кириллица; `DEFAULT_RU =
  usize::MAX`) и 500 самых популярных по TMDB popularity без него (`DEFAULT_FOREIGN`). До
  2026-10-14 было 90 000 + 10 000 (`--ru 90000 --foreign 10000`). Плюс то, что сборка не трогает:
  `active_manual` (промоушен, ручные), сущности без TMDB, сущности с пользовательскими данными
  (`InterestDirectory::entities_in_use`).
- **Холодный архив** — индекс Tantivy в `ARCHIVE_DIR`, через mmap (индекс не в куче процесса).
  Документ: `id` (slug; `STRING | STORED` — точный поиск и `delete_term`), `title` (`TEXT | STORED`),
  `orig_title` (`TEXT | STORED` — в ТЗ только индекс, но без него промоушен потерял бы оригинальное
  название), `type` (`STRING | STORED`), `tmdb_id` (u64), `release_date` (`YYYY-MM-DD`, хранится),
  `popularity` (f64, `FAST`), `cover_url` (URL постера TMDB `w780`, как `entities.cover_url`; только
  `STORED`; с 2026-10-14). Схема только растёт: новое поле — последнее, номера прежних не меняются,
  поэтому индекс старой схемы открывается как есть (`Index::open`; `open_or_create` требует точного
  совпадения) — постеров в нём нет и при записи они не сохраняются (`Archive::is_current`); новую
  схему даёт `build --fresh`. Токенизатор `default` (по словам, нижний регистр, без стемминга;
  «ё» ≠ «е»). Ранжирование: BM25 × `1 + ln(1+popularity)/10`.
- **Писатель** — только на время операции, бюджет ровно `WRITER_BUDGET` = 50 МБ
  (`index.writer(50_000_000)`; Tantivy делит его на ≤3 потока по ≥15 МБ). Один писатель на каталог:
  второй получает `LockBusy` → промоушен во время сборки — 409. Читатель (`OnCommitWithDelay`)
  подхватывает коммиты другого процесса сам.

**Конвейер** (`archive/pipeline.rs`, CLI вместо сервера; в dev:
`docker exec nexus-app cargo run -q -p nexus -- archive ...`, прод: `nexus archive ...`):

1. `nexus archive dump FILE.tsv` — TSV (`\t`, без кавычек; `\t`/`\n` в полях → пробел): строки
   `entities` с `entity_tmdb` и без `active_manual` (курсор, запись в отдельном потоке пачками по
   1000) + все документы архива. Колонки: `id`, `title`, `orig_title`, `type`, `popularity_score`
   (`entity_stats.tmdb_popularity`, нет — 0), `has_ru_locale` (`true`/`false`), `tmdb_id`,
   `release_date`, `cover_url` (`entities.cover_url` / постер документа архива). Читатель берёт колонки
   по заголовку (`tmdb_id`, `release_date`, `cover_url` — необязательные); неверные строки (slug,
   тип, число) пропускает, неверный `cover_url` (не `http(s)://`) — только обнуляет.
2. `nexus archive build FILE.tsv [--fresh] [--defer-delete] [--ru N] [--foreign 500]` (`--ru` по
   умолчанию — все):
   - проход 1 — Top-K: две min-кучи `BinaryHeap<Reverse<(popularity, slug)>>` (при равной
     popularity выше меньший slug), в памяти только 100 000 slug'ов;
   - проход 2 — чтение в блокирующем потоке, пачки по 1000 через канал (ёмкость 4). Горячая пачка —
     один `INSERT … ON CONFLICT (slug) DO UPDATE SET active = true WHERE NOT active AND NOT
     active_manual` (`unnest`); вставленные (вернулись из архива) — с `cover_url` из дампа — получают `entity_tmdb` и
     `entity_stats.tmdb_popularity`; их `tmdb_id` удаляются из `archived_tmdb`; из архива —
     `delete_term`. Холодная пачка — в архив (`delete_term` + `add_document` по slug), кроме
     `active_manual` и сущностей с данными пользователей (они остаются в PostgreSQL и убираются из
     архива);
   - `commit()` архива каждые 100 000 документов; **после** коммита строки PostgreSQL, ушедшие в
     архив, удаляются пачками по 1000 (одна транзакция на пачку: повторная проверка
     `entities_in_use`, `archived_tmdb`, `DELETE FROM entities` — каскадом титры, теги, статистика,
     связки импорта; затем люди без единой работы). Падение посреди сборки оставляет согласованное
     состояние: удалено только то, что уже в архиве; повтор dump + build доводит до конца;
   - `--fresh` — архив пересоздаётся пустым в **текущей схеме** (`Archive::recreate`: под писателем
     удаляются файлы индекса, кроме `.tantivy-*.lock`, затем `Index::create`; дамп уже содержит
     архив). Сервер, открывший индекс раньше, подхватывает новый на коммите и читает постеры сразу
     (номера полей прежние); между пересозданием и первым коммитом (100 000) поиск по архиву пуст;
   - в конце — перестройка Meilisearch с `force` и ISR `{"all": true}`.
   Отчёт: `rows`, `invalid`, `hot` (`inserted`, `activated`), `archived`, `deleted`,
   `kept_in_use`, `kept_manual`, `people_deleted`, `archive_size`, `manual_archived` (строк, не
   допущенных в Top-K: TMDB id с `archived_tmdb.manual`, их проход 1 пропускает).
3. Место на диске PostgreSQL возвращает только `VACUUM FULL` (блокирует таблицы — окно
   обслуживания): `entities`, `entity_credits`, `people`, `person_credit_counts`, `entity_stats`,
   `entity_tmdb`, `entity_tags`, `catalog_import.movies`, `catalog_import.people`.

**Первичная раскладка (миллион строк)** — построчное удаление упирается в FK-каскады и триггер
счётчиков на каждый титр со случайным чтением диска (на dev ~32 строки/с, ~9 ч). Поэтому:
`nexus archive build FILE --fresh --defer-delete` (архив и `archived_tmdb`, строки остаются, поиск не
перестраивается) → `scripts/archive-purge.sql` (одна транзакция: `session_replication_role =
replica` — нужен суперпользователь, `LOCK … EXCLUSIVE` на `entities`/`people` — чтение идёт,
set-based удаление детей, `entities`, людей без работ, пересчёт `person_credit_counts` и
`tags.entities_count` как в миграции `entity_active`, проверка всех FK на `entities`/`people` —
висячие → ROLLBACK; повторный запуск ничего не находит) → `VACUUM FULL` → перестройка поиска
(`POST /admin/search/reindex?force=true` или перезапуск). Тест — `deferred_delete_and_purge_script`.

**Ручной перенос в архив** (2026-10-14, `archive::demote`): `POST /admin/archive/demote`
`{"slugs": [...]}` (1–100, без повторов). Пропускаются с причиной (`skipped[].reason`): `not_found`
(нет на сайте), `no_tmdb` (без `entity_tmdb`: пометку `manual` держит TMDB id), `in_use` (данные
пользователей — `InterestDirectory::entities_in_use`, как в сборке; решение пользователя: не
удалять их). Остальные: `Touched::collect` (адреса страниц, участники) → писатель архива (занят —
409, ничего не меняется) → `upsert` документов (название, оригинальное название, дата, постер, TMDB
id, popularity) и `commit` → одна транзакция: `archived_tmdb` с `manual = true`,
`pipeline::delete_entities` (`entities` вместе с `active_manual` — каскадом титры, теги,
статистика, сырьё TMDB; люди без других работ) → упала БД — документы удаляются из архива обратно →
`publish::run` (удаление из Meilisearch, `fame` участников, ISR). Ответ `{archived, skipped}`.
Возврат — промоушен «скелетом» (описание, титры, жанры теряются). Тест —
`demote_moves_films_to_archive_and_build_keeps_them_cold`.

**Повторная сборка** (новые popularity после «Обновить рейтинги»): `dump` + `build` без `--fresh` —
фильмы архива участвуют в Top-K наравне, поднявшиеся возвращаются в PostgreSQL «скелетом».
Ограничения: возвращённый **сборкой** фильм — «скелетом», без описания, титров и жанров (постер —
из архива, если есть; дозаливка — `tmdb_full.py`). Промоушен и возврат открытием страницы берут
полные данные TMDB ([«Возврат из архива открытием страницы»](#возврат-из-архива-открытием-страницы-)).
«Пересобрать активные» работает только по горячему слою.

### Возврат из архива открытием страницы ✅

Решение пользователя (2026-10-15): кто-то открыл фильм в архиве — значит, ему место на сайте.
Фронтенд показывает архивную страницу (`GET /archive/entities/{slug}`) и сразу шлёт
`POST /archive/entities/{slug}/restore` (публичный): бэкенд за ~1–2 с берёт всё из TMDB, возвращает
фильм на сайт и отвечает **полной карточкой** (`EntityDetail`, как `GET /entities/{slug}`) —
фронтенд подменяет страницу. Код — `archive::restore`, раскладка TMDB — `archive/enrich.rs`.

1. slug неверный — 404; фильм уже на сайте (`active`) — 200 и карточка (второй посетитель,
   параллельный запрос: запросы одного slug в процессе идут по очереди, `IN_FLIGHT`).
2. Документа в архиве нет — 404; не фильм или без TMDB id — 409 `not_restorable`;
   `archived_tmdb.manual` (убран админом) — 409 `kept_in_archive`.
3. Нет `TMDB_API_KEY` или `ARCHIVE_RESTORE_PER_HOUR = 0` — 503 `restore_unavailable`. Лимит
   возвратов в час (`ARCHIVE_RESTORE_PER_HOUR`, по умолчанию 300, `governor`, на процесс и каталог
   архива) — 429 с `Retry-After`: боты не вытащат архив на сайт.
4. `GET {TMDB_API_URL}/movie/{id}?language=ru-RU&append_to_response=credits,release_dates,keywords,
   translations,alternative_titles,images,videos,external_ids,watch/providers,recommendations,
   similar,reviews,lists` (таймаут 8 с). Сеть/5xx/429/401 — 503 `tmdb_unavailable`, фильм остаётся в
   архиве; 404 (фильма в TMDB нет) — возврат «скелетом» из документа архива.
5. Одна транзакция (`enrich::insert_film`, правила полей — как `tmdb_full.py`): `entities`
   (`active`, `active_manual`; название — русское из TMDB, нет перевода — из архива; оригинальное,
   описание, дата, постер `w780`; `metadata`: `runtime_min`, `countries`, `studios`, `age_rating` RU
   иначе US, `trailers` — YouTube, официальные и русские первыми, до 5), `entity_tmdb`, удаление
   из `archived_tmdb`, `entity_stats` (`imdb_id`, `tmdb_popularity`, `tmdb_votes`),
   `entity_tmdb_data` (сырьё), жанры → теги `genre` (slug как `GENRE_SLUGS`; нет тега — создаётся),
   титры: режиссёры, сценаристы, продюсеры, композиторы, операторы, монтажёры, весь актёрский
   состав по `order`; позиции — по ролям. Люди — по TMDB id в `catalog_import.people`, новые
   создаются (slug — транслит имени, `enrich::slugify` = `slugify` из `import_tmdb.py`; занят —
   `<slug>-<tmdb_id>`; фото `h632`). Конфликт уникальности (вернул другой инстанс) — 200 и его
   карточка.
6. Документ удаляется из архива, если писатель свободен (идёт сборка — останется до неё:
   `active_manual` → `keep`).
7. Публикация в фоне (`tokio::spawn`): Meilisearch, `fame` участников, ISR — статическая страница
   генерируется; ответ не ждёт.

Не делается: сиквелы по коллекциям TMDB (поиск по сырью без индекса), рейтинг IMDb (придёт с
«Обновить рейтинги»). Админский промоушен — тот же `insert_film`: с ключом — полные данные, TMDB
недоступен — «скелетом» (warning). Тест — `restore_on_open_brings_the_full_film_back`
(подставной TMDB: полный фильм, 404 → «скелет», 500 → 503, повтор, лимит, `kept_in_archive`).

**Постеры архива** (2026-10-14). Архив собирался без постеров, а `tmdb_full.py` качает только
горячий слой. `scripts/catalog-import/archive_covers.py DUMP OUT [--cache F] [--rate 40]
[--limit N]` — для строк дампа с TMDB id без `cover_url` запрос `GET /3/movie/{id}?language=ru-RU`
(постер русской версии, иначе основной), популярные первыми; ответы — в кэш `OUT.cache` (TSV
`tmdb_id`, `cover_url`; пусто — нет постера или 404), перезапуск продолжает; затем `OUT` — дамп с
колонкой `cover_url`. Порядок: `dump` → `archive_covers.py` (~1,07 млн запросов, ~7,5 ч при 40/с)
→ свежий `dump` → `archive_covers.py` (из кэша, секунды) → `build OUT --fresh` (архив в новой
схеме) → перезапуск не нужен. Дальше постер живёт в дампе и переходит между слоями сам.
Dev: прогон запущен 2026-10-14 (`target/archive-work/`, лог `covers.log`); на первых тысячах
архивных фильмов постер есть у ~92%.

**Первая раскладка dev (2026-10-13/14).** Бэкап до неё — `pg_dump`
`/home/dev/backups/nexus_db_before_archive_2026-10-13.dump` (973 МБ). Дамп: 1 192 695 фильмов TMDB
(121 357 с русским названием, 1 071 338 без), 117 МБ. Сборка `--fresh --defer-delete` — ~27 мин
(debug-сборка, память контейнера ≤ 580 МБ вместе с сервером): горячий слой 100 000 (41 595 включены из
скрытых), архив 1 092 695 документов, 189 МБ. `archive-purge.sql` — 16 мин: удалено 1 070 695 строк
`entities` и 1 741 522 человека без работ; осталось 103 383 сущности (100 000 + 3 383 сидов,
ручных и не из TMDB), активных 100 046, с русским названием 90 401; пользовательские данные —
ни одной исключённой. `VACUUM FULL` — 2 мин: база 7,7 ГБ → 1,0 ГБ. Meilisearch — 98 467 документов
(видимые: невышедшие TMDB скрыты `VISIBLE`).

**Вторая раскладка dev (2026-10-14).** Удалены 3 369 сущностей без TMDB (сиды, сериалы, книги,
игры, фильмы только из Wikidata) и 2 405 людей без работ; 14 «ручных» фильмов TMDB («Интерстеллар»,
«Властелин колец», …) — `active_manual = false`, обычные фильмы. Затем `dump` + `build --ru 1000000
--foreign 500 --defer-delete`: горячий слой 121 871 (121 371 с русским названием, 31 357 вернулись
из архива «скелетом»), архив 1 070 838; `archive-purge.sql` удалил 9 500 иностранных и 38 780 людей
(25 с). Бэкап до неё — `/home/dev/backups/nexus_db_before_seed_purge_2026-10-14.dump`.

Тесты: `app/tests/archive.rs` — `build_splits_hot_layer_and_archive`,
`archive_search_validates_query`, `promote_moves_film_from_archive_to_catalog`,
`deferred_delete_and_purge_script`; Top-K и разбор
дампа — модульные тесты `archive::pipeline`; индекс старой схемы и `recreate` —
`archive::index::legacy_index_is_read_and_recreated`.

## Книги из LibGen ✅

`nexus books import --dump FILE.sql[.gz] [--table updated|fiction] [--batch-size 2000] [--limit N]`
(`catalog::books`, CLI — `app/src/main.rs`; dev: `docker exec nexus-app cargo run -q -p nexus -- books import ...`).
Метаданные русских книг из официального SQL-дампа LibGen (`libgen.sql` → таблица `updated`,
`fiction.sql` → `fiction`); файлы не скачиваются.

- Чтение потоковое (`books::dump::Statements`: операторы по `;` вне строк и комментариев, в памяти
  один `INSERT`), `.gz` — `MultiGzDecoder`; разбор файла — в `spawn_blocking`, запись — пачками,
  каждая в своей транзакции (прерванный импорт продолжается повторным запуском).
- Колонки — по `CREATE TABLE` из дампа или списку колонок `INSERT`; без них — порядок `updated`
  (`books::UPDATED_COLUMNS`). Обязательные: `Title`, `Language`, `MD5`.
- Отбор: `Language` содержит `ru`/`rus`/`Russian`/`Русский` (и в списке `Russian, English`),
  `Visible` пустой, md5 — 32 hex, название непустое. `--limit` — по отобранным строкам.
- Книга (`entities`, `kind = 'book'`, `active = true`): `title` (≤ 300), `release_date` = 1 января
  года (первые 4 цифры `Year`, 1000–2100, иначе `NULL`), `cover_url` = `https://libgen.is/covers/`
  + `Coverurl` (абсолютный URL — как есть), `metadata` через `metadata::validate` (`pages` — ведущие
  цифры `Pages`, `publisher`, `isbn` — первый корректный из `Identifier`; невалидное отбрасывается).
  slug — `slugify(title)` (≤ 80), занят — `-<fnv8(work_key)>`, без букв — `book-<hash>`.
- Всё о книге — в `entities`: md5, формат и `ID` файла — `metadata.libgen.files`, ключ произведения
  (нормализованные название и авторы, `Book::work_key`) — `metadata.libgen.work`. Файлы одного
  произведения — одна книга; карточку задаёт первый файл, следующие (в том числе при повторном
  импорте) дописываются в `files` (до 200). Известные md5 пропускаются; остальные поля созданной
  карточки импорт не меняет.
- Авторы — `people` (`full_name`, slug как у книги, без букв — `author-<hash>`) + `entity_credits`
  (`role = 'author'`, `position` — порядок в `Author`, до 5). `Author` делится по `,`/`;`;
  «Фамилия, Имя Отчество» и «Фамилия, И.О.» — один автор. Тот же автор в следующих книгах —
  человек с тем же именем (`replace(lower(full_name), 'ё', 'е')`), уже записанный автором
  (`role = 'author'`); актёры и режиссёры TMDB с таким именем не подхватываются.
- Событий нет (как у `nexus archive`). Если поиск включён и книги созданы — `search::reindex`
  (`force: false`, `revalidate: true`): книги и авторы попадают в Meilisearch.
- Отчёт: строки дампа, русские, известные, книги, файлы, люди, титры.

Тесты: `app/tests/books.rs` — `imports_russian_books_with_authors_and_is_idempotent`,
`limit_and_dump_without_schema`; разбор дампа и полей — модульные тесты `books::dump`, `books`.

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
| GET | `/entities` | Список видимых (без поиска по названию: `q` → 400 `use_search`, искать — `GET /search`). Фильтры: `kind`, `tag` (slug), `year`, `studio` (slug студии: `metadata.studios` содержит её название; неизвестный — пусто). `sort`: `recommended` (по умолчанию, балл), `trending`, `top_rated`, `new` (только вышедшие), `soon`, `popular` (наши оценки), `rating`, `title` ([«Сортировка и активные»](#сортировка-и-активные-)). В элементах — `release_status`, `imdb_rating`, `imdb_votes` |
| GET | `/studios/{slug}` | Страница студии ([«Студии»](#студии-)): `slug`, `name`, `logo_url`, `origin_country`, `headquarters`, `homepage`, `description`, `parent` (`{slug, name, logo_url}` или `null`), `entities_count`. Данные компании — из TMDB при первом открытии. Фильмы — `GET /entities?studio={slug}`. 404 — нет студии |
| GET | `/entities/{slug}` | Карточка: все поля (с `active`), `studios` (`[{slug, name, logo_url}]` по порядку `metadata.studios`), теги, участники в порядке титров (`position`), `related` (активные). Из PostgreSQL. Скрытая — 404; с токеном админа — 200 (и скрытые связанные); неверный токен — 401 |
| GET | `/entities/{slug}/similar` | Похожие активные (Meilisearch: тот же тип, хотя бы один общий тег, по баллу «Рекомендуемые», затем новее): `?limit=` 1–24 (по умолчанию 12) → `Page<EntitySummary>` ([«Похожие»](#похожие-)); у скрытой — 404; 503 — Meilisearch недоступен |
| GET | `/people` | Список публичных людей по алфавиту (с активной работой или без работ), `q` — подстрока в имени |
| GET | `/people/{slug}` | Человек и его работы в активных сущностях, новые сверху. Из PostgreSQL. Все работы скрыты — 404; админу — 200 со всеми работами |
| GET | `/tags` | Все теги с `kind` и `entities_count` (активных сущностей), без пагинации (тегов немного); `?kind=genre\|tag` — только жанры или только теги |
| GET | `/tags/{slug}` | Тег: `id`, `slug`, `name`, `kind`, `entities_count`; нет — 404 |
| GET | `/archive/search` | Поиск по холодному архиву (Tantivy): `q` (1–200 символов, все слова в `title` или `orig_title`; синтаксис Tantivy разбирается снисходительно), `limit` 1–20 (20), `offset` 0–1000 (0) → `Page<ArchiveDoc>` `{items, total, limit, offset}` (`total` — всего найдено, `Count`); `ArchiveDoc`: `slug`, `title`, `original_title`, `kind`, `tmdb_id`, `release_date`, `popularity`, `cover_url` (`null` — нет постера). Публичный. На сайте — второй блок выдачи после `GET /search` (чекбокс «Архив», по умолчанию включён). 400 — пустой/длинный `q`, `limit`/`offset` вне диапазона; 503 — нет `ARCHIVE_DIR` или индекс не открывается |
| GET | `/archive/entities/{slug}` | Фильм из архива для его страницы (только чтение): `ArchiveDoc` (название, оригинальное название, дата, постер). Публичный. 404 — нет в архиве или неверный slug (фильм мог вернуться на сайт — `GET /entities/{slug}`); 503 — архив недоступен |
| POST | `/archive/entities/{slug}/restore` | Открыли архивную страницу — вернуть фильм на сайт со всеми данными TMDB ([«Возврат из архива открытием страницы»](#возврат-из-архива-открытием-страницы-)) → 201 `EntityDetail`; уже на сайте → 200 `EntityDetail`. Публичный. 404 — нет ни в архиве, ни на сайте; 409 `kept_in_archive` (убран админом) / `not_restorable` (не фильм TMDB); 429 — лимит `ARCHIVE_RESTORE_PER_HOUR` (`Retry-After`); 503 `restore_unavailable` (нет ключа TMDB, лимит 0) / `tmdb_unavailable` (TMDB не ответил; фильм в архиве) / архив недоступен |
| GET | `/search` | Весь поиск сайта, Meilisearch, по видимым: `q` (название, оригинальное, студии; пустой — все), фильтры `kind`, `tag`, `year` (ровно) или `year_from`/`year_to` (диапазон включительно), `studio` (точное название, без учёта регистра), `studio_slug` (slug студии), `person` (slug, в любой роли) и `person`+`role` (в этой роли) ([«Поиск»](#поиск)). По релевантности; `sort=recommended` / `trending` / `top_rated` — по баллу, TMDB popularity, рейтингу IMDb; `new` (вышедшие, новые сверху), `soon` (будущие, ближайшие сверху), `title` (без значения — в конце), `popular` (больше оценок Nexus, затем новее), `rating` (средняя оценка Nexus, затем число оценок, затем новее; без оценок — в конце; оценки доезжают до индекса за ~1 мин); другое значение — 400. `total` — точное число совпадений (до 500000), листается весь результат. 400 — `year` вместе с диапазоном, `year_from > year_to`, год вне 1–9998, пустая/длинная (>200) студия, `role` без `person`, неверный slug тега/человека |
| GET | `/search/people` | Поиск людей, Meilisearch, индекс `people` ([«Поиск людей»](#поиск-людей-)): `q` (имя по-русски или латиницей, опечатки, начало слова; до 300 символов; пустой — самые известные), `known_for` (основная роль: `actor`, `director`, …), `limit` 1–100 (20), `offset` → `Page<PersonHit>` (`total` точный): `id`, `slug`, `full_name`, `original_name`, `birth_date`, `photo_url`, `known_for`. Ранжирование — качество совпадения, затем известность (`person_stats.fame`). 400 — длинный `q`, `known_for` не `a-z`/`_`; 503 — Meilisearch недоступен; индекса нет — пустая страница |
| GET | `/search/studios` | Подсказки для фильтра `studio` (facet search Meilisearch): `q` — часть названия, опечатки допустимы (пустой — самые частые), `kind`, `limit` 1–50 (10) → `[{"name", "slug", "count"}]` (`slug` — страница студии, значение для `studio_slug`; `null` — страницы нет). 400 — `q` длиннее 200; 503 — Meilisearch недоступен; индекса нет — `[]` |

**`related`** в карточке — одна лента для карусели: `[{...EntitySummary, "relation": "prequel"|"sequel"|"remake"}, ...]`, хронологически по `release_date` (без даты — в конце, затем `position`, затем заголовок), пусто — `[]`. Сиквелы, ремейки и приквелы идут **в одной полосе**, тип связи — у каждой карточки. Связи — в самой карточке, а не отдельным запросом: карточка — статика фронтенда, они попадают в HTML.

**Пагинация** списков и поиска: `?limit=20&offset=0` (`limit` 1–100, по умолчанию 20; значения вне
диапазона приводятся к границам). Ответ:

```json
{ "items": [ ... ], "total": 137, "limit": 20, "offset": 0 }
```

В поиске `total` — точное число совпадений Meilisearch (`totalHits`, см. «Точный `total`»).

### Похожие ✅

`GET /entities/{slug}/similar` — через Meilisearch (решение пользователя 2026-10-15: «хотя бы один
общий тег», без подсчёта совпадений). `entities::similar` читает из БД только саму сущность (видима
ли, `kind`), slug'и её тегов и slug'и связанных; список — `search::similar`:

- фильтр `kind = <kind> AND tags IN [<теги сущности>] AND slug NOT IN [<сама и связанные>]` —
  тот же `kind`, хотя бы один общий тег, без самой сущности и её связанных (`related` и так на
  карточке); число общих тегов и жанры на порядок не влияют;
- сортировка `rank_score:desc, release_ts:desc` — балл «Рекомендуемые», затем новее; без значения —
  в конце;
- `limit` 1–24 (12), `offset` нет; `total` — точное число подходящих (`search_exact`);
- нет тегов — пустая страница (`total: 0`), не 404; скрытая сущность — 404; Meilisearch недоступен
  — 503; индекса нет — пустая страница.

До 2026-10-15 был SQL-запрос с весами общих тегов (жанр — 2, тег — 1) по `entity_tags`.

### Админка (только `admin`, иначе 401/403)

| Метод | Путь | Что делает |
|---|---|---|
| GET | `/admin/entities` | Все сущности, включая скрытые: фильтры и `sort` как у `/entities` плюс `q` (подстрока в названии или оригинальном, без учёта регистра, по БД) и `active=true\|false` → `Page<AdminEntitySummary>` (`EntitySummary` + `active`) |
| POST | `/admin/entities` | Создать сущность (можно сразу с `tags`: slug'и; `active`, по умолчанию `true`) → 201, карточка |
| PATCH | `/admin/entities/{id}` | Изменить: переданные поля заменяются, `null` очищает необязательное, `metadata` заменяется целиком, `active` включает или скрывает (и ставит `active_manual`). `kind` не меняется |
| PATCH | `/admin/entities/{id}/stream` | То же, ход публикации потоком `text/event-stream` ([«Публикация»](#публикация-правок-)) |
| DELETE | `/admin/entities/{id}` | Удалить (каскадно теги, участники, рецензии, элементы коллекций) → 204 |
| PUT | `/admin/entities/{id}/tags` | Заменить набор тегов: `{"tags": ["sci-fi"]}` |
| POST | `/admin/entities/{id}/credits` | Добавить участника: `person_id`, `role`, `character_name`, `position` → 201 |
| DELETE | `/admin/entities/{id}/credits/{credit_id}` | Удалить участника (`credits[].id` из карточки) → 204 |
| PUT | `/admin/entities/{id}/relations` | Связать: `{"kind": "sequel" \| "prequel" \| "remake", "to_id": "...", "position"?: 0}` — `to_id` сиквел, приквел или ремейк сущности → 200, `related` сущности. С собой — 409 `self_relation`; такая связь уже есть (в любую сторону) — 409; нет `to_id` — 400 |
| DELETE | `/admin/entities/{id}/relations/{to_id}?kind=...` | Убрать связь того же вида (`prequel` — со стороны сиквела, ремейк — с любой) → 204, нет — 404 |
| POST | `/admin/people` | Добавить человека (`slug`, `full_name`, `original_name`, `birth_date`, `photo_url`, `bio`) → 201; в индексе `people` сразу |
| PATCH / DELETE | `/admin/people/{id}` | Изменить (`null` очищает; `original_name` — до 300 символов) / удалить вместе с участием; `fame` и документ в индексе `people` пересчитываются |
| POST | `/admin/tags` | Создать тег: `slug`, `name`, `kind` (`genre` / `tag`, по умолчанию `tag`) → 201 |
| PATCH / DELETE | `/admin/tags/{id}` | Переименовать, сменить slug или `kind` / удалить (снимается со всех сущностей). Карточки с тегом пересобираются |
| POST | `/admin/search/reindex` | Перестроить поисковый индекс без простоя (`?force=true` — без проверки размера, `SEARCH_REINDEX_MIN_PERCENT`: 80, в dev 0) → `ReindexResult`; `409` — уже идёт или новый индекс слишком мал |
| POST | `/admin/search/reindex/stream` | То же потоком `text/event-stream` ([«Перестройка поиска»](#перестройка-поиска-)) |
| POST | `/admin/tmdb/stats/stream` | «Обновить рейтинги»: дампы TMDB (popularity) и IMDb (рейтинг) → `entity_stats`, пересчёт балла, перестройка поиска; потоком; `done.stats`. `409` — уже идёт долгая операция ([«Сортировка и активные»](#сортировка-и-активные-)) |
| POST | `/admin/tmdb/active/stream` | «Пересобрать активные»: `?count=` (80000, 0–1000000, иначе 400) вышедших по баллу + все «Скоро премьера»; потоком; `done.active`. `409` — как выше |
| POST | `/admin/archive/demote` | Перенести фильмы с сайта в архив: `{"slugs": [...]}` 1–100 → 200 `{"archived": [slug], "skipped": [{"slug", "reason": "not_found" \| "no_tmdb" \| "in_use"}]}`; документы в Tantivy, строки удалены из БД (с людьми без работ), `archived_tmdb.manual` — сборка не вернёт; поиск и статика обновляются. 400 — пусто, >100, повтор, неверный slug; 409 — писатель архива занят (ничего не перенесено); 503 — архив недоступен |
| POST | `/admin/archive/promote/{slug}` | Вернуть фильм из архива (`enrich::insert_film`): с `TMDB_API_KEY` — со всеми данными TMDB (как возврат открытием страницы), TMDB недоступен или ключа нет — «скелетом» (`title`, `original_title`, `release_date`, `cover_url`); `active`, `active_manual`, `entity_tmdb`, `entity_stats`, удаление из `archived_tmdb` (и пометки `manual`); затем `delete_term` + `commit` в Tantivy, публикация (Meilisearch, ISR) → 201, карточка. 404 — нет в архиве (и неверный slug); 409 — slug или TMDB id уже в каталоге, писатель архива занят сборкой (в БД ничего не пишется); 503 — архив недоступен |
| GET | `/admin/ranking` | Настройки балла и окна «Скоро премьера» |
| POST | `/admin/ranking/preview` | Топ по формуле с настройками из тела (`?limit=` 1–200, 50), без записи → `[RankPreviewRow]`; неверные настройки — 400 |
| PUT | `/admin/ranking/stream` | Сохранить настройки и применить потоком: `rank`, перестройка поиска. 400 / 409 |

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

## Мини-дамп — удалён

До 2026-10-14 `seeds/catalog.sql` (46 сущностей всех типов, люди, теги) грузил `make seed`. По
решению пользователя сиды каталога удалены вместе с файлом и тестом `seed_catalog_is_valid`:
каталог — только фильмы TMDB ([«Полные данные TMDB»](#полные-данные-tmdb-)), `make seed` грузит
лишь тестовых пользователей (`seeds/dev.sql`). Тестам данные дают фикстуры
`app/tests/fixtures/catalog.sql`.

## Каталог фильмов из TMDB + Wikidata (основной путь)

Три шага, всё в `scripts/catalog-import/`, кэш — `~/.cache/nexus-catalog-import/` (вне репозитория:
`cargo-watch` перезапускал бы приложение на каждую запись):

1. `fetch_tmdb.py` — только скачивание TMDB: все фильмы ежедневного дампа, кроме `video`
   (~1,19 млн, по убыванию popularity), и TMDB id из кэша Wikidata.
   `GET /3/movie/{id}?append_to_response=credits&language=ru-RU`, 40 запросов/с, ответ ужат до
   нужных полей (до 30 актёров, режиссёры) → `tmdb/movies.jsonl`, 404 → `tmdb/missing.txt`. ~8 ч,
   перезапуск продолжает. Ключ — `TMDB_API_KEY` из окружения, корневого `.env` или `infra/.env`
   (v3-ключ или v4-токен).
2. `import_wikidata.py --stage download` и `--stage wiki` — кэш Wikidata (QLever) и Википедии, см.
   ниже; люди — вместе с TMDB id человека (P4985). Этап `load` после появления `catalog_import`
   не запускается: в БД пишет только `merge.py`.
3. `merge.py` — слияние в БД: `DATABASE_URL=… python3 scripts/catalog-import/merge.py`, затем
   `scripts/reindex-search.sh`.

**Один фильм — одна строка.** Схема `catalog_import` (создаёт скрипт, приложение не читает):
`movies` (`entity_id` PK ↔ `tmdb_id` UNIQUE ↔ `qid` UNIQUE, `created`, `written`), `people`
(`person_id` ↔ `tmdb_id` ↔ `qid`, `created`, `written`). Источники связывает сама Wikidata: P4947 —
TMDB id фильма, P4985 — TMDB id человека (у 77% людей фильмов). Фильм ищется по связке, затем среди
готовых непривязанных (сиды) по slug (с годом или без) или «название + год» (они читаются один раз
при старте), и только потом создаётся; slug занят — суффикс `-t<tmdb_id>` / `-q<qid>`. Связки
первого импорта (`wikidata_import`) переносятся при первом запуске; после этого
`DROP SCHEMA wikidata_import CASCADE` безопасен. Если у двух записей Wikidata один TMDB id
(фильм и режиссёрская версия: Apocalypse Now и Redux), id достаётся самой известной; строка,
уже слитая в этом прогоне, второй раз не обновляется (в логе — `conflicts`).

**Поля — первый непустой источник:**

| Поле | Порядок |
|---|---|
| `title` | русское из TMDB → русское из Wikidata → любое (TMDB, затем en Wikidata) |
| `original_title` | TMDB → Wikidata P1476 / en |
| `description` | TMDB (ru) → вступление ru.wikipedia |
| `cover_url` | TMDB `w780` → Commons; несвободные постеры en/ru.wikipedia не используются и удаляются |
| `release_date`, `runtime_min` | TMDB → Wikidata |
| `countries`, `studios`, жанры, режиссёры | объединение обоих (студии — без учёта регистра, до 20) |
| актёры | порядок TMDB (как в титрах), до 15, добор из Wikidata; персонаж — русский из Wikidata, если в TMDB не по-русски |
| люди: `full_name` | русское (Wikidata → TMDB), иначе TMDB / en |
| люди: `photo_url` | TMDB `h632` → Commons |
| люди: `birth_date`, `bio` | Wikidata |

Жанры TMDB → теги по `GENRE_SLUGS` (`import_tmdb.py`, + `music` «Музыка», `tv-movie`
«Телефильм»), Wikidata — по `GENRES`; один набор slug, новые — `kind = genre`.

**Ручные правки не теряются.** В `written` — то, что скрипт записал в каждое поле, и хэши титров и
жанров. Поле обновляется, только если оно пустое или в нём всё ещё записанное скриптом; пустым
значение не затирается (кроме несвободного постера). Титры и жанры заменяются целиком, только
если их не меняли после импорта. У готовых строк (сиды, админка: `created = false`) — только
заполнение пустых полей, титры и жанры не трогаются. Проверено: правка названия, bio и удалённый
актёр пережили повторное слияние; повторный прогон без изменений источников ничего не меняет
(`updated 0`, `people ~0`).

**Скорость** (тест на копии dev: 19 тыс. TMDB + 100 тыс. Wikidata): первый прогон ~180 фильмов/с,
повторный ~480/с (Python ~30%, БД ~70%). Оценка объёма после полного слияния (~1,2 млн фильмов):
PostgreSQL ~5–7 ГБ (`entity_credits` ~8–12 млн строк, люди ~1,5–2 млн), индекс Meilisearch
~5–8 ГБ на версию; кэш TMDB ~4–6 ГБ. Картинки не храним — только ссылки на CDN.

**Лицензии.** TMDB: некоммерческий сайт — бесплатно с атрибуцией (логотип и «This product uses
the TMDB API but is not endorsed or certified by TMDB» — задача фронтенда), коммерческий —
отдельная лицензия TMDB; права на постеры у правообладателей. Wikidata — CC0, тексты Википедии —
CC BY-SA, Commons — по лицензии файла с атрибуцией.

**Активные.** `merge.py` вставляет новые фильмы скрытыми, пишет связи в `entity_tmdb` и
`entity_stats` (`imdb_id`, `tmdb_votes`, `tmdb_popularity`, если её нет); включает кнопка админки
«Пересобрать активные» ([«Сортировка и активные»](#сортировка-и-активные-)). Привязанные готовые
строки (сиды) получают `active_manual = true`. Первая раскладка на dev (2026-10-12) — ещё
скриптом (40 000 по `vote_count` + 40 000 по `popularity`); её место занимает пересборка по баллу.

**Статистика — `entity_stats`, не `metadata`** (миграция `20261018100000_entity_stats.sql`):
`metadata.popularity`/`vote_count` устарели — `MovieMetadata` их принимает и отбрасывает (старые
формы админки не получают 400), `merge.py` убирает их при записи, разом —
`merge.py --cleanup-metadata` (~25 мин: GIN-индексы). `merge.py --numbers-only` — `entity_stats` из
кэша TMDB пачками (2 мин на 1,2 млн).

## Сортировка и активные ✅

**Статус выхода** — по сегодняшней дате, без заданий: `released` (`release_date ≤ сегодня`), `soon`
(выход в ближайшие `catalog_ranking.soon_days`, 30 дней — «Скоро премьера»), `announced` (позже или
без даты); поле `release_status` в `EntitySummary`, карточке и выдаче поиска. **Видимость**
(`VISIBLE` в `models.rs`, поле `visible` карточки и админского списка): `active` и — для фильмов из
TMDB (`entity_tmdb`) — `released` или `soon`, либо `active_manual`. Невышедшие и без даты из TMDB
скрываются сами и сами появляются за `soon_days` дней до выхода (поиск — при ежедневной перестройке
или кнопке); не из TMDB (сиды, админка) и включённые вручную — видны с любой датой. `VISIBLE`
используют списки, карточка (404), похожие, связанные, фильмографии, справочник для `social`,
индексация. Счётчики людей и тегов считают `active` (без окна дат).

**Балл «Рекомендуемые»** (`catalog::ranking`, `modules/catalog/src/ranking.rs`) — сортировка по
умолчанию, у вышедших фильмов с `imdb_votes ≥ min_votes`:
`score = weight_quality·q + weight_popularity·p + weight_recency·r`, где `q` — байесовский рейтинг
IMDb `(v·r + m·C)/(v + m)` (`m` = `trust_votes`, `C` — средний), шкала 5 → 0 … 9 → 1; `p` —
`ln(1+popularity)/ln(1+p99)` (99-й перцентиль TMDB popularity); `r` — `0.5^(возраст/half_life_years)`.
Настройки — `catalog_ranking` (одна строка): по умолчанию 0.5 / 0.3 / 0.2, `trust_votes` 5000,
`half_life_years` 4, `min_votes` 1000, `soon_days` 30. Хранится в `entity_stats.rank_score` и
`quality`; пересчёт — после «Обновить рейтинги» и сохранения настроек (один `UPDATE` узкой таблицы).

**Сортировки** `/entities` (`sort`): `recommended` (по умолчанию, балл; без балла — в конце, по
`tmdb_votes`, затем `tmdb_popularity`), `trending` (TMDB popularity, вышедшие с баллом), `top_rated` (байесовский рейтинг), `new`
(только вышедшие), `soon` (будущие в окне, ближайшие сверху), `popular`/`rating` (наши оценки),
`title`. Фильмы с датой в будущем в публичных списках — только в `soon` («Скоро премьера»), в
остальных сортировках их нет. Поиск: `sort=recommended|trending|top_rated|new|soon|title`
(`rank_score`, `popularity`, `quality`, `release_ts`, `title` — `sortableAttributes`); в поиске
будущие фильмы не скрываются вне `soon`.

**Админка** (запуск вручную, расписания нет; ход — потоком `JobEvent`; одна долгая операция за раз
— общая блокировка с перестройкой поиска, занято — `409` до потока; без Meilisearch — только БД):
- **Обновить рейтинги** — `POST /admin/tmdb/stats/stream`: дамп TMDB `movie_ids_*.json.gz`
  (`TMDB_EXPORTS_URL`, без ключа, ~28 МБ; кроме `video`) → `tmdb_popularity`; IMDb
  `title.ratings.tsv.gz` (`IMDB_DATASETS_URL`, без ключа, ~9 МБ) → `imdb_rating`, `imdb_votes` по
  `imdb_id`; шаги `tmdb_export`, `imdb_ratings`, `stats` (с `progress`), `rank`, затем перестройка
  поиска; `done.stats` — `{tmdb_export, tmdb_updated, imdb_updated, ranked}`. Ошибка скачивания —
  с причиной (`… Connection refused`). Некоторые DNS (Яндекс `77.88.8.8`, часть провайдеров)
  отвечают на `files.tmdb.org` заглушкой `127.0.0.1`: в dev у `app` — `dns: [1.1.1.1, 8.8.8.8]`
  (`infra/docker-compose.yml`), на проде нужен такой же резолвер или зеркало (`TMDB_EXPORTS_URL`).
- **Пересобрать активные** — `POST /admin/tmdb/active/stream?count=80000` (0–1000000; 250 000 —
  `?count=250000`): из связанных с TMDB без `active_manual` активны топ `count` вышедших по баллу
  (без балла — по `tmdb_votes`, `tmdb_popularity`) и все `soon`; остальные (в том числе анонсы и без
  даты) скрыты. Один `UPDATE` разницы (операторный триггер сдвигает счётчики), затем перестройка
  поиска с `force` и ISR `{"all": true}`; `done.active` — `{activated, deactivated, soon, active}`.
  Сначала — «Обновить рейтинги»: топ по текущему баллу.
- **Настройка сортировки** — `GET /admin/ranking`; `POST /admin/ranking/preview?limit=50` (тело —
  настройки, топ вышедших по формуле с составляющими `q_part`, `p_part`, `r_part` и `active`, ничего
  не пишет — подбирать веса на глаз); `PUT /admin/ranking/stream` (сохранить, `rank`, перестройка
  поиска с `force`). Проверка: веса 0–10, `half_life_years` 0.1–100, голоса 0–10 000 000,
  `soon_days` 0–365, иначе 400.
- Редакционные подборки («Выбор редакции») — позже.

Лицензии: IMDb datasets — только личное и некоммерческое использование (как бесплатный TMDB);
коммерческий запуск — лицензия IMDb.

Тесты: `unreleased_tmdb_films_are_hidden_until_soon`, `ranking_preview_and_save`,
`search_skips_hidden_and_sorts_by_stats`, `legacy_metadata_tmdb_keys_are_dropped`,
`stats_refresh_from_tmdb_and_imdb` (фейковые дампы TMDB и IMDb, повтор — 0 изменений),
`active_rebuild_by_count_keeps_soon_and_manual`; разбор дампов — модульные тесты.

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
| `metadata.rs` | Схемы `metadata` по типам и их проверка (у книг — `LibgenSource`, `LibgenFile`) |
| `validate.rs` | Проверка slug, строк, URL, ролей; UNIQUE → 409 |
| `entities.rs`, `people.rs`, `tags.rs` | Чтение |
| `search.rs` | Индекс `entities` (`SearchDoc`, `entity_settings`), `update_index` (сущности и люди), перестройка обоих индексов с версиями (в т.ч. по расписанию), `GET /search` (фильтры — `filters_of`), `GET /search/studios` |
| `studios.rs` | Студии: `sync` (из `metadata.studios` и сырья TMDB), `refs` (ссылки в карточке), `GET /studios/{slug}` (данные компании из TMDB при первом открытии), `metadata_filter` для `GET /entities?studio=` |
| `people_search.rs` | Индекс `people` (`PersonDoc`, `settings`, `fill`, `update_index`), известность `refresh_fame` → `person_stats`, `GET /search/people` |
| `tmdb.rs` | Операции админки: «Обновить рейтинги» (дампы TMDB и IMDb), «Пересобрать активные» |
| `archive/enrich.rs` | Полные данные TMDB: `tmdb_get` (общий запрос с ключом; и для студий), `fetch_movie`, `film_fields`, `film_credits`, `slugify`, `insert_film` (фильм, статистика, сырьё, жанры, люди, титры) |
| `archive/index.rs` | Индекс Tantivy архива: схема (с `cover_url`), открытие по каталогу (и старой схемы), `recreate`, поиск с `offset` и `Count`, `get`, `upsert`/`delete`, писатель 50 МБ |
| `archive/pipeline.rs` | `dump` (TSV из БД и архива, с `cover_url`), `build` (Top-K без `archived_tmdb.manual`, маршрутизация, удаление из PostgreSQL; `--fresh` — `recreate`), `delete_entities` (и для ручного переноса) |
| `archive/mod.rs` | `GET /archive/search`, `GET /archive/entities/{slug}`, `POST /archive/entities/{slug}/restore` (лимит, `IN_FLIGHT`), `POST /admin/archive/promote/{slug}`, `POST /admin/archive/demote`, `archive::open` |
| `books/mod.rs` | `nexus books import`: отбор русских строк, `Book`, `work_key`, авторы, запись пачками (`entities` с `metadata.libgen`, `people`, `entity_credits`) |
| `books/dump.rs` | Потоковый разбор дампа MySQL: `Statements`, `create_table`, `insert` |
| `ranking.rs` | Балл «Рекомендуемые»: формула, настройки, предпросмотр, пересчёт |
| `publish.rs` | Публикация правки: `Touched`, поиск, ISR, поток `.../stream` |
| `jobs.rs` | Шаги и события потоков (`JobEvent`), `Reporter`, ответ `text/event-stream` |
| `admin/` | Админские эндпоинты (`relations.rs` — связанные произведения) |
| `media/` | `nexus media` (`media/pick.rs` — фильтр Rutube) |

Тесты — `app/tests/catalog.rs` (данные — `app/tests/fixtures/catalog.sql`), архив —
`app/tests/archive.rs` (свой каталог индекса на тест). CLI `nexus archive` и `nexus books` — `app/src/main.rs`, импорт книг — `app/tests/books.rs`. Тесты поиска ходят в
настоящий Meilisearch из `make up` (в CI — сервис в `ci.yml`), у каждого теста свой префикс
индексов. Остальные тесты идут с выключенным поиском. Живые проверки — `http/catalog.http`.

</code_map>
