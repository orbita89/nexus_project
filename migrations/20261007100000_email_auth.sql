-- Вход и регистрация по почте.
--   * регистрация по паролю требует подтверждения email;
--   * вход по ссылке из письма (без пароля) — у таких пользователей может не быть пароля;
--   * сброс пароля по ссылке.

ALTER TABLE users
    ADD COLUMN email_verified_at timestamptz,
    -- NULL — пользователь зарегистрировался по ссылке и пароль не задавал.
    ALTER COLUMN password_hash DROP NOT NULL;

-- Пользователи, созданные до обязательного подтверждения, считаются подтверждёнными.
UPDATE users SET email_verified_at = created_at;

CREATE TYPE email_token_purpose AS ENUM ('verify_email', 'login', 'reset_password');

-- Одноразовые токены из писем. Как и refresh-токены, хранится только хеш.
CREATE TABLE email_tokens (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    purpose    email_token_purpose NOT NULL,
    email      citext      NOT NULL,
    -- NULL — вход по ссылке для email, у которого ещё нет аккаунта: он создастся при подтверждении.
    user_id    uuid REFERENCES users (id) ON DELETE CASCADE,
    token_hash text        NOT NULL UNIQUE,
    expires_at timestamptz NOT NULL,
    used_at    timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Новый токен гасит предыдущие неиспользованные того же назначения на тот же адрес.
CREATE INDEX email_tokens_active_idx ON email_tokens (purpose, email) WHERE used_at IS NULL;
-- Для фоновой чистки.
CREATE INDEX email_tokens_expires_at_idx ON email_tokens (expires_at);
