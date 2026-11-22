-- ИИ-преподаватель: обсуждение книги с ИИ на ключе самого пользователя.
--
-- Ключи пользователя (свои провайдеры): шифрует Postgres (pgcrypto) секретом AI_SECRET_KEY, как
-- catalog_ai_providers. Наружу — только подсказка key_hint.
CREATE TABLE user_ai_providers (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Протокол: openai — любой OpenAI-совместимый (/chat/completions), anthropic — /v1/messages.
    kind       text        NOT NULL CHECK (kind IN ('openai', 'anthropic')),
    title      text        NOT NULL CHECK (length(title) BETWEEN 1 AND 60),
    base_url   text        CHECK (length(base_url) <= 300),
    model      text        NOT NULL CHECK (length(model) BETWEEN 1 AND 120),
    api_key    bytea       NOT NULL,
    key_hint   text        NOT NULL,
    is_default boolean     NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX user_ai_providers_user_idx ON user_ai_providers (user_id, created_at);
-- Провайдер по умолчанию — один на пользователя.
CREATE UNIQUE INDEX user_ai_providers_default_uniq ON user_ai_providers (user_id) WHERE is_default;

CREATE TRIGGER user_ai_providers_set_updated_at
    BEFORE UPDATE ON user_ai_providers
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Обсуждение книги с ИИ. summary — сжатое содержание ранних сообщений (в модель уходит оно и
-- последние реплики, а не вся переписка: так запрос стоит копейки при любой длине чата);
-- summarized — сколько первых сообщений уже вошло в summary.
CREATE TABLE ai_threads (
    id             uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id        uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    entity_id      uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    title          text        NOT NULL CHECK (length(title) BETWEEN 1 AND 200),
    summary        text        NOT NULL DEFAULT '',
    summarized     integer     NOT NULL DEFAULT 0,
    messages_count integer     NOT NULL DEFAULT 0,
    tokens_in      bigint      NOT NULL DEFAULT 0,
    tokens_out     bigint      NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ai_threads_user_idx ON ai_threads (user_id, updated_at DESC);
CREATE INDEX ai_threads_book_idx ON ai_threads (user_id, entity_id, updated_at DESC);

CREATE TRIGGER ai_threads_set_updated_at
    BEFORE UPDATE ON ai_threads
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE ai_messages (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    thread_id  uuid        NOT NULL REFERENCES ai_threads (id) ON DELETE CASCADE,
    role       text        NOT NULL CHECK (role IN ('user', 'assistant')),
    content    text        NOT NULL CHECK (length(content) <= 20000),
    -- Вопрос из читалки: выделенный фрагмент и глава — к ним вопрос и относится.
    quote      text        CHECK (length(quote) <= 1500),
    chapter    text        CHECK (length(chapter) <= 300),
    tokens_in  integer,
    tokens_out integer,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ai_messages_thread_idx ON ai_messages (thread_id, created_at, id);
