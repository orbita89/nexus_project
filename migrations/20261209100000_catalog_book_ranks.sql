-- Слой Б книжного поиска (`catalog::books::sidecar`): плотный номер книги и ручной порядок.
--
-- `catalog_book_ords` — номер книги `ord` (0, 1, 2, …): индекс в плоских массивах и бит в
-- RoaringBitmap слоя Б, колонка `ord` в Tantivy. Выдаётся один раз и не меняется: перестройка
-- индекса и слияние сегментов его не трогают, поэтому слой Б собирается из БД без чтения
-- Tantivy. `text_hash` — отпечаток того, что книга кладёт в Tantivy (карточка, серия): совпал —
-- документ не переписывается (новые оценки, теги и места идут только в слой Б).
CREATE TABLE catalog_book_ords (
    entity_id  uuid PRIMARY KEY REFERENCES entities (id) ON DELETE CASCADE,
    ord        integer GENERATED ALWAYS AS IDENTITY (MINVALUE 0 START WITH 0) UNIQUE,
    text_hash  bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER catalog_book_ords_set_updated_at
    BEFORE UPDATE ON catalog_book_ords
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Ручное место в общем списке книг: 1 — первое место выдачи «Рекомендуемые» и списков по
-- фасетам (внутри фасета — относительный порядок). Одна таблица на любые сущности: книги —
-- первый шаг, дальше тот же слой Б для остальных разделов. Место уникально.
CREATE TABLE catalog_manual_ranks (
    entity_id  uuid PRIMARY KEY REFERENCES entities (id) ON DELETE CASCADE,
    rank       integer NOT NULL UNIQUE CHECK (rank BETWEEN 1 AND 65535),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER catalog_manual_ranks_set_updated_at
    BEFORE UPDATE ON catalog_manual_ranks
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
