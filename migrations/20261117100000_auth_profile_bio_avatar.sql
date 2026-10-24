-- auth: «о себе», загруженное фото профиля и видимость личной статистики.
--
-- Фото: файл лежит на диске (`AVATARS_DIR`), в строке только `avatar_id` — им же называется файл.
-- Готовая ссылка остаётся в `avatar_url` (`/api/v1/auth/avatars/<id>`), поэтому все модули,
-- которые уже читают `avatar_url` через `UserRef`, работают без единой правки; у входа через
-- провайдера там по-прежнему внешний https-адрес.
--
-- `stats_public`: аналитика профиля (оценки, «просмотрено», «посмотрю позже») по умолчанию
-- видна только владельцу; владелец может открыть её всем.

ALTER TABLE users
    ADD COLUMN bio          text CHECK (bio IS NULL OR char_length(bio) <= 1000),
    ADD COLUMN stats_public boolean NOT NULL DEFAULT false,
    ADD COLUMN avatar_id    uuid;

-- Журнал загрузок: нужен только для лимита частоты (10 за 10 минут). Счётчики входа живут в
-- памяти процесса, а здесь запись и так идёт в той же транзакции, что и смена аватара.
-- Старые строки убирает фоновая уборка (`auth::cleanup`).
CREATE TABLE avatar_uploads (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX avatar_uploads_user_idx ON avatar_uploads (user_id, created_at DESC);
