-- Личная заметка о человеке каталога (режиссёре, актёре, авторе): как `entity_notes`, текст «для
-- себя», которого не видит никто, кроме автора. Отдельная таблица, а не общая с произведениями:
-- у каждой цели свой FK с CASCADE и своя UNIQUE-пара.
-- FK на `people` допустим: колонка-ссылка своя, а данные человека social читает справочником.
CREATE TABLE person_notes (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    person_id  uuid        NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    body       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, person_id)
);

-- «Мои заметки» о людях: свои заметки, новые сверху.
CREATE INDEX person_notes_user_idx ON person_notes (user_id, created_at DESC);

CREATE TRIGGER person_notes_set_updated_at
    BEFORE UPDATE ON person_notes
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
