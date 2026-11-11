-- Таблица `episodes` больше не нужна: серии живут в `entities.metadata.episodes`
-- (миграция `20261203100000_catalog_episodes_to_metadata.sql`).
--
-- Отдельным файлом — чтобы между переносом и дропом можно было сверить данные:
--     SELECT sum(jsonb_array_length(metadata -> 'episodes')) FROM entities WHERE kind = 'season';
--     SELECT count(*) FROM episodes;
-- Числа совпали — применять; нет — разбираться, данные ещё на месте.

DROP TABLE IF EXISTS episodes;
