-- Небольшой каталог для тестов app/tests/catalog.rs. Ожидания в тестах завязаны на эти данные.

INSERT INTO tags (slug, name) VALUES
    ('sci-fi',  'Научная фантастика'),
    ('fantasy', 'Фэнтези'),
    ('unused',  'Без сущностей');

INSERT INTO entities (kind, slug, title, original_title, release_date, metadata) VALUES
    ('book',   'dune-novel',         'Дюна',               'Dune',                 '1965-08-01', '{"pages": 896}'),
    ('movie',  'dune-2021',          'Дюна',               'Dune',                 '2021-10-22', '{"runtime_min": 155}'),
    ('movie',  'dune-part-two-2024', 'Дюна: Часть вторая', 'Dune: Part Two',       '2024-03-01', '{}'),
    ('game',   'witcher-3',          'Ведьмак 3',          'The Witcher 3',        '2015-05-19', '{}'),
    ('series', 'no-date',            '100% без даты',      NULL,                   NULL,         '{}');

INSERT INTO entity_tags (entity_id, tag_id)
SELECT e.id, t.id FROM entities e, tags t
WHERE (e.slug, t.slug) IN (('dune-novel', 'sci-fi'), ('dune-2021', 'sci-fi'),
                           ('dune-part-two-2024', 'sci-fi'), ('witcher-3', 'fantasy'));

INSERT INTO people (slug, full_name, birth_date) VALUES
    ('denis-villeneuve',  'Дени Вильнёв',  '1967-10-03'),
    ('timothee-chalamet', 'Тимоти Шаламе', NULL),
    ('frank-herbert',     'Фрэнк Херберт', NULL),
    ('nobody',            'Никто Никтович', NULL);

INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
SELECT e.id, p.id, v.role, v.character_name, v.position
FROM (VALUES
    ('dune-2021',          'timothee-chalamet', 'actor',    'Пол Атрейдес', 1),
    ('dune-2021',          'denis-villeneuve',  'director', NULL,           0),
    ('dune-part-two-2024', 'denis-villeneuve',  'director', NULL,           0),
    ('dune-novel',         'frank-herbert',     'author',   NULL,           0)
) AS v(entity, person, role, character_name, position)
JOIN entities e ON e.slug = v.entity
JOIN people p ON p.slug = v.person;
