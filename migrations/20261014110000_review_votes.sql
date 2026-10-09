-- Лайки и дизлайки рецензий. Счётчики денормализованы в reviews: списки рецензий их не считают.
-- Пишет их триггер на review_votes в той же транзакции, что и голос, — так они верны и при
-- каскадном удалении голосов.

ALTER TABLE reviews
    ADD COLUMN likes_count    bigint NOT NULL DEFAULT 0 CHECK (likes_count >= 0),
    ADD COLUMN dislikes_count bigint NOT NULL DEFAULT 0 CHECK (dislikes_count >= 0);

CREATE TABLE review_votes (
    review_id  uuid        NOT NULL REFERENCES reviews (id) ON DELETE CASCADE,
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    value      smallint    NOT NULL CHECK (value IN (1, -1)),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (review_id, user_id)
);

-- Свои голоса за рецензии сущности (votes/mine).
CREATE INDEX review_votes_user_idx ON review_votes (user_id);

CREATE FUNCTION review_votes_count() RETURNS trigger AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        UPDATE reviews SET
            likes_count    = likes_count    - (OLD.value = 1)::int,
            dislikes_count = dislikes_count - (OLD.value = -1)::int
        WHERE id = OLD.review_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        UPDATE reviews SET
            likes_count    = likes_count    + (NEW.value = 1)::int,
            dislikes_count = dislikes_count + (NEW.value = -1)::int
        WHERE id = NEW.review_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER review_votes_count
    AFTER INSERT OR UPDATE OR DELETE ON review_votes
    FOR EACH ROW EXECUTE FUNCTION review_votes_count();

-- updated_at рецензии — время правки автором (фронтенд пишет «изменено»): счётчики его не трогают.
DROP TRIGGER reviews_set_updated_at ON reviews;
CREATE TRIGGER reviews_set_updated_at
    BEFORE UPDATE ON reviews
    FOR EACH ROW
    WHEN ((OLD.rating, OLD.body, OLD.is_spoiler) IS DISTINCT FROM (NEW.rating, NEW.body, NEW.is_spoiler))
    EXECUTE FUNCTION set_updated_at();
