-- Личная заметка о произведении: текст «для себя», которого не видит никто, кроме автора.
-- Отдельная запись, а не флаг приватности у рецензии: публичный отзыв и своя пометка нужны
-- человеку одновременно, флаг заставил бы выбирать. Оценка остаётся в `reviews` и публична.
CREATE TABLE entity_notes (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    entity_id  uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    body       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, entity_id)
);

-- Вкладка «Заметки» в своём профиле: свои заметки, новые сверху.
CREATE INDEX entity_notes_user_idx ON entity_notes (user_id, created_at DESC);

CREATE TRIGGER entity_notes_set_updated_at
    BEFORE UPDATE ON entity_notes
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
