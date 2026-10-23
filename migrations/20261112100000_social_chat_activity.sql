-- social: чат, этап 5 — активность комнат (`messages_24h`, сортировка `?sort=active`).
-- Счётчик не материализуем: он считается подзапросом по этому индексу (диапазон за сутки на
-- комнату). Пока комнат десятки, это дешевле отдельной колонки, которую нечем обнулять по
-- истечении суток. Описание — .docs/backend/social.md.

-- Сообщения комнаты за последние сутки: `messages_24h` и сортировка по активности.
CREATE INDEX chat_messages_room_recent_idx ON chat_messages (room_id, created_at DESC)
    WHERE deleted_at IS NULL;
