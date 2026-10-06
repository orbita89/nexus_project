-- Мини-дамп социального модуля для локальной разработки: рецензии и оценки, подписки, коллекции,
-- форум и интересы тестовых пользователей (seeds/dev.sql) на сущности из seeds/catalog.sql.
-- Загрузка: make seed (после dev.sql и catalog.sql). Повторный запуск безопасен: существующие
-- записи не трогаются.
--
-- Связи пишутся через подзапросы по slug и username: опечатка даёт NOT NULL violation, а не тихо
-- потерянную строку. Проверка, что дамп загружается и читается через API, — тест
-- seed_social_is_valid (app/tests/social.rs).

BEGIN;

-- Рецензии: с текстом, только оценка (без текста) и только текст (без оценки).
INSERT INTO reviews (user_id, entity_id, rating, body)
SELECT
    (SELECT id FROM users WHERE username = v.username::citext),
    (SELECT id FROM entities WHERE slug = v.slug),
    v.rating, v.body
FROM (VALUES
    ('user',   'dune-2021',               9::smallint, 'Вильнёв сделал невозможное: Арракис ощущается огромным и живым. Звук и музыка Циммера — отдельный персонаж.'),
    ('user',   'dune-novel',              10,          'Книга, после которой фантастика уже не та. Экология, религия и политика в одном сюжете.'),
    ('user',   'dune-1984',               5,           NULL),
    ('user',   'the-witcher-3-wild-hunt', 10,          'Кровавый барон — лучший квест в истории RPG.'),
    ('user',   'stalker-1979',            8,           NULL),
    ('user',   'roadside-picnic',         NULL,        'Перечитываю раз в несколько лет и каждый раз нахожу новое.'),
    ('author', 'dune-2021',               7,           'Красиво, но книжный Пол сложнее. Ждём вторую часть.'),
    ('author', 'dune-part-two-2024',      9,           'Вторая часть сильнее первой: Пол наконец становится тем, кем должен.'),
    ('author', 'blade-runner-2049',       10,          NULL),
    ('author', 'do-androids-dream',       8,           'Дик тревожнее и страннее фильма. Электрические овцы — не просто метафора.'),
    ('admin',  'dune-2021',               8,           NULL),
    ('admin',  'interstellar',            9,           'Сцена с видеосообщениями до сих пор выбивает из колеи.'),
    -- Заблокированный пользователь: его рецензия остаётся в списках и сводке.
    ('blocked', 'dune-2021',              2,           'Скучно.')
) AS v(username, slug, rating, body)
ON CONFLICT (user_id, entity_id) DO NOTHING;

-- Подписки: user и admin читают автора, автор — пользователя.
INSERT INTO follows (follower_id, followee_id)
SELECT
    (SELECT id FROM users WHERE username = v.follower::citext),
    (SELECT id FROM users WHERE username = v.followee::citext)
FROM (VALUES
    ('user',   'author'),
    ('admin',  'author'),
    ('author', 'user'),
    ('user',   'admin')
) AS v(follower, followee)
ON CONFLICT DO NOTHING;

-- Коллекции: id фиксированы, чтобы повторная загрузка не плодила копии.
INSERT INTO collections (id, user_id, title, description, is_public)
SELECT v.id::uuid, (SELECT id FROM users WHERE username = v.username::citext), v.title, v.description, v.is_public
FROM (VALUES
    ('00000000-0000-4000-9000-000000000001', 'user',   'Дюна во всех видах',
     'Книги и экранизации по порядку выхода: удобно сравнивать.', true),
    ('00000000-0000-4000-9000-000000000002', 'user',   'Посмотреть позже',
     NULL, false),
    ('00000000-0000-4000-9000-000000000003', 'author', 'Зона: Стругацкие и наследники',
     'От «Пикника на обочине» до S.T.A.L.K.E.R. 2.', true)
) AS v(id, username, title, description, is_public)
ON CONFLICT (id) DO NOTHING;

