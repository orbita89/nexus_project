-- social: чат, этап 6 — поиск по комнатам, по сообщениям и по пользователям (для «@»).
-- Всё на PostgreSQL: pg_trgm (уже включён каталогом) для коротких полей и полнотекстовый
-- индекс для тел сообщений. Meilisearch здесь не нужен — комнат десятки, а сообщения ищутся
-- всегда внутри ограниченного набора комнат. Описание — .docs/backend/social.md.

-- Поиск комнат по названию и описанию (ILIKE '%...%' + similarity для порядка).
CREATE INDEX chat_rooms_title_trgm       ON chat_rooms USING gin (title gin_trgm_ops);
CREATE INDEX chat_rooms_description_trgm ON chat_rooms USING gin (description gin_trgm_ops);

-- Поиск по телам сообщений: по словам, с учётом русской морфологии. Удалённые не индексируются
-- (их тело пустое и в выдаче им делать нечего).
CREATE INDEX chat_messages_body_fts ON chat_messages
    USING gin (to_tsvector('russian', body)) WHERE deleted_at IS NULL;

-- Автодополнение «@»: username уже покрыт users_username_trgm, нужно имя.
CREATE INDEX users_display_name_trgm ON users USING gin (display_name gin_trgm_ops);
