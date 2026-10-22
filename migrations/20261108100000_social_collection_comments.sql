-- social: обсуждение коллекции — комментарии с ответами одного уровня (ответ на ответ привязывается
-- к корню в коде), мягкое удаление (удаление корня удаляет и ответы), закрытие обсуждения. Описание — .docs/backend/social.md.

CREATE TABLE collection_comments (
    id            uuid        PRIMARY KEY DEFAULT uuidv7(),
    collection_id uuid        NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    author_id     uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Корневой комментарий той же коллекции (проверяет код); NULL — сам корневой.
    parent_id     uuid        REFERENCES collection_comments (id) ON DELETE CASCADE,
    body          text        NOT NULL CHECK (char_length(body) BETWEEN 1 AND 2000),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    deleted_at    timestamptz
);

CREATE TRIGGER collection_comments_set_updated_at
    BEFORE UPDATE ON collection_comments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Обсуждение коллекции по времени.
CREATE INDEX collection_comments_collection_idx ON collection_comments (collection_id, created_at, id)
    WHERE deleted_at IS NULL;
-- Ответы корня (удаление вместе с корнем).
CREATE INDEX collection_comments_parent_idx ON collection_comments (parent_id)
    WHERE parent_id IS NOT NULL;
-- Лимит частоты: комментарии автора за последнюю минуту.
CREATE INDEX collection_comments_author_idx ON collection_comments (author_id, created_at DESC);

-- Закрытое обсуждение: читать можно, писать — только admin. Счётчик `comments_count` — подзапрос
-- по `collection_comments_collection_idx` (колонка с триггером трогала бы `updated_at` коллекции).
ALTER TABLE collections ADD COLUMN comments_locked boolean NOT NULL DEFAULT false;
