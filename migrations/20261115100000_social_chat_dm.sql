-- social: чат, этап 8 — личные переписки 1:1, блокировки и настройка «кто может написать».
-- Переписка — строка `chat_rooms` того же протокола, что и остальные комнаты: канал `dm:<id>`,
-- та же история, та же отправка, те же `chat.message.*`. Пара хранится в канонической порядке
-- (`dm_user_a < dm_user_b`), поэтому на двоих ровно одна строка.
-- «Убрать переписку у себя» — существующая `chat_room_hidden`, скрывать ничего нового не надо.
-- Описание — .docs/backend/social.md.

ALTER TABLE chat_rooms
    ADD COLUMN dm_user_a uuid REFERENCES users (id) ON DELETE CASCADE,
    ADD COLUMN dm_user_b uuid REFERENCES users (id) ON DELETE CASCADE,
    DROP CONSTRAINT chat_rooms_one_target,
    -- Комната — ровно одного: сущности, человека, тега, своя (по slug) или переписка.
    ADD CONSTRAINT chat_rooms_one_target
        CHECK (num_nonnulls(entity_id, person_id, tag_id, slug, dm_user_a) = 1),
    -- Собеседника всегда два, и порядок канонический: на пару — одна строка.
    ADD CONSTRAINT chat_rooms_dm_pair
        CHECK ((dm_user_a IS NULL) = (dm_user_b IS NULL) AND (dm_user_a IS NULL OR dm_user_a < dm_user_b));

-- Переписка этих двоих — одна.
CREATE UNIQUE INDEX chat_rooms_dm_idx ON chat_rooms (dm_user_a, dm_user_b);
-- Мои переписки: обе стороны ищутся по своей колонке, без OR по индексам.
CREATE INDEX chat_rooms_dm_a_idx ON chat_rooms (dm_user_a) WHERE dm_user_a IS NOT NULL;
CREATE INDEX chat_rooms_dm_b_idx ON chat_rooms (dm_user_b) WHERE dm_user_b IS NOT NULL;
-- Сколько переписок пользователь завёл за сутки (лимит на рассылки).
CREATE INDEX chat_rooms_dm_started_idx ON chat_rooms (created_by, created_at DESC)
    WHERE dm_user_a IS NOT NULL;

-- Блокировки: односторонние, заблокированного не уведомляют.
CREATE TABLE user_blocks (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    blocked_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, blocked_id),
    CONSTRAINT user_blocks_not_self CHECK (user_id <> blocked_id)
);
-- «Кто меня заблокировал» — для профиля и поиска людей.
CREATE INDEX user_blocks_blocked_idx ON user_blocks (blocked_id);

-- Настройки чата. Строки заводятся по требованию: её отсутствие = значения по умолчанию.
CREATE TABLE user_chat_settings (
    user_id    uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- Кто может начать переписку: all (по умолчанию), following (только те, на кого я подписан), none.
    dm_policy  text NOT NULL DEFAULT 'all' CHECK (dm_policy IN ('all', 'following', 'none')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER user_chat_settings_updated_at BEFORE UPDATE ON user_chat_settings
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
