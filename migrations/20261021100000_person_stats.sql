-- people.original_name и person_stats: поиск людей в Meilisearch (индекс `people`).
--
-- original_name — имя латиницей (английский перевод TMDB), если оно отличается от `full_name`:
-- «Брэд Питт» находится и по «brad pitt». Заполняется из сырья `person_tmdb_data` (ниже) и
-- `tmdb_full.py --people`, правится в админке.
--
-- person_stats.fame — известность для ранжирования поиска людей: сумма по работам в активных
-- сущностях «вес роли × число оценок TMDB фильма» (`catalog::people_search::refresh_fame`).
-- Вес: режиссёр и первые 3 актёра — 1, актёры 4–10 — 0.4, остальные актёры — 0.1, сценарист и
-- композитор — 0.3, прочие — 0.15; за фильм берётся наибольший вес человека. known_for — роль
-- с наибольшим вкладом («актёр», «режиссёр», ...). Пересчитывается перестройкой поиска и
-- публикацией правок затронутых людей. Отдельная таблица, как entity_stats: пересчёт не трогает
-- строки people (их триггеры и индексы).

ALTER TABLE people ADD COLUMN original_name text;

UPDATE people p
SET original_name = t.name
FROM (
    SELECT d.person_id,
           (SELECT tr->'data'->>'name'
            FROM jsonb_array_elements(d.data->'translations'->'translations') tr
            WHERE tr->>'iso_639_1' = 'en' AND coalesce(tr->'data'->>'name', '') <> ''
            LIMIT 1) AS name
    FROM person_tmdb_data d
) t
WHERE t.person_id = p.id AND t.name IS NOT NULL AND t.name <> p.full_name;

CREATE TABLE person_stats (
    person_id  uuid PRIMARY KEY REFERENCES people (id) ON DELETE CASCADE,
    fame       double precision NOT NULL DEFAULT 0,
    known_for  text,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX person_stats_fame_idx ON person_stats (fame DESC, person_id);

CREATE TRIGGER person_stats_set_updated_at
    BEFORE UPDATE ON person_stats
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
