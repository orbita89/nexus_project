# Модуль `social` — LOD 2

**Summary (EN):** reviews & ratings, votes, comments, reactions, follows, collections, forum threads/posts, interests, marks, feed, moderation, events for realtime.
**Отвечает за:** всё общение вокруг контента — форумы, рецензии и оценки, коллекции, подписки
на пользователей, интересы.
**URL:** `/api/v1/social` · **Код:** `modules/social/` · **Источник (LOD 3):** `documents/modules/social.md`

**Статус:** ✅ рецензии и оценки (спойлеры, лайки и дизлайки, комментарии, только после выхода),
реакции, отметки «позже» / «просмотрено», подписки на пользователей, коллекции (с превью),
социальный профиль, форум (темы на несколько сущностей, сообщения с ветками, закрытие тем),
интересы (сущности, люди, теги), чат по интересам (комнаты сущностей, людей, тегов и свои комнаты
админа, отправка по WebSocket), лента, события
для `realtime`, модерация, мини-дамп.
🕓 «новое в ленте» по подпискам в реальном времени, более умное «популярное».

Все таблицы ссылаются на `entities.id`, а не на «фильм» или «книгу». Поэтому любая функция
модуля сразу работает для любого типа контента.

<tables>

## Таблицы

### `follows` ✅ — подписки на пользователей

Кто на кого подписан. Используется для ленты «что пишут люди, на которых я подписан».

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `follower_id` | `uuid` | PK, FK → `users`, CASCADE | Кто подписался |
| `followee_id` | `uuid` | PK, FK → `users`, CASCADE | На кого |
| `created_at` | `timestamptz` | NOT NULL | |

**Ограничения:** нельзя подписаться на себя (`follower_id <> followee_id`); повторная подписка
невозможна (PK).
**Индексы:** PK — «на кого подписан я»; по `followee_id` — «кто подписан на меня».

### `user_likes` ✅ — лайки профилей

Миграция `20261119100000_social_user_likes.sql`. Только лайк: дизлайка у пользователя нет —
оценивают человека, а не рецензию, и минус здесь работал бы как травля.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `user_id` | `uuid` | PK, FK → `users`, CASCADE | Кто лайкнул |
| `liked_id` | `uuid` | PK, FK → `users`, CASCADE | Кого |
| `created_at` | `timestamptz` | NOT NULL | |

**Ограничения:** себе лайк не поставить (CHECK `user_likes_not_self`); повторный невозможен (PK).
**Индексы:** PK; (`liked_id`, `created_at DESC`) — счётчик профиля и «кто лайкнул», новые сверху.
Отсюда же берётся одно из слагаемых рейтинга пользователя (этап «карма»).

### `reviews` ✅ — рецензии и оценки

Отзыв пользователя о произведении. Это одновременно и оценка, и рецензия: можно поставить только
оценку, только написать текст, или и то и другое.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `user_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | Автор |
| `entity_id` | `uuid` | NOT NULL, FK → `entities`, CASCADE | О каком произведении |
| `rating` | `smallint` | 1–10 или NULL | Оценка |
| `body` | `text` | | Текст рецензии |
| `is_spoiler` | `boolean` | NOT NULL, default `false` | В тексте спойлеры (миграция `20261014100000_review_spoilers.sql`) |
| `likes_count`, `dislikes_count` | `bigint` | NOT NULL, ≥ 0 | Голоса, пишет триггер на `review_votes` |
| `comments_count` | `bigint` | NOT NULL, ≥ 0 | Комментарии без удалённых, пишет триггер на `review_comments` |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер, только при правке `rating`, `body`, `is_spoiler`: счётчики его не меняют |

**Ограничения:** одна рецензия на пару «пользователь — произведение» (повторная — это редактирование);
нужна хотя бы оценка или текст.
**Индексы:** (`entity_id`, `created_at DESC`) — свежие рецензии на странице произведения;
(`user_id`, `created_at DESC`) — рецензии на странице пользователя (миграция
`20261008100000_social_indexes.sql`).

### `review_votes` ✅ — лайки и дизлайки рецензий

Миграция `20261014110000_review_votes.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `review_id` | `uuid` | PK, FK → `reviews`, CASCADE | |
| `user_id` | `uuid` | PK, FK → `users`, CASCADE | Кто голосовал |
| `value` | `smallint` | NOT NULL, `1` или `-1` | Лайк или дизлайк |
| `created_at` | `timestamptz` | NOT NULL | |

Один голос на пару «пользователь — рецензия»; за свою рецензию голосовать нельзя (проверяет код).
Счётчики `reviews.likes_count` / `dislikes_count` обновляет триггер `review_votes_count` в той же
транзакции — и при каскадном удалении голосов. **Индексы:** PK; `user_id` — свои голоса на карточке.

### `review_comments` ✅ — комментарии к рецензиям

Миграция `20261014120000_review_comments.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `review_id` | `uuid` | NOT NULL, FK → `reviews`, CASCADE | |
| `author_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | |
| `body` | `text` | NOT NULL, 1–2000 символов | |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |
| `deleted_at` | `timestamptz` | | Удалён автором или admin: из списков пропадает |

Плоский список без веток. Ограничения частоты нет, как и у сообщений форума. `reviews.comments_count` (без удалённых) обновляет триггер
`review_comments_count`. **Индексы:** частичный (`review_id`, `created_at`) для неудалённых —
комментарии рецензии по порядку.

### `entity_reactions` ✅ — реакции на произведение

🔥 😢 🤔 😴 под оценкой на карточке. Миграция `20261014130000_entity_reactions.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | |
| `user_id` | `uuid` | PK, FK → `users`, CASCADE | |
| `kind` | `text` | NOT NULL, `fire` / `sad` / `think` / `bored` | |
| `created_at` | `timestamptz` | NOT NULL | Когда поставлена (смена реакции — заново) |

Одна реакция на зрителя: другая заменяет прежнюю. **Индексы:** PK — подсчёт по сущности; `user_id`.

### `comments` ✅ — комментарии к произведениям, людям и студиям

Миграция `20261118100000_social_comments.sql`. Одна таблица на три вида целей: правила, лимиты и
модерация у них общие, а эндпоинт один (`/comments?target=entity:dune-2021`). Комментарии к
рецензиям (`review_comments`) и обсуждения коллекций (`collection_comments`) остаются при своих
таблицах: у них строгие FK и свои правила (закрытие обсуждения, `reviews.comments_count`).

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK, `uuidv7()` | |
| `target_kind` | `text` | NOT NULL, CHECK `entity` / `person` / `studio` | Вид цели |
| `target_id` | `uuid` | NOT NULL | Строка в `entities`, `people` или `studios` — **без FK**: колонка одна, таблиц три |
| `author_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | |
| `parent_id` | `uuid` | FK → `comments`, CASCADE | Корневой комментарий той же цели (проверяет код); `NULL` — сам корневой |
| `body` | `text` | NOT NULL, 1–2000 символов | |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |
| `deleted_at` | `timestamptz` | | Мягкое удаление: из списков пропадает |

Существование цели проверяет код — через справочники (`EntityDirectory`, `PersonDirectory`,
`StudioDirectory`), а осиротевшая строка (цель удалили) просто не попадает в выдачу: справочник
не вернёт ссылку. **Индексы:** частичный (`target_kind`, `target_id`, `created_at`, `id`) для
неудалённых — лента цели; (`parent_id`) — ответы корня; частичный (`author_id`, `created_at DESC`)
— «мои комментарии» и лимит частоты.

### `collection_reactions` ✅ — реакции на коллекцию

Та же миграция. 🔥 😢 🤔 😴 под подборкой — как `entity_reactions` у произведения.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `collection_id` | `uuid` | PK, FK → `collections`, CASCADE | |
| `user_id` | `uuid` | PK, FK → `users`, CASCADE | |
| `kind` | `text` | NOT NULL, `fire` / `sad` / `think` / `bored` | |
| `created_at` | `timestamptz` | NOT NULL | Смена реакции — заново |

Одна реакция на зрителя: другая заменяет прежнюю. **Индексы:** PK; `user_id`.

### `collections` ✅ — коллекции

Пользовательские подборки: «Лучшая фантастика», «Экранизации Кинга: книга vs фильм». В одной
коллекции могут быть произведения разных типов.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `user_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | Владелец |
| `title` | `text` | NOT NULL | Название |
| `description` | `text` | | Описание |
| `is_public` | `boolean` | NOT NULL, default `true` | Видна всем или только владельцу |
| `featured_position` | `integer` | CHECK: `NULL` или `is_public` | Особая коллекция (помечает admin, «Топ‑10»): порядок на главной; `NULL` — обычная |
| `slug` | `text` | UNIQUE; CHECK `collections_slug_featured_check`: `NULL` или особая | Читаемый адрес особой (`/collections/{slug}`): `^[a-z0-9]+(-[a-z0-9]+)*$`, 3–100, не UUID, не служебное слово (`social::slug::RESERVED`: `featured`, `new`, `by-slug`, `admin`, `create`, `edit`, `search`). Миграция `20261103200000_social_collection_slugs.sql` |
| `wallpaper_url`, `cover_url` | `text` | CHECK `collections_style_featured_check` (с `theme` и полями ниже): у не особой — `NULL` / значения по умолчанию | Оформление особой: обои (широкий фон шапки, 21:9 / 16:9, от 1920 px) и обложка карточки (16:9, от 1280 px) — https-URL до 2048 символов, как `image_url` баннеров (без upload). Миграция `20261105100000_social_collection_style.sql` |
| `theme` | `text` | `^[a-z0-9-]{1,32}$` (проверка в коде), без enum | id пресета оформления фронтенда (`default`, `halloween`, `romance`, `new-year`, `horror`, `sci-fi`, `family`, `summer`); неизвестный фронтенд игнорирует |
| `wallpaper_mode` | `text` | NOT NULL, default `header`, CHECK `header` / `page` | Где обои особой: только в шапке или фоном всей страницы. Миграция `20261106100000_social_collection_page_style.sql` |
| `wallpaper_dim` | `smallint` | CHECK 0–100 | Затемнение обоев, %; `NULL` — по теме на фронтенде |
| `wallpaper_blur` | `smallint` | NOT NULL, default 0, CHECK 0–20 | Размытие обоев, px |
| `effects` | `boolean` | NOT NULL, default `true` | Декоративные эффекты темы (капли крови, снег…) |
| `accent_color` | `text` | CHECK `^#[0-9a-f]{6}$` | Свой акцентный цвет поверх цвета темы; `NULL` — цвет темы |
| `header_mode` | `text` | NOT NULL, default `styled`, CHECK `styled` / `plain` / `none` | Шапка страницы особой: в теме и с обоями / нейтральная (без темы и обоев, остальное в теме) / без шапки (заголовок над списком). Миграция `20261107100000_social_collection_header_mode.sql` |
| `comments_locked` | `boolean` | NOT NULL, default `false` | Обсуждение закрыто: читать можно, писать — только admin. Меняют владелец (`PATCH /collections/{id}`) и admin (`PATCH /admin/collections/{id}`). Миграция `20261108100000_social_collection_comments.sql` |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |

**Индексы:** по `user_id` — коллекции пользователя; частичный по `created_at` для публичных —
лента новых коллекций; частичный (`user_id`, `created_at DESC`) для публичных — коллекции тех, на
кого подписан, в ленте (миграция `20261011100000_social_feed.sql`); частичный
`collections_featured_idx` (`featured_position`, `created_at`) для особых (миграция
`20261103100000_social_featured_collections.sql`); GIN `collections_title_trgm` (`title gin_trgm_ops`) — поиск в админке (миграция
`20261110100000_social_collections_title_trgm.sql`).

