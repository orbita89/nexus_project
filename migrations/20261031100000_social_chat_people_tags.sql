-- social: чат по интересам, этап 2 — интересы к людям и тегам каталога и их комнаты чата.
-- Описание — .docs/backend/social.md.

-- Люди в интересах (автор, режиссёр, актёр): комнаты `person:<slug>` в «моих чатах».
CREATE TABLE user_person_interests (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    person_id  uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, person_id)
);
CREATE INDEX user_person_interests_user_id_idx ON user_person_interests (user_id, created_at DESC);
-- Кто интересуется человеком: `people_in_use` (catalog не удаляет таких при переносе в архив).
CREATE INDEX user_person_interests_person_id_idx ON user_person_interests (person_id);

-- Теги и жанры в интересах: комнаты `tag:<slug>`.
CREATE TABLE user_tag_interests (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    tag_id     uuid NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, tag_id)
);
CREATE INDEX user_tag_interests_user_id_idx ON user_tag_interests (user_id, created_at DESC);

-- Комната — ровно одного: сущности, человека или тега.
ALTER TABLE chat_rooms
    ALTER COLUMN entity_id DROP NOT NULL,
    ADD COLUMN person_id uuid UNIQUE REFERENCES people (id) ON DELETE CASCADE,
    ADD COLUMN tag_id uuid UNIQUE REFERENCES tags (id) ON DELETE CASCADE,
    ADD CONSTRAINT chat_rooms_one_target CHECK (num_nonnulls(entity_id, person_id, tag_id) = 1);
