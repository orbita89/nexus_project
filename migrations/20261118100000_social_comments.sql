-- social: комментарии ко всему, что не рецензия и не коллекция — к произведениям, людям и
-- студиям. Одна таблица с парой (`target_kind`, `target_id`) вместо трёх одинаковых: правила,
-- лимиты и модерация у них общие, а эндпоинт один (`/comments?target=entity:dune-2021`).
--
-- Строгого FK на `entities`/`people`/`studios` здесь быть не может: колонка одна, а таблиц три.
-- Существование цели проверяет код — через справочники `shared::directory` (модуль не читает
-- чужие таблицы), а осиротевшие строки после удаления цели отсекаются при выдаче: справочник
-- не вернёт ссылку, и комментарий в ответ не попадёт.
--
-- Комментарии к рецензиям (`review_comments`) и обсуждения коллекций (`collection_comments`)
-- остаются при своих таблицах: у них строгие FK и свои правила (закрытие обсуждения,
-- `reviews.comments_count`).

CREATE TABLE comments (
    id          uuid        PRIMARY KEY DEFAULT uuidv7(),
    target_kind text        NOT NULL CHECK (target_kind IN ('entity', 'person', 'studio')),
    target_id   uuid        NOT NULL,
    author_id   uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Корневой комментарий той же цели (проверяет код); NULL — сам корневой.
    parent_id   uuid        REFERENCES comments (id) ON DELETE CASCADE,
    body        text        NOT NULL CHECK (char_length(body) BETWEEN 1 AND 2000),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz
);

CREATE TRIGGER comments_set_updated_at
    BEFORE UPDATE ON comments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Лента комментариев цели по времени.
CREATE INDEX comments_target_idx ON comments (target_kind, target_id, created_at, id)
    WHERE deleted_at IS NULL;
-- Ответы корня (удаление вместе с корнем).
CREATE INDEX comments_parent_idx ON comments (parent_id)
    WHERE parent_id IS NOT NULL;
-- «Мои комментарии» в профиле и лимит частоты: комментарии автора, свежие сверху.
CREATE INDEX comments_author_idx ON comments (author_id, created_at DESC)
    WHERE deleted_at IS NULL;

-- Реакции на коллекцию: те же четыре, что и у произведения (`entity_reactions`).
-- Одна на зрителя, повторная заменяет.
CREATE TABLE collection_reactions (
    collection_id uuid        NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    user_id       uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind          text        NOT NULL CHECK (kind IN ('fire', 'sad', 'think', 'bored')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (collection_id, user_id)
);

CREATE INDEX collection_reactions_user_idx ON collection_reactions (user_id);