### `collection_links` ✅ — связанные подборки

Миграция `20261109100000_social_collection_links.sql` (2026-10-22). Какие коллекции admin показывает
рядом с данной («Похожие подборки») и в каком порядке. Связь направленная (обратную admin добавляет
сам); приоритет над будущим автоподбором.

| Колонка | Тип | Ограничения | Смысл |
|---|---|---|---|
| `collection_id` | `uuid` | PK, FK → `collections` CASCADE | Откуда связь |
| `related_id` | `uuid` | PK, FK → `collections` CASCADE; CHECK ≠ `collection_id` | На что ссылается |
| `position` | `integer` | NOT NULL | Порядок показа: всегда 0, 1, 2, … без пропусков (код перенумеровывает) |
| `created_at` | `timestamptz` | NOT NULL | |

**Индексы:** `collection_links_order_idx` (`collection_id`, `position`); `collection_links_related_idx`
(`related_id`). Не больше 12 связей у коллекции (`collection_links::MAX_LINKS`, проверка в коде).
Связывать можно только публичные; связанная, ставшая приватной, пропускается в выдаче (связь остаётся
и вернётся при открытии), удалённая уходит каскадом. Изменения — под `FOR UPDATE` строки коллекции.

### `collection_comments` ✅ — обсуждение коллекции

Миграция `20261108100000_social_collection_comments.sql` (2026-10-22). По образцу `review_comments`.

| Колонка | Тип | Ограничения | Смысл |
|---|---|---|---|
| `id` | `uuid` | PK, `uuidv7()` | |
| `collection_id` | `uuid` | NOT NULL, FK → `collections` CASCADE | Чьё обсуждение |
| `author_id` | `uuid` | NOT NULL, FK → `users` CASCADE | Автор |
| `parent_id` | `uuid` | FK → `collection_comments` CASCADE | Корневой комментарий той же коллекции (проверяет код); `NULL` — сам корневой. Один уровень: ответ на ответ привязывается к корню |
| `body` | `text` | NOT NULL, CHECK 1–2000 символов | |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер `set_updated_at` |
| `deleted_at` | `timestamptz` | | Мягкое удаление: из списка пропадает; удаление корня удаляет и его ответы |

**Индексы:** `collection_comments_collection_idx` (`collection_id`, `created_at`, `id`) WHERE не удалён —
лента и `comments_count`; `collection_comments_parent_idx` (`parent_id`) — ответы корня;
`collection_comments_author_idx` (`author_id`, `created_at DESC`) — лимит 5 комментариев в минуту.
`comments_count` коллекции — подзапрос (не колонка: триггер-счётчик трогал бы `updated_at` коллекции).
`collections.comments_locked` (`boolean`, NOT NULL, default `false`) — закрытое обсуждение. Событий нет.

### `collection_slug_history` ✅ — бывшие адреса особых коллекций

Миграция `20261103200000_social_collection_slugs.sql` (2026-10-21). Особая коллекция открывается по
читаемому адресу `/collections/{slug}` (обычные — по uuid). Бывший slug (смена адреса, снятие из
особых, удаление коллекции) остаётся здесь навсегда: старые ссылки не ломаются и не ведут на чужой
контент.

| Колонка | Тип | Ограничения | Смысл |
|---|---|---|---|
| `slug` | `text` | PK | Бывший адрес |
| `collection_id` | `uuid` | FK → `collections`, ON DELETE SET NULL | Чей был; `NULL` — коллекция удалена (адрес занят, по нему 404) |
| `created_at` | `timestamptz` | NOT NULL | Когда ушёл в историю |

Индекс `collection_slug_history_collection_idx` (`collection_id`). Триггер `collections_keep_slug`
(BEFORE DELETE ON `collections`) переносит slug удаляемой коллекции в историю с `collection_id = NULL`.
Занятость slug — текущий у другой коллекции **или** в истории не своей коллекции; выбор и запись —
под `pg_advisory_xact_lock('collection-slug')`.

### `collection_items` ✅ — содержимое коллекций

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `collection_id` | `uuid` | PK, FK → `collections`, CASCADE | |
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | Произведение |
| `position` | `integer` | NOT NULL, default 0 | Порядок в коллекции |
| `note` | `text` | | Комментарий автора к пункту |
| `added_at` | `timestamptz` | NOT NULL | |

**Индексы:** (`collection_id`, `position`) — коллекция по порядку; `entity_id` — в каких коллекциях
есть произведение.

### `forum_threads` ✅ — темы форума

Киллер-фича (см. [vision](../../documents/vision.md)): тема привязана сразу к нескольким произведениям, текст
темы — её первое сообщение. Миграция `20261009100000_social_forum.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `author_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | Автор |
| `title` | `text` | NOT NULL | Заголовок |
| `body` | `text` | NOT NULL | Текст темы |
| `is_locked` | `boolean` | NOT NULL, default `false` | Закрыта для ответов (решает admin) |
| `posts_count` | `integer` | NOT NULL, ≥ 0 | Видимые сообщения (без заглушек) |
| `last_post_at` | `timestamptz` | NOT NULL | Последнее сообщение; у новой темы — создание |
| `edited_at` | `timestamptz` | | Когда автор правил тему: пометка «изменено» |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер (меняют и счётчики) |

`posts_count` и `last_post_at` пишутся в той же транзакции, что и сообщение, под блокировкой строки
темы: списки не считают сообщения на лету.
**Индексы:** `created_at DESC` — свежие; `last_post_at DESC` — активные; (`author_id`,
`created_at DESC`) — темы пользователя; (`posts_count DESC`, `last_post_at DESC`, `id DESC`) —
популярные темы в ленте.

### `forum_thread_entities` ✅ — к каким произведениям привязана тема

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `thread_id` | `uuid` | PK, FK → `forum_threads`, CASCADE | |
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | Произведение |
| `position` | `smallint` | NOT NULL | Порядок показа: первая — главная |

Тема про «Дюну» = строки для книги и для фильмов. **Индексы:** PK; `entity_id` — «все темы про этот
фильм». Сущность, удалённая из каталога, пропадает из темы, сама тема остаётся.

### `forum_posts` ✅ — сообщения в темах

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `id` | `uuid` | PK | |
| `thread_id` | `uuid` | NOT NULL, FK → `forum_threads`, CASCADE | Тема |
| `author_id` | `uuid` | NOT NULL, FK → `users`, CASCADE | Автор |
| `parent_id` | `uuid` | FK (`thread_id`, `parent_id`) → (`thread_id`, `id`), `SET NULL (parent_id)` | Ответ на сообщение той же темы |
| `body` | `text` | `NULL` только у заглушки | Текст |
| `deleted_at` | `timestamptz` | задан ⇔ `body` = `NULL` | Удалено, но на него есть ответы |
| `edited_at` | `timestamptz` | | Пометка «изменено» |
| `created_at`, `updated_at` | `timestamptz` | NOT NULL | `updated_at` — триггер |

- **Ветки:** составной внешний ключ не даёт ответить на сообщение другой темы. Глубина не
  ограничена.
- **Удаление:** сообщение без ответов удаляется целиком, с ответами — становится заглушкой
  (`body` = `NULL`, `deleted_at`), чтобы ветка не рвалась. Заглушка, у которой не осталось
  ответов, удаляется следом (и так вверх по ветке). Если удалён сам пользователь (каскад), ответы
  на его сообщения становятся сообщениями верхнего уровня.
- **Индексы:** `UNIQUE (thread_id, id)` (цель составного ключа); (`thread_id`, `created_at`) —
  сообщения темы по порядку; частичный по `parent_id` — есть ли ответы; частичный по `author_id`
  для неудалённых — счётчик в профиле.

### `user_interests` ✅ — интересы

