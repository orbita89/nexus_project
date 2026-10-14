-- entity_tmdb_data, person_tmdb_data: полный ответ TMDB по фильму горячего слоя и по человеку —
-- всё, что отдаёт API (`scripts/catalog-import/tmdb_full.py`). Из него скрипт раскладывает
-- нужное в `entities`, `entity_credits`, `people`, `entity_tags`, `entity_stats`,
-- `entity_relations`; остальное (переводы, картинки, ключевые слова, где смотреть, рекомендации,
-- рецензии TMDB, …) лежит здесь до появления в API. Приложение эти таблицы пока не читает.
--
-- Ответы большие (фильм — 50–400 КБ JSON), поэтому сжатие lz4: быстрее pglz при той же степени.
-- Удалить всё сырьё — TRUNCATE обеих таблиц: разложенные поля останутся.

CREATE TABLE entity_tmdb_data (
    entity_id  uuid PRIMARY KEY REFERENCES entities (id) ON DELETE CASCADE,
    tmdb_id    integer NOT NULL,
    data       jsonb NOT NULL,           -- GET /movie/{id}?language=ru-RU&append_to_response=…
    fetched_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE entity_tmdb_data ALTER COLUMN data SET COMPRESSION lz4;

CREATE TABLE person_tmdb_data (
    person_id  uuid PRIMARY KEY REFERENCES people (id) ON DELETE CASCADE,
    tmdb_id    integer NOT NULL,
    data       jsonb NOT NULL,           -- GET /person/{id}?language=ru-RU&append_to_response=…
    fetched_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE person_tmdb_data ALTER COLUMN data SET COMPRESSION lz4;
