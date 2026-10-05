-- init_schema: базовая схема Nexus.
--   auth     — users, refresh_tokens
--   контент  — entities (ядро), tags/entity_tags, people/entity_credits
--   social   — follows, reviews, collections/collection_items

CREATE EXTENSION IF NOT EXISTS pgcrypto;   -- gen_random_uuid()
CREATE EXTENSION IF NOT EXISTS citext;     -- регистронезависимый email
CREATE EXTENSION IF NOT EXISTS pg_trgm;    -- поиск по подстроке в названиях

-- Обновляет updated_at на любом UPDATE. Вешается триггером на все таблицы с этим полем.
CREATE OR REPLACE FUNCTION set_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------- auth

CREATE TABLE users (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email         citext      NOT NULL UNIQUE,
    username      citext      NOT NULL UNIQUE,
    password_hash text        NOT NULL,
    display_name  text,
    avatar_url    text,
    is_active     boolean     NOT NULL DEFAULT true,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER users_set_updated_at
    BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Храним только хеш токена: утечка таблицы не даёт возможности войти.
CREATE TABLE refresh_tokens (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash text        NOT NULL UNIQUE,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    user_agent text,
    ip         inet,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX refresh_tokens_user_id_idx ON refresh_tokens (user_id);
-- Для фоновой чистки протухших токенов.
CREATE INDEX refresh_tokens_expires_at_idx ON refresh_tokens (expires_at)
    WHERE revoked_at IS NULL;

-- ------------------------------------------------------------- контент

CREATE TYPE entity_kind AS ENUM ('movie', 'series', 'book', 'game');

CREATE TABLE entities (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind           entity_kind NOT NULL,
    slug           text        NOT NULL UNIQUE,
    title          text        NOT NULL,
    original_title text,
    description    text,
    release_date   date,
    cover_url      text,
    -- Специфика типа (длительность фильма, ISBN книги, платформы игры) — без ALTER TABLE на каждый тип.
    metadata       jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER entities_set_updated_at
    BEFORE UPDATE ON entities
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE INDEX entities_kind_idx         ON entities (kind);
CREATE INDEX entities_release_date_idx ON entities (release_date DESC NULLS LAST);
CREATE INDEX entities_metadata_gin     ON entities USING gin (metadata jsonb_path_ops);
CREATE INDEX entities_title_trgm       ON entities USING gin (title gin_trgm_ops);

CREATE TABLE tags (
    id   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug text NOT NULL UNIQUE,
    name text NOT NULL
);

CREATE TABLE entity_tags (
    entity_id uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    tag_id    uuid NOT NULL REFERENCES tags (id)     ON DELETE CASCADE,
    PRIMARY KEY (entity_id, tag_id)
);

-- Обратный обход: все сущности по тегу (прямой порядок покрыт первичным ключом).
CREATE INDEX entity_tags_tag_id_idx ON entity_tags (tag_id);

CREATE TABLE people (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug       text NOT NULL UNIQUE,
    full_name  text NOT NULL,
    birth_date date,
    photo_url  text,
    bio        text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER people_set_updated_at
    BEFORE UPDATE ON people
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE INDEX people_full_name_trgm ON people USING gin (full_name gin_trgm_ops);

CREATE TABLE entity_credits (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_id      uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    person_id      uuid NOT NULL REFERENCES people (id)   ON DELETE CASCADE,
    role           text NOT NULL,          -- actor, director, author, composer, ...
    character_name text,
    position       integer NOT NULL DEFAULT 0,
    UNIQUE (entity_id, person_id, role, character_name)
);

CREATE INDEX entity_credits_entity_id_idx ON entity_credits (entity_id, position);
CREATE INDEX entity_credits_person_id_idx ON entity_credits (person_id);

-- -------------------------------------------------------------- social

CREATE TABLE follows (
    follower_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    followee_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (follower_id, followee_id),
    CONSTRAINT follows_no_self CHECK (follower_id <> followee_id)
);

-- Лента подписчиков: кто подписан на пользователя.
CREATE INDEX follows_followee_id_idx ON follows (followee_id);

CREATE TABLE reviews (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    uuid     NOT NULL REFERENCES users (id)    ON DELETE CASCADE,
    entity_id  uuid     NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    rating     smallint,
    body       text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- Одна рецензия на пару пользователь-сущность.
    UNIQUE (user_id, entity_id),
    CONSTRAINT reviews_rating_range CHECK (rating IS NULL OR rating BETWEEN 1 AND 10),
    -- Пустая запись без оценки и текста смысла не имеет.
    CONSTRAINT reviews_not_empty CHECK (rating IS NOT NULL OR body IS NOT NULL)
);

CREATE TRIGGER reviews_set_updated_at
    BEFORE UPDATE ON reviews
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Рецензии конкретной сущности, свежие сверху.
CREATE INDEX reviews_entity_id_idx ON reviews (entity_id, created_at DESC);

CREATE TABLE collections (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    title       text NOT NULL,
    description text,
    is_public   boolean     NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER collections_set_updated_at
    BEFORE UPDATE ON collections
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE INDEX collections_user_id_idx ON collections (user_id);
CREATE INDEX collections_public_idx  ON collections (created_at DESC) WHERE is_public;

CREATE TABLE collection_items (
    collection_id uuid NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    entity_id     uuid NOT NULL REFERENCES entities (id)    ON DELETE CASCADE,
    position      integer     NOT NULL DEFAULT 0,
    note          text,
    added_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (collection_id, entity_id)
);

CREATE INDEX collection_items_entity_id_idx ON collection_items (entity_id);
CREATE INDEX collection_items_order_idx     ON collection_items (collection_id, position);