INSERT INTO collection_items (collection_id, entity_id, position, note)
SELECT v.collection::uuid, (SELECT id FROM entities WHERE slug = v.slug), v.position, v.note
FROM (VALUES
    ('00000000-0000-4000-9000-000000000001', 'dune-novel',                 0, 'С чего всё началось'),
    ('00000000-0000-4000-9000-000000000001', 'dune-messiah',               1, NULL),
    ('00000000-0000-4000-9000-000000000001', 'dune-1984',                  2, 'Линч: странно, но любопытно'),
    ('00000000-0000-4000-9000-000000000001', 'dune-2021',                  3, NULL),
    ('00000000-0000-4000-9000-000000000001', 'dune-part-two-2024',         4, NULL),
    ('00000000-0000-4000-9000-000000000002', 'arrival-2016',               0, NULL),
    ('00000000-0000-4000-9000-000000000002', 'the-expanse',                1, 'Советовали начать с книги'),
    ('00000000-0000-4000-9000-000000000003', 'roadside-picnic',            0, 'Первоисточник'),
    ('00000000-0000-4000-9000-000000000003', 'stalker-1979',               1, 'Тарковский почти ничего не взял из сюжета — и всё из духа'),
    ('00000000-0000-4000-9000-000000000003', 'stalker-shadow-of-chernobyl', 2, NULL),
    ('00000000-0000-4000-9000-000000000003', 'stalker-2',                  3, NULL)
) AS v(collection, slug, position, note)
ON CONFLICT (collection_id, entity_id) DO NOTHING;

-- Форум: темы на несколько сущностей сразу. id тем и сообщений фиксированы; время — относительно
-- загрузки, чтобы «свежие» и «активные» выглядели живыми.
INSERT INTO forum_threads (id, author_id, title, body, is_locked, created_at)
SELECT v.id::uuid, (SELECT id FROM users WHERE username = v.username::citext), v.title, v.body,
       v.is_locked, now() - v.age::interval
FROM (VALUES
    ('00000000-0000-4000-9100-000000000001', 'author', 'Дюна: книга против экранизаций',
     'Херберт, Линч и Вильнёв: что каждая версия поняла в Поле Атрейдесе, а что потеряла?', false, '10 days'),
    ('00000000-0000-4000-9100-000000000002', 'author', 'Пикник на обочине, Сталкер и Зона в играх',
     'Тарковский взял у Стругацких почти только название. А что взяли разработчики S.T.A.L.K.E.R.?', false, '5 days'),
    ('00000000-0000-4000-9100-000000000003', 'admin',  'Ведьмак: с чего начинать — книги, игра или сериал?',
     'Собираем советы новичкам. Тема закрыта: всё главное уже сказано.', true, '2 days'),
    ('00000000-0000-4000-9100-000000000004', 'author', 'Бегущий по лезвию 2049 и роман Дика',
     'Фильм — продолжение экранизации, но вопросы задаёт те же, что и книга. Согласны?', false, '1 hour')
) AS v(id, username, title, body, is_locked, age)
ON CONFLICT (id) DO NOTHING;

INSERT INTO forum_thread_entities (thread_id, entity_id, position)
SELECT v.thread::uuid, (SELECT id FROM entities WHERE slug = v.slug), v.position
FROM (VALUES
    ('00000000-0000-4000-9100-000000000001', 'dune-novel',                  0::smallint),
    ('00000000-0000-4000-9100-000000000001', 'dune-2021',                   1),
    ('00000000-0000-4000-9100-000000000001', 'dune-part-two-2024',          2),
    ('00000000-0000-4000-9100-000000000001', 'dune-1984',                   3),
    ('00000000-0000-4000-9100-000000000002', 'roadside-picnic',             0),
    ('00000000-0000-4000-9100-000000000002', 'stalker-1979',                1),
    ('00000000-0000-4000-9100-000000000002', 'stalker-shadow-of-chernobyl', 2),
    ('00000000-0000-4000-9100-000000000003', 'the-last-wish',               0),
    ('00000000-0000-4000-9100-000000000003', 'the-witcher-3-wild-hunt',     1),
    ('00000000-0000-4000-9100-000000000003', 'the-witcher-series',          2),
    ('00000000-0000-4000-9100-000000000004', 'blade-runner-2049',           0),
    ('00000000-0000-4000-9100-000000000004', 'do-androids-dream',           1)
) AS v(thread, slug, position)
ON CONFLICT (thread_id, entity_id) DO NOTHING;

