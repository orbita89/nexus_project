-- Галерея человека: упорядоченный список ссылок, первая — главное фото. Flat First: `text[]` в
-- строке `people`, а не отдельная таблица. `photo_url` остаётся и всегда равен первому фото
-- (карточки, справочник `PersonDirectory`, индекс поиска читают его без изменений).
ALTER TABLE people ADD COLUMN photos text[] NOT NULL DEFAULT '{}';

UPDATE people SET photos = ARRAY[photo_url] WHERE photo_url IS NOT NULL;

-- Два поля не расходятся, кто бы ни писал (админка, импорты, дозаполнение из TMDB):
-- изменили `photos` — `photo_url` = первое фото; изменили только `photo_url` — галерея из одного фото.
CREATE FUNCTION people_sync_photos() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF cardinality(NEW.photos) > 0 THEN
            NEW.photo_url := NEW.photos[1];
        ELSIF NEW.photo_url IS NOT NULL THEN
            NEW.photos := ARRAY[NEW.photo_url];
        END IF;
    ELSIF NEW.photos IS DISTINCT FROM OLD.photos THEN
        NEW.photo_url := NEW.photos[1];
    ELSIF NEW.photo_url IS DISTINCT FROM OLD.photo_url THEN
        NEW.photos := CASE WHEN NEW.photo_url IS NULL THEN '{}'::text[] ELSE ARRAY[NEW.photo_url] END;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER people_sync_photos_trg
    BEFORE INSERT OR UPDATE OF photos, photo_url ON people
    FOR EACH ROW EXECUTE FUNCTION people_sync_photos();