На какие произведения подписан пользователь. По ним `realtime` сам подписывает соединения
пользователя на каналы сущностей, позже из них строится лента. Таблица в `social` (а не в `auth`):
она ссылается на `entities` и нужна социальным функциям. Миграция `20261010100000_social_interests.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `user_id` | `uuid` | PK, FK → `users`, CASCADE | Кто |
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | Что интересует |
| `created_at` | `timestamptz` | NOT NULL | Когда добавлено |

**Индексы:** PK; (`user_id`, `created_at DESC`) — свои интересы, новые сверху; `entity_id` — кто
интересуется сущностью (лента, рассылки). Не больше 500 интересов на пользователя.

### `chat_rooms`, `chat_messages`, `chat_reads`, `chat_mutes` ✅ — чат по интересам

Комната на каждую сущность (`entity:<slug>`), человека (`person:<slug>`) и тег (`tag:<slug>`)
каталога. Отправка и «прочитано» — по WebSocket
(`chat.send` / `chat.read`, см. realtime.md), история и настройки — REST. Миграция
`20261030100000_social_chat.sql` (этап 1, сущности) и `20261031100000_social_chat_people_tags.sql`
(этап 2, люди и теги), обе 2026-10-20. Сообщения хранятся навсегда.

| Таблица | Колонки | Смысл |
|---|---|---|
| `chat_rooms` | `id` PK uuidv7; `entity_id` UNIQUE FK → `entities` CASCADE, `person_id` UNIQUE FK → `people` CASCADE, `tag_id` UNIQUE FK → `tags` CASCADE — ровно одно (CHECK `chat_rooms_one_target`: `num_nonnulls(...) = 1`); `messages_count` int (0); `last_message_id`, `last_message_at`; `created_at`, `updated_at` (триггер `set_updated_at`) | Комната. Создаётся при первом сообщении или заглушении (`INSERT … ON CONFLICT (<колонка>) DO UPDATE … RETURNING id` — строка блокируется до конца транзакции: счётчик и последнее сообщение меняются по очереди) |
| `chat_messages` | `id` PK uuidv7; `room_id` FK → `chat_rooms` CASCADE; `author_id` FK → `users` CASCADE; `reply_to` FK → `chat_messages` SET NULL; `body` text; `created_at`, `edited_at`, `deleted_at`. CHECK `deleted_at IS NOT NULL OR char_length(body) BETWEEN 1 AND 2000` | Сообщение. id растёт со временем — по нему курсор истории и прочитанное. Удаление мягкое: `deleted_at`, `body = ''` (заглушка в истории) |
| `chat_reads` | PK (`user_id`, `room_id`), оба FK CASCADE; `last_read_id` uuid; `updated_at` | Прочитано до сообщения; назад не двигается (`GREATEST`). Своё сообщение сразу прочитано |
| `chat_mutes` | PK (`user_id`, `room_id`), оба FK CASCADE; `created_at` | Заглушённые комнаты: события приходят, фронтенд не показывает уведомления |

**Индексы:** `chat_messages_room_idx` (`room_id`, `id DESC`) — история; `chat_messages_author_idx`
(`author_id`, `created_at DESC`) — лимит частоты.

**Правила:** текст 1–2000 символов (`MAX_CHAT_BODY`, обрезаются пробелы); не больше 5 сообщений
автора за 10 с (`RATE_MESSAGES`, `RATE_WINDOW_SECS`; счёт по `chat_messages`, работает и на
нескольких экземплярах) — иначе 429; `reply_to` — неудалённое сообщение той же комнаты;
непрочитанные — чужие неудалённые сообщения новее `last_read_id`, считаются до 100 (`MAX_UNREAD`).

### Свои комнаты админа и `chat_room_members` ✅ — чат, этап 3

Миграция `20261101100000_social_chat_rooms.sql` (2026-10-20). Своя комната — строка `chat_rooms` с
`slug` (UNIQUE, a-z/0-9/дефисы, 1–80: канал `room:<slug>`), `title` (1–100), `description` (до 500),
`created_by` FK → `users` SET NULL; CHECK `chat_rooms_one_target` теперь
`num_nonnulls(entity_id, person_id, tag_id, slug) = 1`, CHECK `chat_rooms_custom_title` — `title` есть
ровно у своих. У своей комнаты `target_id` в сообщениях и событиях — id самой комнаты.

| Таблица | Колонки | Индексы |
|---|---|---|
| `chat_room_members` | PK (`user_id` FK → `users` CASCADE, `room_id` FK → `chat_rooms` CASCADE), `created_at` | (`user_id`, `created_at DESC`); `room_id` — число участников |

Вступление — как интерес: комната в «моих чатах», канал realtime подписывается сам
(`InterestDirectory::room_ids`). Писать может любой вошедший (как в остальных комнатах).

### Системные комнаты, свои комнаты пользователей и жалобы ✅ — чат, этап 4

Миграция `20261111100000_social_chat_public_rooms.sql` (2026-10-23). `chat_rooms` получает
`system boolean NOT NULL DEFAULT false` и `owner_id` FK → `users` SET NULL; CHECK
`chat_rooms_system_custom` — системная всегда со `slug` и без `owner_id`. Индексы:
`chat_rooms_owner_idx` (`owner_id`, `created_at DESC`) WHERE `owner_id IS NOT NULL` — лимиты
владельца; `chat_rooms_system_idx` (`id`) WHERE `system` — их подписывает каждое соединение.

**Системная комната** (`system = true`) — вкладка панели чата: участник в ней каждый
пользователь, строк в `chat_room_members` для неё нет. Поэтому `members_count` берётся из
`UserDirectory::count()` (`chat::fill_rooms`), `room_ids` возвращает системные всем (канал
`room:<slug>` подписан без `subscribe`), в `GET /chats` они идут первыми в порядке заведения, а
`me.member` / `joined` у вошедшего всегда `true`. Создать, покинуть, изменить, удалить и
пожаловаться — 403 `room_is_system`; заводятся только миграцией. Гость читает их как любую
комнату. Пять штук: `general`, `movies`, `series`, `books`, `games`.

**Десять основных комнат** заведены той же миграцией как обычные (`system = false`,
`owner_id = NULL`), из них можно выйти: `releases`, `what-to-watch`, `what-to-read`, `classics`,
`scifi-fantasy`, `horror`, `detectives`, `anime`, `indie`, `offtop`.

**Своя комната пользователя**: `POST /chat/rooms` ставит `owner_id = created_by = автор` и сразу
пишет его в `chat_room_members` (+ `interest.added`). Владелец правит и удаляет её сам
(`PATCH` / `DELETE /chat/rooms/{slug}`), админ — любую несистемную. У комнат админа `owner_id`
пуст. Лимиты (`rooms::MAX_OWNED_ROOMS` = 5 живых, `MAX_ROOMS_PER_DAY` = 3 в сутки, одним
запросом по `chat_rooms` владельца): суточный → 429 с `Retry-After` до выхода самой старой из
окна 24 ч, живых → 409 `room_limit` (ждать бесполезно, нужно удалить свою).
`rooms::RESERVED_SLUGS` (`general`, `movies`, `series`, `books`, `games`, `admin`, `api`, `new`,
`search`, `chat`) → 409 `slug_taken`; админу они доступны, системные закрыты UNIQUE.

| Таблица | Колонки | Индексы |
|---|---|---|
| `chat_room_reports` | PK (`room_id` FK → `chat_rooms` CASCADE, `user_id` FK → `users` CASCADE), `reason` text, `created_at` | `room_id` — число жалоб |

Жалоба одна на пользователя: повторная — `ON CONFLICT DO UPDATE` причины и `created_at`, дубля
нет. Админский разбор — `GET /admin/chat/rooms/reports`.

### Вложения в сообщениях ✅ — чат, этап 9

Миграция `20261116100000_social_chat_attachments.sql` (2026-10-24). Только картинки.

| Таблица | Колонки | Индексы |
|---|---|---|
| `chat_attachments` | `id` PK uuidv7; `user_id` FK → `users` CASCADE — загрузивший; `message_id` UNIQUE FK → `chat_messages` CASCADE (NULL — загружено, но ещё не отправлено); `width`, `height`, `size`; `created_at` | `created_at` WHERE `message_id IS NULL` — уборка сирот; (`user_id`, `created_at DESC`) — лимит частоты |

CHECK тела сообщения ослаблен до `char_length(body) <= 2000`: с вложением текст необязателен,
а CHECK не видит другую таблицу — пустое тело разрешает обработчик, только когда есть
`attachment_id`.

**Два шага, как в мессенджерах.** `POST /chat/attachments` (multipart) заливает файл и
возвращает `id`, затем `chat.send` передаёт `attachment_id`. Так прогресс загрузки виден
отдельно от отправки и можно отправить одну картинку без текста. Привязка — `attachments::attach`
внутри транзакции отправки: чужое или уже использованное вложение откатывает всю отправку
(400 `unknown_attachment`).

**Присланный файл не сохраняется никогда.** `attachments::reencode` декодирует его (`image`),
ограничивает длинную сторону `MAX_SIDE` = 1600 и кодирует в WebP. Отсюда три следствия:
подтверждается, что это действительно картинка (а не исполняемый файл с расширением `.png`),
исчезают EXIF и GPS из снимка, и на диск ложится файл в разы меньше присланного. Размеры
читаются из заголовка **до** декодирования и отклоняются при стороне больше
`MAX_SOURCE_SIDE` = 10000 — защита от «архивных бомб» (картинка 30000×30000 весит килобайты, а
в памяти развернулась бы в гигабайты). Пережатие уходит в `spawn_blocking`: оно упирается в
процессор. Имя файла от клиента не используется нигде — ни в пути, ни в ответе.

**Лимиты:** `MAX_UPLOAD_BYTES` = 2 МБ на вход (`DefaultBodyLimit` на этом маршруте — чуть выше,
чтобы на превышении отвечала наша ошибка `file_too_large`, а не 413 от фреймворка),
`RATE_UPLOADS` = 10 загрузок за `RATE_WINDOW_SECS` = 600 с.

**Раздаёт приложение, не nginx** (`GET /chat/attachments/{id}`): доступ такой же, как к
сообщению, в котором лежит вложение (`chat::visible_message`), то есть вложение из личной
переписки не открывается посторонним по прямой ссылке. Пока вложение никуда не приложено, его
видит только загрузивший. Заголовки ответа: `image/webp`, `nosniff`, `inline`, `immutable` —
id новый на каждую загрузку, поэтому файл можно кэшировать навсегда.

**Файлы на диске:** `ATTACHMENTS_DIR` (по умолчанию `data/attachments`, в docker —
`/var/lib/nexus/attachments` на томе архива), раскладка `<первые 2 символа id>/<id>.webp` —
чтобы в одной директории не оказались сотни тысяч файлов. Удаление сообщения снимает строку в
той же транзакции, файл стирается после коммита. Загруженное и не отправленное старше
`ORPHAN_LIFETIME_HOURS` = 1 убирает `cleanup_orphans` при старте приложения.

### Личные переписки, блокировки и `dm_policy` ✅ — чат, этап 8

Миграция `20261115100000_social_chat_dm.sql` (2026-10-24). Переписка — строка `chat_rooms`
того же протокола, что и остальные комнаты: канал `dm:<id>`, та же история, та же отправка по
WebSocket, те же `chat.message.*`. `chat_rooms` получает `dm_user_a` / `dm_user_b` (FK → `users`
CASCADE); CHECK `chat_rooms_one_target` теперь
`num_nonnulls(entity_id, person_id, tag_id, slug, dm_user_a) = 1`, CHECK `chat_rooms_dm_pair` —
собеседника либо два, либо ни одного, и порядок канонический (`dm_user_a < dm_user_b`), поэтому
на двоих ровно одна строка и повторный `PUT` возвращает её же. Индексы: UNIQUE
`chat_rooms_dm_idx` (пара), `chat_rooms_dm_a_idx` / `chat_rooms_dm_b_idx` (мои переписки, без OR
по индексам), `chat_rooms_dm_started_idx` (`created_by`, `created_at DESC`) — суточный лимит.

`ChatRoom::Dm(id)` и `Channel::Dm(id)` в `shared`; `kind = dm`, `target_id` = id самой комнаты,
как у своих комнат. Имя канала — просто id, читать для него ничего не нужно.

**Доступ.** Это единственная комната, которую видят не все: только её двое. Проверяет
`dm::visible` внутри `chat::room_by_slug`, поэтому история, панель и заглушение закрыты разом;
отправку закрывает `dm::peer_of` в `PgChat::send`. Для всех остальных, включая гостя, переписки
не существует — **404, а не 403**: иначе ответ подтверждал бы, что двое переписываются.
`realtime` не даёт подписаться на чужой `dm:` (`Chat::dm_member`), а свои подписывает сам
(`Chat::dm_room_ids`, имя канала — id) — и при `auth`, и на лету по `dm.created`, как интересы.
Личные переписки доступны только вошедшим: гостю `dm::visible` отдаёт 404.

**Убрать у себя** — та же `chat_room_hidden`, что у системных комнат: `DELETE /chat/dm/{id}`
прячет строку из `GET /chats`, история цела, у собеседника она остаётся. Новое сообщение
снимает скрытие у получателя (в `PgChat::send`), повторный `PUT .../dm` — у отправителя.

| Таблица | Колонки | Индексы |
|---|---|---|
| `user_blocks` | PK (`user_id`, `blocked_id`), оба FK → `users` CASCADE; `created_at`; CHECK `user_blocks_not_self` | `blocked_id` — «кто меня заблокировал» |
| `user_chat_settings` | `user_id` PK FK → `users` CASCADE; `dm_policy` text (`all` \| `following` \| `none`, по умолчанию `all`); `created_at`, `updated_at` (триггер) | — |

**Блокировка** односторонняя и не уведомляет, но личные сообщения закрывает в обе стороны
(`dm::check_not_blocked`, зовётся и при открытии, и на каждой отправке — значит, действует и на
уже открытую переписку). Общие каналы не трогает. Строки `user_chat_settings` заводятся по
требованию: её отсутствие равно значениям по умолчанию.

**Кто пишет первым** (`dm::DmPolicy`, проверяется только при создании переписки): `all` — любой
вошедший **с подтверждённым email** (`UserDirectory::is_verified`, иначе 403
`email_not_verified`), `following` — только те, на кого собеседник подписан сам, `none` — никто
(403 `dm_closed`). Плюс `dm::MAX_NEW_DMS_PER_DAY` = 20 новых переписок в сутки на пользователя
(429 с `Retry-After`); ответ в существующую под лимит не попадает.

**Поиск людей** `GET /users?q=` исключает обе стороны блокировки: `dm::hidden_from` собирает
список в `social`, а `UserDirectory::search` принимает его параметром `exclude` — блокировки
живут в `social`, и `auth` о них не знает.

### Скрытые комнаты в «Моих чатах» ✅ — чат, этап 7

Миграция `20261114100000_social_chat_hidden_rooms.sql` (2026-10-23): таблица
`chat_room_hidden` (PK `user_id` + `room_id`, оба FK CASCADE, `created_at`).

Из системной комнаты нельзя выйти — в ней состоят все, — но держать все пять в списке
пользователь не обязан: открываются они вкладками. Заглушение не помогает, оно убирает
уведомления, а строка остаётся. `PUT` / `DELETE /chat/rooms/{slug}/hidden` убирает и
возвращает строку.

Скрытая комната **не приходит в `GET /chats`** (фильтр в системной ветке CTE `mine`) и больше
ничем не отличается: история, отправка, `InterestDirectory::room_ids` и подписка на канал
`room:<slug>` не меняются — иначе пропали бы упоминания. `CustomRoom.hidden` (рядом с
`joined`, гостю `null`) показывает состояние в `GET /chat/rooms`, `GET /chat/rooms/{slug}` и
`GET /chat/{kind}/{slug}`; в самих «Моих чатах» он всегда `false` — скрытых там нет.

Эндпоинт только для системных комнат: из остальных можно просто выйти, и скрытая несистемная
комната исчезла бы из списка без способа вернуть её из интерфейса — поэтому 400.

**Реалтайм каталога.** Создание комнаты публикует `chat.room.created` в общий канал `rooms`
(`shared::events::Channel::Rooms`, имя канала — `rooms`, подписка только вручную через
`subscribe`: автоматически он никому не выдаётся, он нужен лишь открытой странице со списком
комнат). Туда же, кроме канала самой комнаты, уходят `chat.room.updated` и `chat.room.deleted`
— открытый список обновляет строку, не перезагружаясь.

### Активность комнат и упоминания ✅ — чат, этап 5

Миграция `20261112100000_social_chat_activity.sql` (2026-10-23): индекс
`chat_messages_room_recent_idx` (`room_id`, `created_at DESC`) WHERE `deleted_at IS NULL`.

`CustomRoom` += `messages_24h` (сообщений за сутки) и `last_message_at`. Счётчик **не**
материализован: он считается подзапросом в `CUSTOM_COLUMNS` по этому индексу — диапазон на
комнату. Колонку-счётчик пришлось бы чем-то обнулять по истечении суток (cron или пересчёт при
чтении), а комнат пока десятки, поэтому подзапрос дешевле и всегда точен. Когда комнат станут
тысячи, менять надо здесь: `messages_24h` в `ORDER BY` считается для всех комнат выборки.

`GET /chat/rooms?sort=active|members|new` (`rooms::RoomSort`, по умолчанию `active`): системные
всегда первыми, дальше — `messages_24h` убыв. + `last_message_at` убыв. (`active`),
`members_count` убыв. (`members`) или `created_at` убыв. (`new`).

**Упоминания.** `chat::mentions` разбирает `@username` в теле сообщения: `@` считается началом
упоминания, только если перед ним не буква и не цифра (`mail@example.com` — не упоминание), имя
— `[a-zA-Z0-9_]` до 32 символов, дубли схлопываются, регистр не важен. После
`chat.message.created` всех названных ищет один `UserDirectory::by_usernames`, и каждому
существующему, кроме автора, уходит `chat.mention` в его `user:`-канал. Непрочитанное считается
как прежде — упоминание его не трогает.

### Поиск по чату и по пользователям ✅ — чат, этап 6

Миграция `20261113100000_social_chat_search.sql` (2026-10-23). Всё на PostgreSQL, не в
Meilisearch: комнат десятки, а сообщения ищутся всегда внутри ограниченного набора комнат, так
что отдельный индекс и его инкрементальное обновление в горячем пути `chat.send` не окупаются
(решение пользователя, 2026-10-23). Индексы: `chat_rooms_title_trgm`,
`chat_rooms_description_trgm` (gin_trgm_ops), `chat_messages_body_fts`
(`to_tsvector('russian', body)` WHERE `deleted_at IS NULL`), `users_display_name_trgm`
(username уже покрыт `users_username_trgm`).

**Комнаты** — `GET /chat/rooms?q=` (`rooms::MAX_ROOM_QUERY` = 100): `ILIKE '%q%'` по `title` и
`description`, порядок — `similarity(title, q)` убыв. При `q` сортировка `?sort` и «системные
первыми» не действуют: там порядок задаёт близость к запросу. `%`, `_` и `\` в запросе —
буквальные символы (`rooms::like_pattern`).

**Сообщения** — `GET /chat/search` (`chat::MAX_CHAT_QUERY` = 100): `websearch_to_tsquery` по
русской конфигурации, то есть по словам с морфологией («дюна» находит «Дюну»), а не по
подстроке. Область: с `room=<kind>:<slug>` — одна комната (доступно гостю, как и её история),
без `room` — все мои чаты (интересы всех трёх видов + свои комнаты + системные), и тогда нужен
вход. Обе ветки SQL упоминают оба параметра области — Postgres не принимает привязку
неиспользованного. Удалённые не находятся (их нет в частичном индексе). Курсор — `before` по
`id` убыв., как в истории; `next_before` считается по лишней прочитанной строке.

**Пользователи** — `GET /users?q=` (`profiles::MAX_USER_QUERY` = 50, `limit` 1–50, по умолчанию
8): автодополнение «@» в чате. Данные — через `UserDirectory::search` (таблица `users`
принадлежит `auth`): `ILIKE` по username и `display_name`, сверху те, чей username начинается с
запроса, дальше по `similarity`. Только активные.

### `user_person_interests`, `user_tag_interests` ✅ — интересы к людям и тегам

Миграция `20261031100000_social_chat_people_tags.sql` (2026-10-20). Как `user_interests`, но для
людей (автор, режиссёр, актёр) и тегов/жанров каталога: из них — каналы realtime `person:` / `tag:`
и комнаты в «моих чатах». Не больше 500 каждого вида (`MAX_INTERESTS`).

| Таблица | Колонки | Индексы |
|---|---|---|
| `user_person_interests` | PK (`user_id` FK → `users` CASCADE, `person_id` FK → `people` CASCADE), `created_at` | (`user_id`, `created_at DESC`); `person_id` — `people_in_use` |
| `user_tag_interests` | PK (`user_id` FK → `users` CASCADE, `tag_id` FK → `tags` CASCADE), `created_at` | (`user_id`, `created_at DESC`) |

### `entity_marks` ✅ — личные отметки

«Посмотреть позже» (`later`) и «Просмотрено» (`done`); у книг — «прочитать», у игр — «пройти»
(подписи — на фронтенде). Миграция `20261014140000_entity_marks.sql`.

| Колонка | Тип | Ограничения | Описание |
|---|---|---|---|
| `entity_id` | `uuid` | PK, FK → `entities`, CASCADE | |
| `user_id` | `uuid` | PK, FK → `users`, CASCADE | |
| `kind` | `text` | NOT NULL, `later` / `done` | |
| `created_at` | `timestamptz` | NOT NULL | Когда поставлена эта отметка; смена `later` → `done` — заново |
| `updated_at` | `timestamptz` | NOT NULL | Триггер |

Одна строка на пару: отметки взаимоисключающие. **Индексы:** PK; (`user_id`, `kind`,
`created_at DESC`) — свои списки.

</tables>

<deps>

## Чужие данные: справочники

`social` не читает таблицы `entities` (catalog) и `users` (auth), хотя ссылается на них внешними
ключами. Название и slug сущности, имя автора, проверка «сущность существует» — через явный
интерфейс `shared::directory` (правило 4 из [«Правил границ»](../../documents/architecture.md#правила-границ)):

| Трейт | Реализует | Методы |
|---|---|---|
| `EntityDirectory` | `catalog::directory::PgEntityDirectory` | `by_slug` → `EntityRef`, `by_ids` → `HashMap<Uuid, EntityRef>`, `by_slugs` → `HashMap<String, EntityRef>`, `overviews(ids)` → `HashMap<Uuid, String>` (краткие описания для детали коллекции), `kind_counts(ids)` → `HashMap<String, i64>` (сколько сущностей каждого типа среди переданных — разрез аналитики профиля по `movie`/`series`/`book`/`game` одним запросом вместо выборки всех `EntityRef`). По slug — только видимые сущности каталога (активные; фильмы из TMDB — вышедшие или «Скоро премьера», либо включённые вручную): невидимая (её нет на сайте) — как неизвестная, эндпоинты по slug сущности отвечают 404, а в `/ratings` её нет. По id — все: коллекции, ленты и интересы со скрытой сущностью её не теряют |
| `UserDirectory` | `auth::directory::PgUserDirectory` | `by_username` (только активные) → `UserRef`, `by_ids` (включая заблокированных), `by_usernames` (пакетно, для `@упоминаний`), `search` (по username и имени, с `exclude` — блокировки передаёт `social`), `is_verified` (подтверждён ли email — право начать переписку), `details(id)` → `UserDetails` `{bio, stats_public, created_at}` (поля страницы профиля: в `UserRef` им не место — он стоит рядом с каждой рецензией), `count` (активных — `members_count` системных комнат чата) |
| `EntityRatings` | `catalog::directory::PgEntityRatings` | `set`, `replace_all` — `social` сообщает каталогу сводки оценок для сортировок `popular` и `rating` (`social::ratings`: после записи рецензии и при старте) |

В обратную сторону `social` сам реализует `InterestDirectory` (`social::directory::PgInterestDirectory`):
`entity_ids(user)` — по нему `realtime` узнаёт интересы, не читая `user_interests`;
`entities_in_use(ids)` → `HashSet<Uuid>` — какие из сущностей есть в `user_interests`, `reviews`,
`collection_items`, `entity_marks`, `entity_reactions`, `forum_thread_entities` (один `UNION`-запрос
на пачку): `catalog` не переносит такие в холодный архив, иначе удаление строки `entities` стёрло бы
их каскадом.

`social` реализует и `Chat` (`social::chat::PgChat`, `state.chat`): `send(state, user, ChatRoom,
body, reply_to)` → сообщение (JSON), `read(state, user, ChatRoom, message_id)`. Его вызывает
`realtime` на `chat.send` / `chat.read` — модули не знают друг о друге, запись в таблицы чата
остаётся у `social`, событие `social` публикует сам после `commit`. `ChatRoom::{Entity, Person, Tag, Room}(id)`; для realtime ещё `room_by_slug(slug)`, `room_slugs(ids)` — slug своих комнат.

Студии — `StudioDirectory` (`state.studios`, реализует `catalog`): `by_slug`, `by_ids` → `StudioRef` `{id, slug, name, logo_url}`; по нему `social` проверяет цель комментария `studio:<slug>` и показывает, к какой студии комментарий оставлен.

Люди и теги — справочники `PersonDirectory` (`state.people`) и `TagDirectory` (`state.tags`),
реализует `catalog` (`by_slug`, `by_ids` → `PersonRef` `{id, slug, full_name, photo_url}`, `TagRef`
`{id, slug, name, kind}`). `InterestDirectory` дополнен: `person_ids(user)`, `tag_ids(user)`, `room_ids(user)` (вступил) —
автоподписка realtime; `people_in_use(ids)` — люди в `user_person_interests` или с комнатой
`chat_rooms.person_id`: `catalog` не удаляет их при переносе в архив.

- `EntityRef` — `id`, `kind`, `slug`, `title`, `cover_url`, `release_date` (по году различаются «Дюна» 1984 и 2021); `UserRef` — `id`, `username`,
  `display_name`, `avatar_url`. Это же их вид в ответах API.
- **Подключение:** `AppState::new(..., Directories { entities, users, interests, ratings, chat, people, tags })`; реализации собирает
  `nexus::directories(pool)` в `app` (и `test_utils::directories` в тестах). Модули по-прежнему
  не зависят друг от друга: `social` знает только трейты из `shared`.
- **Без N+1:** хендлер выбирает свои строки, собирает id и одним вызовом `by_ids` получает ссылки
  на сущности, одним — на пользователей (`modules/social/src/refs.rs`).
- **Подмена в тестах:** `state.entities = Arc::new(Fake)`; пример — тест
  `social_reads_entities_through_directory` в `app/tests/social.rs`.
- **Цена:** сортировать и фильтровать по названию сущности или имени автора `social` не может.
  Если понадобится — отдельный метод справочника или денормализация.

</deps>

<endpoints>

## Эндпоинты

Все пути — от `/api/v1/social`. В Swagger (http://localhost/docs) — отдельная вкладка
**Социальное** (`/api-docs/social.json`, контракт — `documents/api/social.json`). Токен берётся во
вкладке **Nexus API**.

Адресация: сущность — по `slug` (как в каталоге), пользователь — по `username` (без учёта
регистра), коллекция, тема и сообщение — по `id`. Неизвестная сущность и неизвестный или заблокированный
пользователь — 404. Ошибки — `{"error": "...", "code": "..."}` (коды — [for-other-modules.md](../../documents/for-other-modules.md#авторизация-в-хендлере)). Пагинация — как в каталоге:
`?limit=20&offset=0` (1–100), ответ `{items, total, limit, offset}` (`shared::pagination::Page`).

### Комментарии к произведениям, людям и студиям

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/comments?target=entity:dune-2021` | все | Комментарии цели: `Page<TargetComment>` (`?limit=1..100&offset=`), старые сверху (`created_at`, `id`), ответы — в той же ленте с `parent_id`. `target` — `entity:<slug>`, `person:<slug>` или `studio:<slug>`. `TargetComment`: `id`, `target`, `parent_id`, `author` (`UserRef`), `body`, `created_at`, `updated_at`. 400 `validation` `field: target` — не вида `kind:slug` или неизвестный вид; 404 — нет такой цели |
| POST | `/comments` | user | `{"target", "body", "parent_id"?}` (лишние поля — 400) → 201 `TargetComment`. Ответ на ответ привязывается к корню. 400 `validation` с `field`: `target`, `body` (1–2000 после обрезки), `parent_id` (нет неудалённого комментария **у этой цели**); 404 — нет цели; 429 — больше 5 комментариев в минуту (`Retry-After: 60`) |
| DELETE | `/comments/{id}` | автор | Удалить свой (корневой — вместе с ответами) → 204; 403 — чужой; 404 — нет или уже удалён |
| DELETE | `/admin/comments/{id}` | admin | То же для любого комментария |

