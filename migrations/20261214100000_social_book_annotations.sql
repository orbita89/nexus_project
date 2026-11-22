-- Закладки и выделения в читалке книг: личные, видит только автор (как entity_notes).
--   bookmark  — место в книге (cfi — точка);
--   highlight — выделенный фрагмент (cfi — диапазон, text — сам фрагмент), цвет 0–6 и
--               необязательная заметка.
-- Страница книги статическая: всё это фронт берёт по API после входа.
CREATE TABLE book_annotations (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    entity_id  uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    kind       text        NOT NULL CHECK (kind IN ('bookmark', 'highlight')),
    -- EPUB CFI: точка (закладка) или диапазон (выделение) — по нему читалка находит место.
    cfi        text        NOT NULL CHECK (length(cfi) BETWEEN 1 AND 2000),
    -- Выделенный текст (у выделения обязателен) — чтобы показать цитату без открытия книги.
    text       text        CHECK (length(text) <= 2000),
    color      smallint    CHECK (color BETWEEN 0 AND 6),
    note       text        CHECK (length(note) <= 5000),
    -- Глава и доля прочитанного (0–1) на момент создания: подпись и порядок в списке.
    chapter    text        CHECK (length(chapter) <= 300),
    fraction   real        CHECK (fraction BETWEEN 0 AND 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (kind = 'bookmark' OR (text IS NOT NULL AND color IS NOT NULL))
);

-- Все пометки книги у читателя: читалка и блок на странице книги.
CREATE INDEX book_annotations_book_idx ON book_annotations (user_id, entity_id);
-- Профиль: свои пометки, новые сверху.
CREATE INDEX book_annotations_user_idx ON book_annotations (user_id, created_at DESC);
-- Повторная закладка на то же место — та же запись.
CREATE UNIQUE INDEX book_annotations_bookmark_uniq
    ON book_annotations (user_id, entity_id, cfi) WHERE kind = 'bookmark';

CREATE TRIGGER book_annotations_set_updated_at
    BEFORE UPDATE ON book_annotations
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
