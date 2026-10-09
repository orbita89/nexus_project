-- Связанные произведения: сиквелы и ремейки. Храним одно направление: to — сиквел (ремейк) from.
-- Приквелы — обратное чтение sequel; ремейки показываются в обе стороны.
CREATE TABLE entity_relations (
    from_id    uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    to_id      uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    kind       text        NOT NULL CHECK (kind IN ('sequel', 'remake')),
    -- Порядок при одинаковой дате выхода.
    position   integer     NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (from_id, to_id, kind),
    CHECK (from_id <> to_id)
);

-- Обратное чтение: приквелы и оригинал ремейка.
CREATE INDEX entity_relations_to_idx ON entity_relations (to_id);
