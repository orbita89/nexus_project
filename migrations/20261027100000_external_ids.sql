-- entities.external_ids: id сущности во внешних сервисах прямо в строке (`{"tmdb": 550}`) вместо
-- отдельной таблицы на каждый сервис. Новый источник — новый ключ, без миграции схемы.
--
-- Удаляются таблицы, завязанные на TMDB:
--   entity_tmdb       → entities.external_ids.tmdb;
--   archived_tmdb     → сам индекс Tantivy (`nexus archive ids`; флаг ручного переноса — поле
--                       `manual` документа архива);
--   entity_tmdb_data,
--   person_tmdb_data  → сырые ответы TMDB больше не храним: нужное уже разложено в `entities`,
--                       `people`, `studios`, `entity_credits`, `entity_tags`, `entity_stats`.

ALTER TABLE entities ADD COLUMN external_ids jsonb NOT NULL DEFAULT '{}';

-- Перенос без триггера updated_at: карточки не менялись.
ALTER TABLE entities DISABLE TRIGGER entities_set_updated_at;
UPDATE entities e SET external_ids = jsonb_build_object('tmdb', t.tmdb_id)
FROM entity_tmdb t WHERE t.entity_id = e.id;
ALTER TABLE entities ENABLE TRIGGER entities_set_updated_at;

-- Один TMDB id — одна сущность; запросы пишут то же условие `external_ids ? 'tmdb'`.
CREATE UNIQUE INDEX entities_external_tmdb_uq ON entities (((external_ids ->> 'tmdb')::int))
    WHERE external_ids ? 'tmdb';

DROP TABLE entity_tmdb;
DROP TABLE archived_tmdb;
DROP TABLE entity_tmdb_data;
DROP TABLE person_tmdb_data;
