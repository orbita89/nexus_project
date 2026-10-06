-- Мини-дамп социального модуля для локальной разработки: рецензии и оценки, подписки и коллекции
-- тестовых пользователей (seeds/dev.sql) на сущности из seeds/catalog.sql.
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

COMMIT;
