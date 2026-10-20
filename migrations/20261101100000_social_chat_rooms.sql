-- social: чат по интересам, этап 3 — комнаты, которые создаёт админ (`room:<slug>`), и участники.
-- Вступление в комнату — как интерес: комната в «моих чатах», канал realtime подписывается сам.
-- Описание — .docs/backend/social.md.

ALTER TABLE chat_rooms
    ADD COLUMN slug text UNIQUE,
    ADD COLUMN title text,
    ADD COLUMN description text,
    ADD COLUMN created_by uuid REFERENCES users (id) ON DELETE SET NULL,
    DROP CONSTRAINT chat_rooms_one_target,
    -- Комната — ровно одного: сущности, человека, тега или своя (по slug, с названием).
    ADD CONSTRAINT chat_rooms_one_target
        CHECK (num_nonnulls(entity_id, person_id, tag_id, slug) = 1),
    ADD CONSTRAINT chat_rooms_custom_title CHECK ((slug IS NULL) = (title IS NULL));

-- Участники своих комнат.
CREATE TABLE chat_room_members (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    room_id    uuid NOT NULL REFERENCES chat_rooms (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, room_id)
);
-- Свои комнаты пользователя, новые сверху.
CREATE INDEX chat_room_members_user_idx ON chat_room_members (user_id, created_at DESC);
-- Число участников комнаты.
CREATE INDEX chat_room_members_room_idx ON chat_room_members (room_id);
