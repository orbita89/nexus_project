-- social: читаемый адрес особой коллекции — /collections/russian-top-2021 (обычные остаются по
-- uuid). slug есть только у особой; старые slug — в истории, их нельзя занять другой коллекцией,
-- чтобы старые ссылки не вели на чужой контент. Описание — .docs/backend/social.md.

ALTER TABLE collections
    ADD COLUMN slug text UNIQUE,
    ADD CONSTRAINT collections_slug_featured_check
        CHECK (slug IS NULL OR featured_position IS NOT NULL);

-- Бывшие slug: смена адреса, снятие из особых, удаление коллекции. `collection_id` — NULL, если
-- коллекция удалена: slug остаётся занятым навсегда, по нему 404.
CREATE TABLE collection_slug_history (
    slug          text PRIMARY KEY,
    collection_id uuid REFERENCES collections (id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX collection_slug_history_collection_idx ON collection_slug_history (collection_id);

-- Удаляют коллекцию со slug (владелец или admin) — slug уходит в историю.
CREATE FUNCTION collections_keep_slug() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.slug IS NOT NULL THEN
        INSERT INTO collection_slug_history (slug, collection_id) VALUES (OLD.slug, NULL)
        ON CONFLICT (slug) DO UPDATE SET collection_id = NULL;
    END IF;
    RETURN OLD;
END $$;

CREATE TRIGGER collections_keep_slug BEFORE DELETE ON collections
    FOR EACH ROW EXECUTE FUNCTION collections_keep_slug();
