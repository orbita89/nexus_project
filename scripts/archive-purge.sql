-- Массовое удаление из PostgreSQL фильмов, уже перенесённых в холодный архив
-- (`nexus archive build --defer-delete`): строки `entities`, чей TMDB id есть в `archived_tmdb`.
--
-- Зачем отдельно: построчное удаление миллиона строк — 15 FK-каскадов и триггер счётчиков на
-- каждый из ~7 млн титров — идёт часами. Здесь всё set-based в одной транзакции: каскады и
-- триггеры выключены (`session_replication_role = replica`, нужен суперпользователь), дети
-- удаляются явно, счётчики людей и тегов пересчитываются, в конце проверяется, что ни одна
-- ссылка на `entities`/`people` не повисла (иначе — ROLLBACK).
--
-- Не удаляются: `active_manual` и сущности с пользовательскими данными (сборка их в архив и не
-- кладёт; проверка повторяется). Запись в каталог на время транзакции заблокирована
-- (`EXCLUSIVE`), чтение идёт.
--
-- Запуск: docker exec -i infra-postgres-1 psql -v ON_ERROR_STOP=1 -U nexus_user -d nexus_db \
--           < scripts/archive-purge.sql
-- Затем: VACUUM FULL (место на диске) и перестройка поиска.

BEGIN;
SET LOCAL session_replication_role = replica;
LOCK TABLE entities, people IN EXCLUSIVE MODE;

CREATE TEMP TABLE purge_entities ON COMMIT DROP AS
SELECT e.id
FROM entities e
JOIN entity_tmdb t ON t.entity_id = e.id
JOIN archived_tmdb a ON a.tmdb_id = t.tmdb_id
WHERE NOT e.active_manual
  AND NOT EXISTS (SELECT 1 FROM user_interests x WHERE x.entity_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM reviews x WHERE x.entity_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM collection_items x WHERE x.entity_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM entity_marks x WHERE x.entity_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM entity_reactions x WHERE x.entity_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM forum_thread_entities x WHERE x.entity_id = e.id);
ALTER TABLE purge_entities ADD PRIMARY KEY (id);
ANALYZE purge_entities;

CREATE TEMP TABLE purge_touched_people ON COMMIT DROP AS
SELECT DISTINCT c.person_id AS id
FROM entity_credits c JOIN purge_entities d ON d.id = c.entity_id;

-- Дети `entities` (каталог и служебные схемы импорта).
DELETE FROM entity_credits x USING purge_entities d WHERE x.entity_id = d.id;
DELETE FROM entity_tags x USING purge_entities d WHERE x.entity_id = d.id;
DELETE FROM entity_stats x USING purge_entities d WHERE x.entity_id = d.id;
DELETE FROM entity_tmdb x USING purge_entities d WHERE x.entity_id = d.id;
DELETE FROM entity_ratings x USING purge_entities d WHERE x.entity_id = d.id;
DELETE FROM entity_relations x USING purge_entities d WHERE x.from_id = d.id OR x.to_id = d.id;
DO $$
BEGIN
    IF to_regclass('catalog_import.movies') IS NOT NULL THEN
        DELETE FROM catalog_import.movies x USING purge_entities d WHERE x.entity_id = d.id;
    END IF;
    IF to_regclass('wikidata_import.movies') IS NOT NULL THEN
        DELETE FROM wikidata_import.movies x USING purge_entities d WHERE x.entity_id = d.id;
    END IF;
END $$;
DELETE FROM entities x USING purge_entities d WHERE x.id = d.id;

-- Люди, у которых не осталось ни одной работы.
CREATE TEMP TABLE purge_people ON COMMIT DROP AS
SELECT p.id FROM purge_touched_people p
WHERE NOT EXISTS (SELECT 1 FROM entity_credits c WHERE c.person_id = p.id);
ALTER TABLE purge_people ADD PRIMARY KEY (id);
DELETE FROM person_credit_counts x USING purge_people d WHERE x.person_id = d.id;
DO $$
BEGIN
    IF to_regclass('catalog_import.people') IS NOT NULL THEN
        DELETE FROM catalog_import.people x USING purge_people d WHERE x.person_id = d.id;
    END IF;
    IF to_regclass('wikidata_import.people') IS NOT NULL THEN
        DELETE FROM wikidata_import.people x USING purge_people d WHERE x.person_id = d.id;
    END IF;
END $$;
DELETE FROM people x USING purge_people d WHERE x.id = d.id;

-- Счётчики — как в миграции 20261016100000_entity_active.sql.
UPDATE person_credit_counts pc
SET credits = s.credits, active_credits = s.active_credits
FROM (
    SELECT p.id AS person_id,
           count(c.entity_id) AS credits,
           count(c.entity_id) FILTER (WHERE e.active) AS active_credits
    FROM purge_touched_people p
    LEFT JOIN entity_credits c ON c.person_id = p.id
    LEFT JOIN entities e ON e.id = c.entity_id
    GROUP BY p.id
) s
WHERE pc.person_id = s.person_id
  AND (pc.credits, pc.active_credits) IS DISTINCT FROM (s.credits, s.active_credits);

UPDATE tags t
SET entities_count = (
    SELECT count(*) FROM entity_tags et JOIN entities e ON e.id = et.entity_id
    WHERE et.tag_id = t.id AND e.active
);

-- Защита: ни одна ссылка на `entities` и `people` не повисла.
DO $$
DECLARE
    fk record;
    dangling bigint;
BEGIN
    FOR fk IN
        SELECT c.conrelid::regclass AS child, a.attname AS col, c.confrelid::regclass AS parent
        FROM pg_constraint c
        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1]
        WHERE c.contype = 'f' AND c.confrelid IN ('entities'::regclass, 'people'::regclass)
    LOOP
        EXECUTE format(
            'SELECT count(*) FROM %s x WHERE x.%I IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM %s p WHERE p.id = x.%I)',
            fk.child, fk.col, fk.parent, fk.col) INTO dangling;
        IF dangling > 0 THEN
            RAISE EXCEPTION 'archive purge: % rows of % reference missing %',
                dangling, fk.child, fk.parent;
        END IF;
    END LOOP;
END $$;

SELECT (SELECT count(*) FROM purge_entities) AS entities_deleted,
       (SELECT count(*) FROM purge_people) AS people_deleted,
       (SELECT count(*) FROM entities) AS entities_left,
       (SELECT count(*) FROM entities WHERE active) AS active_left;

COMMIT;
