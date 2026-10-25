-- catalog: премии для вкладки «Награды» — справочник, как `studios`.
--
-- Сами награды фильма лежат в его строке (`entities.metadata.awards`: `{award, category, title,
-- year, won}`) — правило Flat First, отдельной таблицы связей нет. Здесь только сами премии:
-- slug для URL и название. Фильмы премии ищутся контейнментом по GIN-индексу `metadata`:
-- `metadata @> '{"awards": [{"award": "Оскар"}]}'`, как фильмы студии.
--
-- Строки создаёт `catalog::awards::sync` — по названиям `metadata.awards[].award`, которых ещё
-- нет: при перестройке поиска, после правок в админке и вручную
-- (`POST /admin/awards/sync` — им же пользуется импорт из Wikidata).

CREATE TABLE awards (
    id             uuid        PRIMARY KEY DEFAULT uuidv7(),
    slug           text        NOT NULL UNIQUE,
    name           text        NOT NULL UNIQUE,
    -- Сколько фильмов упоминают премию: порядок на вкладке и подсказка «не пусто ли».
    entities_count bigint      NOT NULL DEFAULT 0 CHECK (entities_count >= 0),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER awards_set_updated_at
    BEFORE UPDATE ON awards
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Список премий на вкладке: сначала те, у кого больше фильмов.
CREATE INDEX awards_count_idx ON awards (entities_count DESC, name);
