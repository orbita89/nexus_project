-- Личные отметки: «Посмотреть позже» (later) и «Просмотрено» (done) — у книг «прочитать», у игр
-- «пройти». Взаимоисключающие: одна строка на пару, смена отметки меняет kind.
CREATE TABLE entity_marks (
    entity_id  uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       text        NOT NULL CHECK (kind IN ('later', 'done')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (entity_id, user_id)
);

CREATE TRIGGER entity_marks_set_updated_at
    BEFORE UPDATE ON entity_marks
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Списки в личном кабинете: свои отметки вида kind, свежие сверху.
CREATE INDEX entity_marks_user_idx ON entity_marks (user_id, kind, created_at DESC);
