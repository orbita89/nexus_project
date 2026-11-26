-- Единый поисковый индекс каталога (`catalog::index`): два пространства номеров.
--
-- Сущности (книги, фильмы, сериалы, игры…) — `catalog_entity_ords` (бывшая
-- `catalog_book_ords`, номера книг сохраняются). Люди — `catalog_person_ords`. Номер `ord`
-- (0, 1, 2, …) выдаётся один раз и не меняется: это индекс в плоских массивах слоя Б, бит в
-- RoaringBitmap и колонка `ord` в Tantivy. `text_hash` — отпечаток того, что документ кладёт в
-- Tantivy: совпал — документ не переписывается.
ALTER TABLE catalog_book_ords RENAME TO catalog_entity_ords;
ALTER TRIGGER catalog_book_ords_set_updated_at ON catalog_entity_ords
    RENAME TO catalog_entity_ords_set_updated_at;
ALTER INDEX catalog_book_ords_pkey RENAME TO catalog_entity_ords_pkey;
ALTER INDEX catalog_book_ords_ord_key RENAME TO catalog_entity_ords_ord_key;
ALTER TABLE catalog_entity_ords
    RENAME CONSTRAINT catalog_book_ords_entity_id_fkey TO catalog_entity_ords_entity_id_fkey;

CREATE TABLE catalog_person_ords (
    person_id  uuid PRIMARY KEY REFERENCES people (id) ON DELETE CASCADE,
    ord        integer GENERATED ALWAYS AS IDENTITY (MINVALUE 0 START WITH 0) UNIQUE,
    text_hash  bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER catalog_person_ords_set_updated_at
    BEFORE UPDATE ON catalog_person_ords
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