### Реакции на коллекцию

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/collections/{id}/reactions` | гость / вошедший | Сводка: `{"reactions": {"fire": 2, "sad": 0, "think": 1, "bored": 0}, "mine": "fire" \| null}` (`mine` — гостю `null`). 404 — нет коллекции или чужая приватная |
| PUT | `/collections/{id}/reaction` | user | `{"kind": "fire" \| "sad" \| "think" \| "bored"}` → 200, другая заменяет прежнюю. 400 — неизвестная; 404 — нет коллекции или чужая приватная |
| DELETE | `/collections/{id}/reaction` | user | Убрать → 204 (даже если не было) |

### Профиль

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/users` | все | Поиск пользователей для автодополнения «@»: `?q=` (1–50, по username и имени), `?limit=` 1–50 (8) → `[UserRef]`, только активные. 400 — пустой/длинный `q`, `limit` вне 1–50 |
| PUT | `/users/{username}/like` | user | Лайкнуть профиль → 204 (повторно — тоже 204). 400 `like_self` — себе; 404 — нет пользователя |
| DELETE | `/users/{username}/like` | user | Снять лайк → 204 (даже если его не было) |
| GET | `/users/{username}/likes` | все | Кто лайкнул: `Page<{user, since}>` (`?limit=1..100&offset=`), новые сверху. 404 — нет пользователя |
| GET | `/users/{username}/comments` | гость / вошедший | «Мои комментарии»: всё, что пользователь написал — к произведениям, людям и студиям (`comments`), к рецензиям и в обсуждениях коллекций — одной лентой, новые сверху. `Page<UserComment>` (`?limit=1..100&offset=`); `UserComment`: `id`, `body`, `created_at`, `updated_at` и `target` с `kind` — `entity`/`person`/`studio` (рядом `entity`/`person`/`studio`), `review` (`review_id` и `entity`, о котором рецензия) или `collection` (`collection_id`, `title`). Цель удалена — строки в ленте нет. 403 `stats_private`; 404 — нет пользователя |
| GET | `/users/{username}/stats` | гость / вошедший | Аналитика профиля: `ratings` (`average`, `count`, `distribution` 1–10), `reviews_with_text`, `marks` (`later`, `done`), `reactions` (все четыре ключа), `comments` (`reviews`, `collections`, без удалённых) и `by_kind` (`rated`, `done` по типам контента — из `EntityDirectory::kind_counts`, ключей без единой сущности нет). 403 `stats_private` — владелец не открыл аналитику; 404 — нет пользователя |
| GET | `/users/{username}/ratings` | гость / вошедший | Что пользователь оценил: `Page<{entity, rating, has_text, created_at}>`, `?sort=new` (по умолчанию) `\|rating_desc\|rating_asc`, `?limit=1..100&offset=`. 400 — неизвестный `sort`; 403 `stats_private`; 404 — нет пользователя |
| GET | `/users/{username}/marks` | гость / вошедший | Отметки пользователя: `?kind=later\|done` (не передан — все), `limit`, `offset` → `Page<{entity, kind, created_at}>`. Своё без оглядки на приватность — `/users/me/marks`. 403 `stats_private`; 404 — нет пользователя |
| GET | `/users/{username}` | гость / вошедший | Шапка профиля: `user`, `followers_count`, `following_count`, `reviews_count`, `collections_count` (владельцу — с приватными), `threads_count`, `posts_count` (сообщения форума без удалённых), `likes_count` и `liked` (лайкнул ли вошедший; гостю и в своём профиле `false`), `relation`, `blocked` (я заблокировал его; гостю и в своём профиле `false`), `blocked_by` (он заблокировал меня; `null` — гость или свой профиль), а также `bio`, `stats_public` (открыл ли пользователь свою аналитику всем) и `registered_at` — из `auth` через `UserDirectory::details` |

