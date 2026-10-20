-- social: чат по интересам, этап 1 — комната на каждую сущность каталога. Сообщения отправляются
-- по WebSocket (`chat.send`, realtime → shared::directory::Chat → social), история и настройки —
-- REST. Хранятся навсегда; удаление мягкое. Описание — .docs/backend/social.md.

-- Комната сущности: создаётся при первом сообщении (или заглушении). Счётчик и последнее
-- сообщение меняются в транзакции отправки (строка комнаты блокируется UPDATE).
CREATE TABLE chat_rooms (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    entity_id       uuid NOT NULL UNIQUE REFERENCES entities (id) ON DELETE CASCADE,
    messages_count  integer NOT NULL DEFAULT 0,
    last_message_id uuid,
    last_message_at timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER chat_rooms_set_updated_at BEFORE UPDATE ON chat_rooms
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Сообщения. id — uuidv7: растёт со временем, по нему история (курсор `before`) и прочитанное.
-- Удалённое: `deleted_at`, текст стирается (заглушка в истории, ответы на него не рвутся).
CREATE TABLE chat_messages (
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    room_id    uuid NOT NULL REFERENCES chat_rooms (id) ON DELETE CASCADE,
    author_id  uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    reply_to   uuid REFERENCES chat_messages (id) ON DELETE SET NULL,
    body       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    edited_at  timestamptz,
    deleted_at timestamptz,
    CHECK (deleted_at IS NOT NULL OR char_length(body) BETWEEN 1 AND 2000)
);

-- История комнаты, новые сверху.
CREATE INDEX chat_messages_room_idx ON chat_messages (room_id, id DESC);
-- Лимит частоты отправки: сообщения автора за последние секунды.
CREATE INDEX chat_messages_author_idx ON chat_messages (author_id, created_at DESC);

-- Прочитанное: последнее прочитанное сообщение пользователя в комнате (непрочитанные — новее).
CREATE TABLE chat_reads (
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    room_id      uuid NOT NULL REFERENCES chat_rooms (id) ON DELETE CASCADE,
    last_read_id uuid NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, room_id)
);

-- Заглушённые комнаты: события приходят, фронтенд не показывает уведомления.
CREATE TABLE chat_mutes (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    room_id    uuid NOT NULL REFERENCES chat_rooms (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, room_id)
);
