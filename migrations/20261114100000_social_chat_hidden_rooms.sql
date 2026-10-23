-- social: чат, этап 7 — скрытые комнаты в «Моих чатах».
-- Из системной комнаты нельзя выйти (в ней состоят все), но держать все пять в списке
-- пользователь не обязан. Скрытая не приходит в `GET /chats`, но остаётся полностью рабочей:
-- история, отправка и подписка на канал не меняются — иначе пропали бы упоминания.
-- Описание — .docs/backend/social.md.

CREATE TABLE chat_room_hidden (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    room_id    uuid NOT NULL REFERENCES chat_rooms (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, room_id)
);
