-- Провайдеры ИИ для служебных задач каталога (разметка книг `books_llm.py`, дальше — другие).
-- Админ задаёт их в админке (`/admin/ai/providers`) или `nexus ai add`; скрипты берут
-- настройки отсюда, ключи в командах не нужны.
--
-- `api_keys` — ключи через запятую, зашифрованные pgcrypto (`pgp_sym_encrypt`) секретом
-- `AI_SECRET_KEY` приложения: в дампе БД ключей открытым текстом нет. Наружу (API, логи) —
-- только `keys_hint` (`sk-…a1b2`). `is_default` — провайдер по умолчанию, не больше одного.
CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE catalog_ai_providers (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    slug        text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,39}$'),
    kind        text NOT NULL CHECK (kind IN ('anthropic', 'openai')),
    base_url    text CHECK (base_url ~ '^https?://'),
    model       text NOT NULL CHECK (length(model) BETWEEN 1 AND 200),
    api_keys    bytea NOT NULL,
    keys_hint   text NOT NULL,
    keys_count  integer NOT NULL CHECK (keys_count > 0),
    workers     integer NOT NULL DEFAULT 8 CHECK (workers BETWEEN 1 AND 64),
    per_request integer NOT NULL DEFAULT 100 CHECK (per_request BETWEEN 1 AND 500),
    is_default  boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX catalog_ai_providers_default ON catalog_ai_providers (is_default)
    WHERE is_default;

CREATE TRIGGER catalog_ai_providers_set_updated_at
    BEFORE UPDATE ON catalog_ai_providers
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