-- Сообщения с ветками (parent — ответ на сообщение). Сообщение blocked удалено, но на него
-- ответили: остаётся заглушкой «сообщение удалено» (body = NULL).
INSERT INTO forum_posts (id, thread_id, author_id, parent_id, body, deleted_at, created_at)
SELECT v.id::uuid, v.thread::uuid, (SELECT id FROM users WHERE username = v.username::citext),
       v.parent::uuid, v.body, CASE WHEN v.body IS NULL THEN now() - v.age::interval END,
       now() - v.age::interval
FROM (VALUES
    ('00000000-0000-4000-9200-000000000001', '00000000-0000-4000-9100-000000000001', 'user',    NULL,
     'У Херберта Пол с первых страниц понимает, куда ведёт его путь. У Вильнёва это сомнение показано лучше.', '9 days'),
    ('00000000-0000-4000-9200-000000000002', '00000000-0000-4000-9100-000000000001', 'author',  '00000000-0000-4000-9200-000000000001',
     'Согласен, но во второй части фильм прямо спорит с книгой: Чани там голос сомнения.', '8 days'),
    ('00000000-0000-4000-9200-000000000003', '00000000-0000-4000-9100-000000000001', 'admin',   NULL,
     'Не забывайте Линча: странно, но атмосферу Империи он поймал раньше всех.', '7 days'),
    ('00000000-0000-4000-9200-000000000004', '00000000-0000-4000-9100-000000000001', 'blocked', NULL,
     NULL, '6 days'),
    ('00000000-0000-4000-9200-000000000005', '00000000-0000-4000-9100-000000000001', 'user',    '00000000-0000-4000-9200-000000000004',
     'Давайте без спойлеров к «Мессии».', '5 days'),
    ('00000000-0000-4000-9200-000000000006', '00000000-0000-4000-9100-000000000001', 'user',    '00000000-0000-4000-9200-000000000002',
     'Тогда вопрос: «Мессию» Вильнёв снимет так же вольно?', '2 hours'),
    ('00000000-0000-4000-9200-000000000007', '00000000-0000-4000-9100-000000000002', 'user',    NULL,
     'В «Пикнике» Зона — чужой мусор, в «Сталкере» — зеркало. Игра ближе к книге.', '4 days'),
    ('00000000-0000-4000-9200-000000000008', '00000000-0000-4000-9100-000000000002', 'author',  '00000000-0000-4000-9200-000000000007',
     'Аномалии и артефакты точно из книги, а вот Монолит — чистая игра.', '3 days'),
    ('00000000-0000-4000-9200-000000000009', '00000000-0000-4000-9100-000000000003', 'user',    NULL,
     'Начинайте с «Последнего желания»: игра потом раскрывается совсем иначе.', '1 day'),
    ('00000000-0000-4000-9200-000000000010', '00000000-0000-4000-9100-000000000003', 'admin',   NULL,
     'Спасибо всем, закрываю тему.', '20 hours')
) AS v(id, thread, username, parent, body, age)
ON CONFLICT (id) DO NOTHING;

-- Счётчики тем дампа: видимые сообщения и время последнего (у темы без ответов — создание).
UPDATE forum_threads t SET
    posts_count = (SELECT count(*) FROM forum_posts p WHERE p.thread_id = t.id AND p.deleted_at IS NULL),
    last_post_at = COALESCE((SELECT max(p.created_at) FROM forum_posts p WHERE p.thread_id = t.id), t.created_at)
WHERE t.id::text LIKE '00000000-0000-4000-9100-%';

-- Интересы: по ним realtime сам подписывает соединение на каналы сущностей.
INSERT INTO user_interests (user_id, entity_id)
SELECT (SELECT id FROM users WHERE username = v.username::citext), (SELECT id FROM entities WHERE slug = v.slug)
FROM (VALUES
    ('user',   'dune-2021'),
    ('user',   'dune-novel'),
    ('user',   'the-witcher-3-wild-hunt'),
    ('author', 'roadside-picnic'),
    ('author', 'blade-runner-2049')
) AS v(username, slug)
ON CONFLICT DO NOTHING;

COMMIT;
