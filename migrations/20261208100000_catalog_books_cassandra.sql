-- Книги из библиотеки Cassandra (`nexus books cassandra`, `catalog::books::cassandra`).
--
-- Источник — `external_ids.cassandra` (номер книги в библиотеке): по нему повторный импорт
-- узнаёт уже загруженные книги. Уникальный индекс — как у `external_ids.tmdb`.
CREATE UNIQUE INDEX entities_external_cassandra_uq
    ON entities (((external_ids ->> 'cassandra')::bigint))
    WHERE external_ids ? 'cassandra';

-- Перестройка книжного индекса читает книги по порядку id пачками (keyset): без индекса по
-- (kind, id) каждая пачка была бы обходом всей таблицы (сотни тысяч сезонов и фильмов).
CREATE INDEX entities_book_id_idx ON entities (id) WHERE kind = 'book';
