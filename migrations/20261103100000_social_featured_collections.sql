-- social: особые коллекции — admin помечает публичную коллекцию (например «Топ‑10»), она видна
-- всем на главной и рисуется иначе. NULL — обычная коллекция; число — порядок среди особых.
ALTER TABLE collections
    ADD COLUMN featured_position integer,
    ADD CONSTRAINT collections_featured_public_check
        CHECK (featured_position IS NULL OR is_public);

-- Список особых по порядку.
CREATE INDEX collections_featured_idx ON collections (featured_position, created_at)
    WHERE featured_position IS NOT NULL;
