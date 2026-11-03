-- Мини-дамп для локальной разработки: тестовые пользователи всех ролей.
-- Загрузка: make seed (после старта приложения — нужны применённые миграции).
-- Повторный запуск безопасен: существующие записи не трогаются.
--
-- Пароль у всех: password123. Email у всех подтверждён.
-- Хеш получен через: cargo run -p auth --example hash_password -- password123
--
-- НЕ для прода. Когда будем брать данные из прода, персональные данные (email, имена,
-- хеши паролей, IP) обязательно обезличивать до того, как дамп попадёт в репозиторий.

BEGIN;

INSERT INTO users (id, email, username, password_hash, display_name, role, is_active, email_verified_at) VALUES
    ('00000000-0000-4000-8000-000000000001', 'admin@nexus.local',   'admin',   '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Администратор',        'admin',  true,  now()),
    ('00000000-0000-4000-8000-000000000002', 'author@nexus.local',  'author',  '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Автор форума',         'author', true,  now()),
    ('00000000-0000-4000-8000-000000000003', 'user@nexus.local',    'user',    '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Обычный пользователь', 'user',   true,  now()),
    ('00000000-0000-4000-8000-000000000004', 'blocked@nexus.local', 'blocked', '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Заблокированный',      'user',   false, now())
ON CONFLICT DO NOTHING;

-- Описания нескольких популярных жанров: каталог приходит из импорта (scripts/catalog-import),
-- поэтому теги только дополняются — нет тега, нет и строки. Уже написанное не затирается.
UPDATE tags SET description = v.description
FROM (VALUES
    ('drama',       'Истории о характерах и отношениях: конфликт держится на людях, а не на внешних событиях.'),
    ('comedy',      'Смешное во всех видах — от бытовых недоразумений до сатиры на нравы.'),
    ('sci-fi',      'Мир, перестроенный наукой и технологией: космос, будущее, изобретения и их последствия.'),
    ('fantasy',     'Выдуманный мир со своей магией и мифологией, живущий по собственным законам.'),
    ('thriller',    'Напряжение как главный приём: угроза близко, время на исходе.'),
    ('documentary', 'Невыдуманный материал: съёмка, хроника и свидетельства о том, что было на самом деле.')
) AS v(slug, description)
WHERE tags.slug = v.slug AND tags.description IS NULL;

COMMIT;
