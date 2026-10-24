-- social: рейтинг пользователя («карма»). Полный пересчёт раз в сутки, не счётчик на триггерах:
-- слагаемых будет больше (каналы, темы), веса будут меняться, а пересчёт с нуля всегда даёт
-- одно и то же число — его не нужно чинить после любой правки правил.
--
-- Строка заводится только на тех, у кого есть хоть одно слагаемое: у остальных рейтинг —
-- стартовые 30 за регистрацию, и хранить его незачем. Пересчёт стирает строки, у которых
-- слагаемых не осталось.
--
-- `parts` — разбивка `{"profile_likes": 3, "review_likes": 12, "collection_reactions": 2}`:
-- по ней видно, из чего сложился балл, и можно показать её в профиле. jsonb, а не колонки:
-- новое слагаемое не потребует миграции.

CREATE TABLE user_reputation (
    user_id     uuid        PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- Стартовые 30 уже включены.
    score       integer     NOT NULL CHECK (score >= 0),
    parts       jsonb       NOT NULL DEFAULT '{}'::jsonb,
    computed_at timestamptz NOT NULL DEFAULT now()
);

-- Таблицы лидеров и «рейтинг выше N», плюс отсев строк, не обновлённых пересчётом.
CREATE INDEX user_reputation_score_idx ON user_reputation (score DESC, user_id);
CREATE INDEX user_reputation_computed_idx ON user_reputation (computed_at);
