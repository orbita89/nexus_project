-- Картины (раздел «Картины», scripts/catalog-import/paintings_wikidata.py): новый тип сущности
-- и уникальность по id Wikidata, чтобы повторный импорт находил уже записанные полотна.
--
-- ALTER TYPE ... ADD VALUE работает внутри транзакции (PostgreSQL 12+), пока новое значение не
-- используется в ней же: индекс ниже значения не касается.
ALTER TYPE entity_kind ADD VALUE IF NOT EXISTS 'painting';

CREATE UNIQUE INDEX entities_external_wikidata_uq ON entities ((external_ids ->> 'wikidata'))
    WHERE external_ids ? 'wikidata';