**Приватность аналитики.** `/users/{username}/stats`, `/ratings` и `/marks` по умолчанию видит
только владелец (и admin). Чужому и гостю — 403 `stats_private`, а не 404: профиль существует и
открыт, закрыта именно статистика. Открывает её сам владелец — `stats_public: true` в
`PATCH /api/v1/auth/me`; `stats_public` виден в шапке профиля, так что фронтенд знает, показывать
ли вкладки, не получая 403.

`relation` — `{"following": true, "followed_by": false}`: подписан ли вошедший на пользователя и
пользователь на него. Для гостя и своего профиля — `null`. Так фронтенд рисует кнопку
«Подписаться» / «Вы подписаны» без перебора списков подписок.

### Рецензии и оценки

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/entities/{slug}/reviews` | гость | Рецензии на сущность. По умолчанию только с текстом; `all=true` — и оценки без текста. `sort`: `new` (по умолчанию), `rating_desc`, `rating_asc` (без оценки — в конце) |
| GET | `/entities/{slug}/rating` | гость | Сводка: `{"average": 8.3, "count": 12, "distribution": [n1, ..., n10], "reactions": {"fire": 12, "sad": 3, "think": 5, "bored": 1}}`. Среднее округлено до 0.1, без оценок — `null`. Учитываются все оценки, в том числе без текста; в `reactions` всегда все четыре ключа |
| GET | `/ratings?slugs=a,b,c` | гость | Средние оценки пачкой для сеток каталога (до 100 slug'ов): `{"dune-2021": {"average": 8.3, "count": 12}}`. Без оценок — `{"average": null, "count": 0}`, неизвестных slug'ов в ответе нет; пусто или больше 100 — 400 |
| GET | `/entities/{slug}/review` | user | Своя рецензия, нет — 404 |
| PUT | `/entities/{slug}/review` | user | `{"rating": 1–10, "body": "...", "is_spoiler": false}`: создать (201) или заменить целиком (200). Нужна оценка или текст; `is_spoiler` по умолчанию `false`. До выхода (`release_date` позже сегодняшней даты UTC) — 409 `not_released`; без даты — можно |
| DELETE | `/entities/{slug}/review` | user | Удалить свою → 204, нет — 404. Можно и до выхода (премьеру перенесли) |
| GET | `/users/{username}/reviews` | гость | Рецензии пользователя, включая оценки без текста, новые сверху |
| PUT | `/reviews/{id}/vote` | user | `{"value": 1 \| -1}` — лайк или дизлайк; другой `value` меняет голос → 200 `{"likes_count", "dislikes_count", "value"}`. За свою рецензию — 403 `own_review` |
| DELETE | `/reviews/{id}/vote` | user | Снять голос → 200, `value: null` (голоса не было — тоже 200) |
| GET | `/reviews/{id}/comments` | гость | Комментарии к рецензии, старые сверху: `{id, review_id, author, body, created_at, updated_at}` |
| POST | `/reviews/{id}/comments` | user | `{"body": "..."}` (1–2000 символов) → 201. Оценку без текста — 409 `review_without_text` |
| DELETE | `/review-comments/{id}` | автор комментария | → 204; чужой — 403, уже удалён — 404 |
| GET | `/entities/{slug}/reaction` | user | Своя реакция `{"kind": "fire"}`, нет — 404 |
| PUT | `/entities/{slug}/reaction` | user | `{"kind": "fire" \| "sad" \| "think" \| "bored"}` → 200 `{"kind"}`, другая заменяет прежнюю. До выхода — 409 `not_released` |
| DELETE | `/entities/{slug}/reaction` | user | Убрать → 204 (даже если не было) |
| GET | `/entities/{slug}/votes/mine` | user | Свои голоса за рецензии сущности: `{"<id рецензии>": 1 \| -1}` — один запрос на карточку |

Рецензия в ответе: `id`, `author` (`UserRef`), `entity` (`EntityRef`), `rating`, `body`,
`is_spoiler`, `likes_count`, `dislikes_count`, `comments_count`, `created_at`, `updated_at`.

**Личное — отдельными запросами.** Публичные списки (рецензии, сводка, комментарии) одинаковы для
всех и читаются без токена — их можно кэшировать. Поэтому «мой голос», «моя реакция», «моя
отметка» в них не встраиваются: `votes/mine`, `reaction`, `mark` — отдельные эндпоинты с токеном. Рецензии заблокированных пользователей остаются в списках и сводке.
Сводка в карточку каталога не попадает: фронтенд запрашивает её отдельно.

### Подписки

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| PUT | `/users/{username}/follow` | user | Подписаться → 204 (повторно — тоже 204). На себя — 400 |
| DELETE | `/users/{username}/follow` | user | Отписаться → 204 (даже если не был подписан) |
| GET | `/users/{username}/followers` | гость | Подписчики: `{user, since}`, новые сверху |
| GET | `/users/{username}/following` | гость | На кого подписан, новые сверху |

Списки подписок открыты всем.

### Коллекции

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/collections` | гость | Новые публичные коллекции |
| GET | `/collections/featured` | гость | Особые коллекции (помечены admin) по `featured_position` — массив без пагинации |
| GET | `/collections/by-slug/{slug}` | гость | Особая по читаемому адресу: текущий slug → 200 `CollectionDetail` (как `/collections/{id}`); бывший, коллекция всё ещё особая → 200 `{"redirect_slug"}`; бывший, больше не особая, но публичная → 200 `{"redirect_id"}` (фронт делает 301); удалена, приватна, slug неизвестен → 404. Отдельный путь — чтобы не спорить с `/collections/{id}` и `/collections/featured` |
| GET | `/collections/{id}/related` | гость (`Option<AuthUser>`) | Связанные подборки по `position`: `Collection[]`, только публичные; связей нет — `[]`. Публичная коллекция — `Cache-Control: public, s-maxage=600`. 404 — нет коллекции или чужая приватная. То же в поле `related` детали коллекции (`/collections/{id}`, `/collections/by-slug/{slug}`) |
| GET | `/collections/{id}/comments` | гость (`Option<AuthUser>`) | Обсуждение: `Page<CollectionComment>` (`?limit=1..100&offset=`), старые сверху (`created_at`, `id`), ответы — в той же ленте с `parent_id` (фронт группирует под корнем). `CollectionComment`: `id`, `collection_id`, `parent_id`, `author` (`UserRef`), `body`, `created_at`, `updated_at`. 404 — нет коллекции или чужая приватная |
| POST | `/collections/{id}/comments` | user | `{"body", "parent_id"?}` (лишние поля — 400) → 201 `CollectionComment`. Ответ на ответ привязывается к корню. 400 `validation` с `field`: `body` (1–2000 после обрезки), `parent_id` (нет неудалённого комментария в этой коллекции); 403 `comments_locked` — обсуждение закрыто (admin может писать); 404 — нет коллекции или чужая приватная; 429 — больше 5 комментариев в минуту (`Retry-After: 60`) |
| DELETE | `/collection-comments/{id}` | автор | Удалить свой (корневой — вместе с ответами) → 204; 403 — чужой; 404 — нет или уже удалён |
| POST | `/collections` | user | `{"title", "description"?, "is_public"? = true}` → 201, коллекция с `items` |
| GET | `/collections/{id}` | гость / владелец | Коллекция с содержимым по порядку |
| PATCH | `/collections/{id}` | владелец | `title`, `description` (`null` очищает), `is_public`, `comments_locked` (закрыть/открыть обсуждение); особую сделать приватной — 400 `collection_private` |
| DELETE | `/collections/{id}` | владелец | → 204 |
| GET | `/users/{username}/collections` | гость / владелец | Коллекции пользователя, новые сверху; владелец видит и приватные. `?contains={slug}` — у каждой `contains: true/false` (кнопка «В коллекцию» одним запросом); неизвестный slug — 404 |
| GET | `/entities/{slug}/collections` | гость | Публичные коллекции, где есть сущность |
| PUT | `/collections/{id}/items/{slug}` | владелец | Добавить (201) или изменить пункт (200): `{"position"?, "note"?}`. Тело необязательно. Новый без `position` — в конец; `note: null` очищает, не переданная — не меняется |
| DELETE | `/collections/{id}/items/{slug}` | владелец | Убрать → 204 |
| PUT | `/collections/{id}/items` | владелец | Новый порядок: `{"entities": [slug, ...]}` — все пункты ровно по разу, позиции станут 0, 1, 2, ... |

