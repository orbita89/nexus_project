-- social: лайки профилей. Только лайк — дизлайка у пользователя нет: оценивают человека, а не
-- рецензию, и минус здесь работал бы как травля.
--
-- Своему профилю лайк не поставить (CHECK), повторный невозможен (PK). Отсюда же берётся часть
-- рейтинга пользователя (этап «карма»): лайк профиля — одно из слагаемых.

CREATE TABLE user_likes (
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    liked_id   uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, liked_id),
    CONSTRAINT user_likes_not_self CHECK (user_id <> liked_id)
);

-- «Сколько лайков у этого профиля» и «кто его лайкнул», новые сверху.
CREATE INDEX user_likes_liked_idx ON user_likes (liked_id, created_at DESC);
