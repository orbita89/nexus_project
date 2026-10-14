-- catalog_import.people: связь людей с TMDB / Wikidata. Таблицу создаёт и ведёт импорт
-- (`scripts/catalog-import/merge.py`, STATE_SCHEMA — то же определение); с возвратом фильмов из
-- архива её читает и пишет приложение (`catalog::archive::enrich`: люди из титров TMDB по их
-- TMDB id, новых — создаёт). IF NOT EXISTS: в dev/prod она уже есть после импорта.

CREATE SCHEMA IF NOT EXISTS catalog_import;

CREATE TABLE IF NOT EXISTS catalog_import.people (
    person_id uuid    PRIMARY KEY REFERENCES public.people (id) ON DELETE CASCADE,
    tmdb_id   integer UNIQUE,
    qid       text    UNIQUE,
    created   boolean NOT NULL,
    written   jsonb   NOT NULL DEFAULT '{}'
);