**Видимость:** приватную коллекцию видит только владелец; для остальных, включая admin, её нет —
404. Это сознательно: приватное — личное (список «посмотреть позже»), и другим пользователям оно не
показывается, так что модерировать в нём нечего. Admin может удалить коллекцию по id
(`/admin/collections/{id}`), например по запросу правообладателя, но не читать её. Чужую публичную менять нельзя — 403. Эндпоинты «гость / владелец» принимают токен
необязательно (`Option<AuthUser>`): без заголовка — гость, с недействительным токеном — 401.

Коллекция в ответе: `id`, `owner` (`UserRef`), `title`, `description`, `is_public`, `is_featured`, `featured_position`, `slug`, `wallpaper_url`, `cover_url`, `theme`, `wallpaper_mode`, `wallpaper_dim`, `wallpaper_blur`, `effects`, `accent_color`, `header_mode` (у обычной `null` / значения по умолчанию: `header`, `null`, `0`, `true`, `null`, `styled`; фронт рисует особую иначе и строит её адрес `/collections/{slug}`), `comments_count` (комментариев в обсуждении, без удалённых), `comments_locked` (обсуждение закрыто), `items_count`,
`preview` (первые 4 произведения по порядку, `EntityRef` — обложки в списке; одним запросом на
страницу), `contains` (только с `?contains=`), даты; карточка — ещё `related` (связанные подборки, см. `collection_links`) и `items`: `{entity, position, note, added_at}`, где `entity` — `EntityRef` плюс `overview` (краткое описание: без HTML, до 1000 символов по границе предложения или слова, `null` — нет; одним запросом `EntityDirectory::overviews` на всю коллекцию; тот же вид у ответа `PUT .../items/{slug}`) по `position`, затем по времени
добавления. Изменение содержимого обновляет `updated_at` коллекции.

### Форум

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/threads` | гость | Темы форума. `sort`: `active` (по умолчанию, по `last_post_at`) или `new` (по созданию) |
| POST | `/threads` | author+ | `{"title", "body", "entities": [slug, ...]}` → 201, тема со страницей сообщений. `user` — 403 |
| GET | `/threads/{id}` | гость | Тема с текстом и `posts` — страница сообщений по времени: `?limit=` (1–100, по умолчанию **50**) `&offset=` |
| PATCH | `/threads/{id}` | автор темы | `title`, `body`, `entities` (набор заменяется целиком, в новом порядке). Ставит `edited_at` |
| DELETE | `/threads/{id}` | автор темы | → 204, сообщения удаляются вместе с темой |
| GET | `/entities/{slug}/threads` | гость | Темы, к которым привязана сущность (в том числе вместе с другими). `sort` — как у `/threads` |
| GET | `/users/{username}/threads` | гость | Темы пользователя, новые сверху |
| POST | `/threads/{id}/posts` | user+ | `{"body", "parent_id"?}` → 201. `parent_id` — ответ на сообщение этой темы; нет такого или оно удалено — 400. Закрытая тема — 403 (admin может) |
| PATCH | `/posts/{id}` | автор сообщения | `{"body"}`, ставит `edited_at`. В закрытой теме тоже можно. Заглушка — 404 |
| DELETE | `/posts/{id}` | автор сообщения | → 204: без ответов — удаляется, с ответами — заглушка. Повторно — 404 |

Тема в списке: `id`, `author`, `title`, `entities` (`EntityRef` по порядку, первая — главная),
`posts_count`, `last_post_at`, `is_locked`, `edited_at`, `created_at`. В карточке — ещё `body` и
`posts` (`{items, total, limit, offset}`; `total` — со заглушками).

Сообщение: `id`, `thread_id`, `parent_id`, `reply_to` (`UserRef` автора сообщения, на которое ответ;
`null` — ответ на тему или то сообщение удалено), `author`, `body`, `deleted`, `edited_at`,
`created_at`. Список **плоский, по времени**: ветки фронтенд рисует по `parent_id` (цитата «ответ
@user»), так длинные темы листаются обычной пагинацией. У заглушки `deleted: true`, `author` и
`body` — `null`.

Чужие тему и сообщение не правит никто, даже admin: он только удаляет и закрывает. Сообщения и темы
заблокированных пользователей остаются видны, как и их рецензии.

### Интересы

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/interests` | user | Свои интересы, новые сверху: `{entity, since}` |
| GET | `/entities/{slug}/interest` | user | Есть ли сущность в интересах (кнопка «Следить»): `{entity, since}` или 404 |
| PUT | `/entities/{slug}/interest` | user | Добавить → 204 (повторно — тоже 204). Уже 500 интересов — 400 |
| DELETE | `/entities/{slug}/interest` | user | Убрать → 204 (даже если не было) |
| GET | `/interests/people` | user | Свои интересы-люди, новые сверху: `Page<{person: PersonRef, since}>` |
| GET / PUT / DELETE | `/people/{slug}/interest` | user | Как для сущностей: есть ли (`{person, since}` или 404), добавить → 204 (уже 500 — 400), убрать → 204. Человек — только публичный, иначе 404 |
| GET | `/interests/tags` | user | Свои интересы-теги, новые сверху: `Page<{tag: TagRef, since}>` |
| GET / PUT / DELETE | `/tags/{slug}/interest` | user | Как для людей; 404 — нет тега |

Интересы личные: чужие не видит никто, включая admin. Изменение сразу доходит до открытых
WebSocket-соединений пользователя (`interest.added` / `interest.removed`, см. ниже).

### Чат по интересам

Отправить и отметить прочитанным — **только по WebSocket** (`chat.send`, `chat.read`, realtime.md).

