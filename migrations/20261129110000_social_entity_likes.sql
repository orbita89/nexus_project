-- «Нравится» у произведения: одна кнопка-сердечко рядом с «Оценить». Оценка 1–10 требует
-- решения и ставится редко, реакции — про настроение, отметки — про статус; простого «зашло»
-- не было. Дизлайка нет — как у людей и профилей (`person_likes`, `user_likes`).
CREATE TABLE entity_likes (
    entity_id  uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (entity_id, user_id)
);

-- «Что нравится» в профиле: лайки пользователя, новые сверху.
CREATE INDEX entity_likes_user_idx ON entity_likes (user_id, created_at DESC);
