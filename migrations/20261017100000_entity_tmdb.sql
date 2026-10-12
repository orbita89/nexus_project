-- entity_tmdb: связь сущностей каталога с TMDB и ручное переключение `active` — для операций
-- админки «обновить popularity» и «пересобрать активные» (`catalog::tmdb`). Раньше связь была
-- только в служебной схеме скрипта импорта, которую приложение не читает.

-- Сущность ↔ фильм TMDB. Пишет импорт (`scripts/catalog-import/merge.py`).
CREATE TABLE entity_tmdb (
    entity_id uuid    PRIMARY KEY REFERENCES entities (id) ON DELETE CASCADE,
    tmdb_id   integer NOT NULL UNIQUE
);

-- Включили или скрыли в админке — «пересобрать активные» её не трогает.
ALTER TABLE entities ADD COLUMN active_manual boolean NOT NULL DEFAULT false;

-- Перенос из импорта, если он был: связи; «ручные» — привязанные готовые строки (сиды, админка)
-- и переключённые после раскладки скрипта.
DO $$
BEGIN
    IF to_regclass('catalog_import.movies') IS NOT NULL THEN
        INSERT INTO entity_tmdb (entity_id, tmdb_id)
        SELECT entity_id, tmdb_id FROM catalog_import.movies WHERE tmdb_id IS NOT NULL
        ON CONFLICT DO NOTHING;
        UPDATE entities e SET active_manual = true
        FROM catalog_import.movies m
        WHERE m.entity_id = e.id
          AND (NOT m.created
               OR (m.written ? 'active' AND (m.written->>'active')::boolean <> e.active));
    END IF;
END;
$$;

-- Смена `active`: был построчный триггер — массовое переключение (сотни тысяч строк) переписывало
-- одни и те же строки тегов на каждой строке. Теперь операторный: один пересчёт на весь UPDATE
-- по transition tables (с ними нельзя `UPDATE OF active`, поэтому сравнение внутри).
DROP TRIGGER entities_active_changed ON entities;

CREATE FUNCTION entities_active_changed() RETURNS trigger AS $$
BEGIN
    WITH d AS (
        SELECT n.id, CASE WHEN n.active THEN 1 ELSE -1 END AS delta
        FROM new_rows n JOIN old_rows o ON o.id = n.id
        WHERE o.active IS DISTINCT FROM n.active
    ),
    moved AS (
        UPDATE person_credit_counts pc SET active_credits = pc.active_credits + s.delta
        FROM (SELECT c.person_id, sum(d.delta) AS delta
              FROM d JOIN entity_credits c ON c.entity_id = d.id
              GROUP BY c.person_id) s
        WHERE pc.person_id = s.person_id
        RETURNING 1
    )
    UPDATE tags t SET entities_count = t.entities_count + s.delta
    FROM (SELECT et.tag_id, sum(d.delta) AS delta
          FROM d JOIN entity_tags et ON et.entity_id = d.id
          GROUP BY et.tag_id) s
    WHERE t.id = s.tag_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER entities_active_changed
    AFTER UPDATE ON entities
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION entities_active_changed();