| Метод | Путь | Кто | Что делает |
|---|---|---|---|
| GET | `/chats` | user | Мои чаты = комнаты всех сущностей, людей и тегов из интересов и свои комнаты, в которые вступил (сообщений может не быть): `[{room: "entity:<slug>" \| "person:<slug>" \| "tag:<slug>" \| "room:<slug>" \| "dm:<id>", kind, entity \| null, person \| null, tag \| null, custom \| null, dm \| null, last_message \| null, unread (до 100), muted}]` — из `entity`/`person`/`tag`/`custom` заполнено одно, по `kind`; сверху со свежими сообщениями, затем без сообщений (новые интересы сверху). Без пагинации (интересов ≤ 3 × 500) |
| GET | `/chat/{kind}/{slug}` | все (`Option<AuthUser>`) | Комната для открытой панели, в том числе не из «моих чатов» (открыта с карточки): `{room, kind, entity \| person \| tag \| custom, last_message \| null, me}`. `me` — вошедшему `{member, muted, unread (до 100), last_read_id}`: `member` — сущность, человек или тег в интересах / в свою комнату вступил; `last_read_id` — для разделителя «новые сообщения». Гостю `me: null`. 404 — неизвестный `kind` или slug; 401 — неверный токен |
| GET | `/chat/{kind}/{slug}/messages?before=&limit=` | все | История комнаты (`kind` — `entity`, `person`, `tag`, `room`): `{items, next_before}` — `limit` 1–100 (50) сообщений старше `before` (без него — последние), в `items` старые сверху; `next_before` — курсор дальше в прошлое, `null` — начало. 400 — `limit` вне 1–100; 404 — неизвестный `kind` или slug |
| GET | `/chat/search` | все (`Option<AuthUser>`) | Поиск по сообщениям: `?q=` (1–100, по словам с русской морфологией), `?room=<kind>:<slug>` (без него — по всем моим чатам, нужен вход), `?limit=` 1–100 (50), `?before=` — курсор. `{items: [{room, message}], next_before}`, новые сверху; удалённые не находятся. 400 — пустой/длинный `q`, `limit` вне 1–100, `room` не вида `kind:slug`; 401 — без `room` и без входа; 404 — нет комнаты |
| GET | `/chat/messages/{id}` | все | Одно сообщение (перечитать по `chat.message.updated` / `deleted`). 404 — нет |
| PATCH | `/chat/messages/{id}` | автор | `{body}` 1–2000 → 200 сообщение с `edited_at`. 403 — чужое, 400 — пусто/длинно/удалено, 404 — нет |
| DELETE | `/chat/messages/{id}` | автор или admin | Мягкое удаление → 204 (повторно — 204). 403 — чужое, 404 — нет |
| POST | `/chat/attachments` | user | Загрузить картинку (`multipart/form-data`, одно файловое поле) → 201 `Attachment` `{id, url, kind, width, height, size}`. Пережимается в WebP, длинная сторона ≤ 1600. 400 — не картинка (`not_an_image`), больше 2 МБ (`file_too_large`), пустой запрос; 429 — больше 10 загрузок за 10 минут |
| GET | `/chat/attachments/{id}` | как к сообщению | Сама картинка (`image/webp`). Пока не отправлена — только загрузившему; дальше решает комната. 404 — нет или нет доступа |
| PUT | `/users/{username}/dm` | user | Открыть переписку, заведя при первом обращении: 200 `ChatRoomInfo` (`kind = dm`, `dm: {id, peer}`); повторный вызов — та же, скрытую возвращает в список. 400 — себе (`dm_self`); 403 — блокировка (`blocked`), закрытые личные (`dm_closed`), неподтверждённый email (`email_not_verified`); 404 — нет пользователя; 429 — больше 20 новых за сутки |
| GET | `/chat/dm/{id}` | участник | Панель переписки: `ChatRoomInfo`. То же, что `GET /chat/{kind}/{slug}` при `kind = dm`; отдельный обработчик нужен, потому что `DELETE` делает путь статическим и иначе GET отвечал бы 405. 401 — гость; 404 — нет или чужая |
| DELETE | `/chat/dm/{id}` | участник | Убрать переписку у себя → 204. У собеседника остаётся, история цела, новое сообщение возвращает обратно. 404 — нет или чужая |
| PUT / DELETE | `/users/{username}/block` | user | Заблокировать / снять → 204 (повторно — 204). 400 — себя; 404 — нет пользователя |
| GET | `/users/blocked` | user | Мои блокировки, новые сверху: `Page<UserRef>` |
| GET / PATCH | `/users/me/chat-settings` | user | `{dm_policy}` — `all` (по умолчанию), `following`, `none`. PATCH с `{dm_policy}` → 200. 400 — неизвестное значение |
| PUT / DELETE | `/chat/rooms/{slug}/hidden` | user | Убрать комнату из «Моих чатов» / вернуть → 204 (повторно — 204). Скрытая не приходит в `GET /chats`, но остаётся рабочей: история, отправка и канал не меняются. 400 — комната не системная (из неё выходят); 404 — нет комнаты |
| PUT / DELETE | `/chat/{kind}/{slug}/mute` | user | Заглушить / снять → 204 (повторно — 204). 404 — неизвестный `kind` или slug |
| GET | `/chat/rooms` | все (`Option<AuthUser>`) | Комнаты: системные сверху, дальше по `?sort=active\|members\|new` (`active` — по умолчанию): `Page<CustomRoom>` (`?limit=&offset=`). `?q=` — поиск по названию и описанию (тогда порядок по близости, системные не первыми). `joined` — вступил ли вошедший (одним запросом на страницу), гостю `null`. 400 — неизвестный `sort`, `q` длиннее 100 |
| GET | `/chat/rooms/{slug}` | все (`Option<AuthUser>`) | Комната: `CustomRoom` `{id, slug, title, description, members_count, messages_count, messages_24h, last_message_at, created_at, system, owner, joined, hidden}` (`owner` — `UserRef` владельца, `null` у системной и админской; `joined` — гостю `null`, в системной и в `/chats` всегда `true`; `hidden` — убрана ли из «Моих чатов», гостю `null`). 404 — нет |
| POST | `/chat/rooms` | user | Своя комната: `{slug, title, description?}` → 201 `CustomRoom`, автор — владелец и участник (`interest.added`). 400 — неверный slug (a-z, 0-9, дефисы между ними, 1–80), пустое/длинное название (1–100), описание > 500; 409 — slug занят или зарезервирован (`slug_taken`), уже 5 своих комнат (`room_limit`); 429 — больше 3 за сутки (`Retry-After`) |
| PATCH | `/chat/rooms/{slug}` | владелец или admin | `{title?, description?}` (пустое описание — убрать; slug не меняется) → 200 `CustomRoom`, событие `chat.room.updated`. 403 — чужая или системная (`room_is_system`); 404 — нет |
| DELETE | `/chat/rooms/{slug}` | владелец или admin | Удалить с сообщениями и участниками → 204, событие `chat.room.deleted`. 403 — чужая или системная (`room_is_system`); 404 — нет |
| POST | `/chat/rooms/{slug}/report` | user | Жалоба `{reason}` (1–500) → 204; повторная своя обновляет причину, дубля нет. 400 — пустая/длинная; 403 — системная; 404 — нет |
| PUT / DELETE | `/chat/rooms/{slug}/membership` | user | Вступить / выйти → 204 (повторно — 204); публикует `interest.added` / `interest.removed` с `room_id`. В системной PUT — 204 без записи, DELETE — 403 (`room_is_system`). 404 — нет комнаты |
| GET | `/admin/chat/rooms/reports` | admin | Комнаты с жалобами, больше жалоб — выше: `Page<ReportedRoom>` `{room, reports_count, last_report_at}` (`?limit=&offset=`) |
| POST | `/admin/chat/rooms` | admin | Комната от имени площадки (`owner` — `null`): `{slug, title, description?}` → 201 `CustomRoom`; зарезервированные слаги админу доступны. 400 — как выше; 409 — slug занят |
| PATCH | `/admin/chat/rooms/{slug}` | admin | То же, что `PATCH /chat/rooms/{slug}`, для любой несистемной комнаты |
| DELETE | `/admin/chat/rooms/{slug}` | admin | То же, что `DELETE /chat/rooms/{slug}`, для любой несистемной комнаты |

`ChatMessage`: `id`, `kind` (`entity` / `person` / `tag` / `room`), `target_id` (id сущности, человека, тега или своей комнаты), `author` (`UserRef`; `null` у удалённого), `body` (`null` у
удалённого), `reply_to`, `attachment` (картинка или `null`), `created_at`, `edited_at`, `deleted`. Тег OpenAPI — `chat`.

### Отметки «Посмотреть позже» и «Просмотрено»

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/entities/{slug}/mark` | user | Своя отметка `{"kind": "later"}`, нет — 404 |
| PUT | `/entities/{slug}/mark` | user | `{"kind": "later" \| "done"}` → 200 `{"kind"}`; другая заменяет прежнюю |
| DELETE | `/entities/{slug}/mark` | user | Снять → 204 (даже если не было) |
| GET | `/users/me/marks` | user | Свои отметки, свежие сверху: `?kind=later\|done` (не передан — все), `limit`, `offset` → `Page<{entity, kind, created_at}>` |

Отметки приватные, как интересы: чужие не отдаются никак, включая admin.

### Лента

| Метод | Путь | Доступ | Что делает |
|---|---|---|---|
| GET | `/feed` | user | Лента: `?type=review\|thread\|collection`, `?limit=` (1–100, по умолчанию 20), `?cursor=` |

Собирается при чтении из таблиц social **ярусами** — следующий ярус начинается, когда предыдущий
закончился:

1. **Подписки** — всё, что публикуют те, на кого подписан пользователь: рецензии (в том числе
   оценки без текста), темы, публичные коллекции. Новые сверху.
2. **Интересы** — темы о сущностях из интересов и рецензии на них **с текстом**. Новые сверху.
   Записи, уже попавшие в первый ярус, не повторяются.
3. **Популярное** — остальные темы: больше сообщений сверху, при равенстве — недавняя активность.
   Новичок без подписок и интересов видит только его. Для начала так, позже логика усложнится.

Своих записей в ленте нет. Записи заблокированных пользователей показываются, как в других списках.

Ответ — `{items, next_cursor, limit}`. Запись: `type`, `created_at`, ровно одно из `review` / `thread`
/ `collection` (те же DTO, что в остальном API) и `reasons` — почему запись в ленте:
`{"type":"follow","user":UserRef}`, `{"type":"interest","entity":EntityRef}`, `{"type":"popular"}`.
Тема пользователя, на которого подписан, о сущности из интересов приходит **один раз** с обеими
причинами.

**Пагинация — курсором, не `offset`.** `next_cursor` — непрозрачная строка: ярус и место в нём.
Новые записи сверху не сдвигают следующие страницы, дублей на стыке страниц и ярусов нет. `null` —
лента закончилась. `total` нет: для ленты он не нужен и дорог. Испорченный курсор — 400.

### Модерация (только `admin`, иначе 401/403)

| Метод | Путь | Что делает |
|---|---|---|
| DELETE | `/admin/reviews/{id}` | Удалить любую рецензию → 204 (голоса и комментарии — вместе с ней) |
| DELETE | `/admin/review-comments/{id}` | Удалить любой комментарий к рецензии → 204 |
| DELETE | `/admin/collections/{id}` | Удалить любую коллекцию, в том числе приватную → 204 |
| PATCH | `/admin/collections/{id}` | Изменить название и описание **любой** коллекции (например, чужой особой): `{"title"?, "description"?, "comments_locked"?}` — не передано — не меняется, `description: null` — очистить → 200 `Collection`. 400 `validation` с `field` (`title` 1–200, `description` до 2000); `is_public` (и любое другое поле) — 400: видимость меняет только владелец. 404 — нет коллекции |
| DELETE | `/admin/collection-comments/{id}` | Удалить любой комментарий обсуждения коллекции (корневой — с ответами) → 204; 404 — нет или уже удалён |
| GET | `/admin/collections` | Поиск любых коллекций (и приватных) для редактора связей и «Подборок»: `?q=` — подстрока названия без учёта регистра (ILIKE, `%` и `_` буквально, до 200 символов; пусто — все), `public=true\|false`, `featured=true\|false`, `limit` 1–100 (20), `offset` → `Page<Collection>`. Сверху — точное совпадение названия, затем начинающиеся с `q`, затем больше произведений, новее. Индекс `collections_title_trgm` (GIN `gin_trgm_ops`, миграция `20261110100000_social_collections_title_trgm.sql`). 400 `validation` `field: q` — длиннее 200. Публичного поиска коллекций нет (публичные списки — через Meilisearch, коллекций там нет) |
| PUT | `/admin/collections/{id}/related` | Заменить все связи: `{"ids": [uuid, ...]}` — порядок в массиве = `position` → 200 актуальный `Collection[]`. 400 `validation` `field: ids` — сама коллекция, повтор, больше 12; 400 `collection_private` — связываемая приватная; 404 — нет коллекции или одной из связываемых |
| PUT | `/admin/collections/{id}/related/{related_id}` | Добавить связь: в конец или `{"position"?}` со сдвигом остальных (идемпотентно: уже есть — не меняется) → 200 `Collection[]`. 400 `validation` `field: related_id` — сама коллекция, `field: ids` — уже 12; 400 `collection_private`; 404 — нет коллекции или связываемой |
| DELETE | `/admin/collections/{id}/related/{related_id}` | Убрать связь (идемпотентно), позиции перенумеровываются → 204; 404 — нет коллекции |
| PUT | `/admin/collections/{id}/featured` | Пометить публичную коллекцию особой → 200 (`Collection`); тело `{"position"?, "slug"?, "wallpaper_url"?, "cover_url"?, "theme"?, "wallpaper_mode"?, "wallpaper_dim"?, "wallpaper_blur"?, "effects"?, "accent_color"?, "header_mode"?}` необязательно — оформление: не передано — не меняется, `null` — значение по умолчанию (URL, тема, `wallpaper_dim`, `accent_color` — `null`; `wallpaper_mode` — `header`; `wallpaper_blur` — 0; `effects` — `true`; `header_mode` — `styled`). URL — https до 2048, тема — `^[a-z0-9-]{1,32}$`, `wallpaper_mode` — `header`/`page`, `wallpaper_dim` — целое 0–100, `wallpaper_blur` — целое 0–20, `effects` — boolean, `accent_color` — `#rrggbb` (регистр приводится к нижнему), `header_mode` — `styled`/`plain`/`none`; неверный тип — тоже 400 с `field` (поля разбираются как JSON и проверяются в коде); без `position` в конец (уже особая — порядок не меняется); без `slug` при первой пометке — транслит названия (`social::slug::from_title`: «Русский топ 2021» → `russkiy-top-2021`, занят — `-2`, `-3`…), при повторном PUT — не меняется; смена slug — старый в историю, свой бывший можно вернуть. 400 `collection_private` — приватная; 400 `validation` с `field` (`slug`, `wallpaper_url`, `cover_url`, `theme`, `wallpaper_mode`, `wallpaper_dim`, `wallpaper_blur`, `effects`, `accent_color`, `header_mode`) — неверный формат, UUID, служебное слово, не https-URL; 409 `conflict` — slug занят другой коллекцией (сейчас или раньше) |
| DELETE | `/admin/collections/{id}/featured` | Снять отметку → 204 (повторно — тоже); slug уходит в историю (старый адрес ведёт на `/collections/{id}`, другой коллекции не достанется); оформление сбрасывается (в `NULL` и значения по умолчанию) |
| PUT | `/admin/collections/{id}/items/{slug}` | Добавить (201) или изменить (200) произведение в **любой особой** коллекции — как `PUT /collections/{id}/items/{slug}` владельца (`{"note"?, "position"?}`, лимит 500). 400 `not_featured` — коллекция не особая; 404 — нет коллекции или сущности |
| DELETE | `/admin/collections/{id}/items/{slug}` | Убрать произведение из любой особой → 204. 400 `not_featured`; 404 — нет коллекции, сущности или её нет в коллекции |
| DELETE | `/admin/threads/{id}` | Удалить любую тему вместе с сообщениями → 204 |
| DELETE | `/admin/posts/{id}` | Удалить любое сообщение → 204 (с ответами — заглушка, как у автора) |
| PUT | `/admin/threads/{id}/lock` | Закрыть тему для ответов → 204 (повторно — тоже) |
| DELETE | `/admin/threads/{id}/lock` | Открыть тему → 204 |

