-- Импорт русских книг из SQL-дампа LibGen (`nexus books import`, `catalog::books`). Новых таблиц
-- нет: книга — строка `entities` (kind = 'book'), источник — `metadata.libgen`
-- (`{"work": "...", "files": [{"md5", "extension", "id"}]}`), авторы — `people` + `entity_credits`
-- (role = 'author'). Здесь только индексы для повторного импорта.

-- Известные файлы: `... ?| md5[]` по пачке.
CREATE INDEX entities_libgen_md5_idx ON entities
    USING gin (jsonb_path_query_array(metadata, '$.libgen.files[*].md5'))
    WHERE kind = 'book';

-- Другие файлы того же произведения находят книгу по ключу.
CREATE INDEX entities_libgen_work_idx ON entities ((metadata #>> '{libgen,work}'))
    WHERE kind = 'book' AND metadata ? 'libgen';

-- Автор из дампа находит уже созданного человека по имени без регистра и `ё`.
CREATE INDEX people_name_key_idx ON people ((replace(lower(full_name), 'ё', 'е')));
