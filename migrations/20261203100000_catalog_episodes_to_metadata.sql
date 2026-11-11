-- Серии сезона переезжают из таблицы `episodes` в `entities.metadata.episodes`.
--
-- Серия — не сущность: её не ищут, не оценивают и не обсуждают, оценки и рецензии осмысленны на
-- уровне сезона. Строками она стоила слишком дорого: на 1,26 млн серий у 9 тыс. сериалов это
-- уже больше всего остального каталога, а на полном дампе TMDB (236 тыс. сериалов) вышло бы
-- порядка 150 млн строк ради списка под карточкой сезона. Сезон сущностью остаётся.
--
-- Таблица `episodes` не удаляется здесь: дроп — отдельной миграцией, после сверки
--     SELECT sum(jsonb_array_length(metadata -> 'episodes')) FROM entities WHERE kind = 'season';
--     SELECT count(*) FROM episodes;
-- так откат возможен без потери данных.
--
-- ВАЖНО, выполнить ДО применения этой миграции (вне транзакции, иначе DROP + CREATE GIN держат
-- AccessExclusiveLock на entities всё время построения индекса — это простой каталога):
--     CREATE INDEX CONCURRENTLY entities_metadata_gin_new
--         ON entities USING gin ((metadata - 'episodes') jsonb_path_ops);
-- Индекс по выражению без серий: иначе GIN распухнет на миллионы токенов из описаний серий, а
-- выборки `metadata @> ...` (фильтр студий) перестанут его использовать — они тоже переведены
-- на `(metadata - 'episodes') @> ...`.
--
-- После применения — вручную: VACUUM (ANALYZE) entities; (UPDATE оставляет мёртвые версии).

SET LOCAL work_mem = '256MB';

WITH agg AS (
    SELECT season_id,
           -- `- 'season_id'` снимает с объекта серии ключ, по которому группируем: в metadata
           -- сезона он не нужен.
           jsonb_agg(jsonb_strip_nulls(to_jsonb(episode) - 'season_id') ORDER BY number)
               AS episodes
      FROM (SELECT season_id, id, number, title, original_title, overview, air_date,
                   runtime_minutes, still_url, still_width, still_height, created_at, updated_at
              FROM episodes) episode
     GROUP BY season_id
)
UPDATE entities e
   SET metadata = e.metadata || jsonb_build_object('episodes', agg.episodes)
  FROM agg
 WHERE e.id = agg.season_id;

-- Если индекс по выражению заранее не создан (CONCURRENTLY, см. шапку) — создаём здесь, чтобы
-- каталог не остался вообще без GIN. На пустой базе тестов это мгновенно, на проде — простой,
-- поэтому рецепт в шапке и нужен.
DO $$
BEGIN
    IF to_regclass('entities_metadata_gin_new') IS NULL THEN
        CREATE INDEX entities_metadata_gin_new
            ON entities USING gin ((metadata - 'episodes') jsonb_path_ops);
    END IF;
END
$$;

DROP INDEX IF EXISTS entities_metadata_gin;
ALTER INDEX entities_metadata_gin_new RENAME TO entities_metadata_gin;
