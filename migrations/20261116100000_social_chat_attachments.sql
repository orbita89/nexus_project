-- social: чат, этап 9 — вложения в сообщениях (только картинки).
-- Файл не хранится в БД: на диске лежит пережатый нами WebP, здесь — метаданные и владелец.
-- Раздача только через `GET /chat/attachments/{id}` с проверкой доступа к комнате: вложение
-- из личной переписки не должно открываться по прямой ссылке. Описание — .docs/backend/social.md.

CREATE TABLE chat_attachments (
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    -- Кто загрузил: он же единственный, кто может приложить вложение к сообщению.
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- К какому сообщению приложено. NULL — загружено, но ещё не отправлено (или сообщение
    -- удалено): такие строки чистит `cleanup_orphans`.
    message_id uuid UNIQUE REFERENCES chat_messages (id) ON DELETE CASCADE,
    -- Размеры готового файла и его вес на диске.
    width      int NOT NULL,
    height     int NOT NULL,
    size       bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
-- Неприкреплённые вложения старше часа — мусор.
CREATE INDEX chat_attachments_orphan_idx ON chat_attachments (created_at) WHERE message_id IS NULL;
-- Лимит частоты загрузок.
CREATE INDEX chat_attachments_user_idx ON chat_attachments (user_id, created_at DESC);

-- Сообщение может быть без текста, если у него есть вложение. Проверку «тело 1–2000»
-- заменяем на «тело в пределах 0–2000», пустое тело разрешает уже сам обработчик:
-- CHECK не видит вложение (оно в другой таблице и пишется после сообщения).
ALTER TABLE chat_messages
    DROP CONSTRAINT chat_messages_check,
    ADD CONSTRAINT chat_messages_body_check
        CHECK (deleted_at IS NOT NULL OR char_length(body) <= 2000);