### Проверки

| Поле | Правило |
|---|---|
| `rating` | целое 1–10 |
| `body` рецензии | до 10 000 символов |
| `title` коллекции | 1–200 символов |
| `description` коллекции | до 2000 символов |
| `note` пункта | до 1000 символов |
| пунктов в коллекции | не больше 500 (иначе 400) |
| `title` темы | 1–200 символов |
| `body` темы | 1–20 000 символов |
| `entities` темы | 1–10 slug'ов без повторов; неизвестный — 400 |
| `body` сообщения | 1–10 000 символов |
| `body` комментария к рецензии | 1–2000 символов |

Строки обрезаются по краям, пустая необязательная строка становится `null`. Неизвестные поля в
теле — 400.

</endpoints>

<seed>

## Мини-дамп — удалён

До 2026-10-14 `seeds/social.sql` (рецензии, подписки, коллекции, форум, интересы, голоса,
реакции, отметки поверх сидов каталога) грузил `make seed`. Удалён вместе с сидами каталога и тестом
`seed_social_is_valid`; `make seed` грузит только пользователей `seeds/dev.sql`. Живые проверки
`http/social.http`, `http/forum.http` в части чтения ждут данных, созданных запросами или вручную.

</seed>

<code_map>

## Код

| Файл | Что внутри |
|---|---|
| `lib.rs` | Роутер |
| `models.rs` | DTO и строки БД |
| `refs.rs` | Сущности и пользователи через справочники `shared::directory` |
| `validate.rs` | Проверка строк и оценки; FK violation → 404 |
| `reviews.rs`, `follows.rs`, `collections.rs` | Эндпоинты |
| `votes.rs`, `comments.rs` | Голоса и комментарии к рецензиям |
| `reactions.rs`, `marks.rs` | Реакции произведений и коллекций, личные отметки |
| `target_comments.rs` | Комментарии к произведениям, людям и студиям: разбор цели `kind:slug` через справочники, лента, запись (лимит частоты, ответы — к корню), мягкое удаление, `remove` для модерации |
| `likes.rs` | Лайки профилей: поставить, снять, список лайкнувших |
| `user_comments.rs` | «Мои комментарии»: `UNION ALL` трёх таблиц одной лентой, пакетное раскрытие целей |
| `stats.rs` | Аналитика профиля: сводка, список оценок, отметки пользователя; проверка приватности (`visible`) |
| `ratings.rs` | Сводки оценок для сортировок каталога (`EntityRatings`) |
| `threads.rs`, `posts.rs` | Форум: темы и сообщения |
| `interests.rs` | Интересы: сущности, люди, теги (общие `add_interest` / `remove_interest`) |
| `rooms.rs` | Комнаты со slug: список и поиск, комната, вступление и выход, скрытие системных из «Моих чатов», свои комнаты пользователей (лимиты, зарезервированные слаги, правка и удаление владельцем), жалобы и их разбор, админские создание/правка/удаление; системные закрыты от всех изменений |
| `chat.rs` | Чат: `PgChat` (`shared::directory::Chat` для realtime), мои чаты, история, поиск по сообщениям, разбор `@упоминаний`, правка, удаление, заглушение |
| `feed.rs` | Лента: ярусы, курсор |
| `events.rs` | События для `realtime` |
| `attachments.rs` | Вложения чата: загрузка и пережатие в WebP, раздача с проверкой доступа, привязка к сообщению, уборка сирот |
| `dm.rs` | Личные переписки 1:1: открытие и удаление у себя, блокировки и их список, `dm_policy`, доступ (`visible`, `peer_of`, `check_not_blocked`) |
| `directory.rs` | `InterestDirectory` для других модулей |
| `collection_links.rs` | Связанные подборки: выдача (`related` для детали), замена списка, добавление на позицию, удаление, перенумерация |
| `collection_comments.rs` | Обсуждение коллекции: список, запись (ответы — к корню, лимит частоты, закрытое обсуждение), удаление своего, `remove` для модерации |
| `admin.rs` | Модерация; поиск коллекций (`search_collections`); правка названия и описания любой коллекции; особые коллекции: slug, оформление, состав любой особой (`collections::Editor::Admin`) |
| `slug.rs` | slug особых коллекций: проверка формата, транслит названия, служебные слова |

Тесты — `app/tests/social.rs`, `app/tests/chat.rs` (чат: WebSocket + REST), `app/tests/forum.rs`, `app/tests/interests.rs`, `app/tests/feed.rs` (сущности — `app/tests/fixtures/catalog.sql`,
пользователи — через dev login). Живые проверки — `http/social.http` и `http/forum.http`: всё, что
файл создаёт, он же удаляет; `forum.http` первым делом удаляет свои темы, оставшиеся от прерванного
прогона (по заголовку «Проверка из http/forum.http…»).

</code_map>

<events>

## События для realtime

После успешного изменения (после `commit`) `social` публикует событие в `state.events`
(`shared::events::EventBus`), а [`realtime`](../../documents/modules/realtime.md) доставляет его WebSocket-клиентам,
подписанным на каналы события. Ошибка запроса — событий нет. В данных **только id**: тексты и имена
клиент перечитывает по API.

| Событие | Каналы | `data` |
|---|---|---|
| `thread.created` | сущности темы | `thread_id`, `author_id` |
| `thread.updated` | тема, сущности (при смене набора — старые и новые) | `thread_id` |
| `thread.deleted` | тема, сущности | `thread_id` |
| `post.created` | тема, сущности темы | `thread_id`, `post_id`, `parent_id`, `author_id` |
| `post.updated`, `post.deleted` | тема | `thread_id`, `post_id` |
| `reply.created` | автор сообщения, на которое ответили (себе — нет) | `thread_id`, `post_id`, `parent_id`, `author_id` |
| `review.created`, `review.updated`, `review.deleted` | сущность | `review_id`, `entity_id`, `author_id` |
| `review.commented`, `review.comment_deleted` | сущность | `review_id`, `comment_id`, `entity_id`, `author_id` |
| `interest.added`, `interest.removed` | пользователь | `entity_id`, `person_id`, `tag_id` или `room_id` (вступил в свою комнату / вышел) |
| `chat.message.created` | комната: `entity:`, `person:`, `tag:` или `room:` | `kind`, `target_id`, `message` — **сообщение целиком** (`ChatMessage`). Исключение из правила «только id»: иначе каждое сообщение — запрос каждого онлайн-клиента |
| `chat.message.updated`, `chat.message.deleted` | комната | `kind`, `target_id`, `message_id` — перечитать `GET /chat/messages/{id}` |
| `dm.created` | пользователь (`user:`) | `dm_id` — завели переписку. В личные каналы **обоих**: по нему же открытые соединения `realtime` подписываются на `dm:<id>`, иначе сообщения в новой переписке дошли бы до них только после переподключения. Читать `GET /chat/dm/{id}` |
| `chat.room.created` | каталог комнат (`rooms`) | `room_id` — завели новую комнату: открытый список добавляет строку. Только в `rooms`: на канал самой комнаты ещё никто не подписан |
| `chat.room.updated`, `chat.room.deleted` | комната (`room:`) и каталог (`rooms`) | `room_id` — перечитать `GET /chat/rooms/{slug}` или закрыть |
| `chat.mention` | пользователь (`user:`) | `kind`, `target_id`, `message` — в сообщении назвали `@username`. Второе исключение из «только id» после `chat.message.created`: текст нужен сразу, чтобы показать уведомление. Комната — `kind` + `target_id` (в `channels` здесь `user:`, имени комнаты в них нет) |
| `chat.read` | пользователь | `kind`, `target_id`, `message_id` — прочитано до него: другие устройства гасят счётчик |

`thread.updated` — и правка автором, и закрытие/открытие admin; `thread.deleted` и `post.deleted` —
и автором, и admin. Коллекции и подписки на пользователей событий не публикуют. Код —
`modules/social/src/events.rs`.

</events>

<roadmap>

## Дальше 🕓

- «Новое в ленте» в реальном времени по подпискам на людей: сейчас по интересам это уже приходит
  через каналы сущностей `realtime`, по людям нужен новый канал.
- Чат: модерация (бан в комнате, жалобы), вложения, «печатает…».
- «Популярное» сложнее: за период, с рецензиями и коллекциями, с учётом интересов похожих людей.

</roadmap>
