-- Мини-дамп для локальной разработки: тестовые пользователи всех ролей.
-- Загрузка: make seed (после старта приложения — нужны применённые миграции).
-- Повторный запуск безопасен: существующие записи не трогаются.
--
-- Пароль у всех: password123
-- Хеш получен через: cargo run -p auth --example hash_password -- password123
--
-- НЕ для прода. Когда будем брать данные из прода, персональные данные (email, имена,
-- хеши паролей, IP) обязательно обезличивать до того, как дамп попадёт в репозиторий.

BEGIN;

INSERT INTO users (id, email, username, password_hash, display_name, role, is_active) VALUES
    ('00000000-0000-4000-8000-000000000001', 'admin@nexus.local',   'admin',   '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Администратор',        'admin',  true),
    ('00000000-0000-4000-8000-000000000002', 'author@nexus.local',  'author',  '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Автор форума',         'author', true),
    ('00000000-0000-4000-8000-000000000003', 'user@nexus.local',    'user',    '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Обычный пользователь', 'user',   true),
    ('00000000-0000-4000-8000-000000000004', 'blocked@nexus.local', 'blocked', '$argon2id$v=19$m=19456,t=2,p=1$apC26N9EwcIwxzfqlbkpOg$1PRD3C7ATtGryIyfK6ibYuu7kZmQ+2rwRtHBfXRuMqA', 'Заблокированный',      'user',   false)
ON CONFLICT DO NOTHING;

COMMIT;
