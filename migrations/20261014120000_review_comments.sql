-- Комментарии к рецензиям. Удалённый комментарий остаётся строкой с deleted_at и из списков
-- пропадает. comments_count в reviews (без удалённых) пишет триггер в той же транзакции.

CREATE TABLE review_comments (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    review_id  uuid        NOT NULL REFERENCES reviews (id) ON DELETE CASCADE,
    author_id  uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    body       text        NOT NULL CHECK (char_length(body) BETWEEN 1 AND 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz
);

CREATE TRIGGER review_comments_set_updated_at
    BEFORE UPDATE ON review_comments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Комментарии рецензии по времени.
CREATE INDEX review_comments_review_idx ON review_comments (review_id, created_at)
    WHERE deleted_at IS NULL;

ALTER TABLE reviews
    ADD COLUMN comments_count bigint NOT NULL DEFAULT 0 CHECK (comments_count >= 0);

CREATE FUNCTION review_comments_count() RETURNS trigger AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') AND OLD.deleted_at IS NULL THEN
        UPDATE reviews SET comments_count = comments_count - 1 WHERE id = OLD.review_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') AND NEW.deleted_at IS NULL THEN
        UPDATE reviews SET comments_count = comments_count + 1 WHERE id = NEW.review_id;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER review_comments_count
    AFTER INSERT OR DELETE OR UPDATE OF deleted_at ON review_comments
    FOR EACH ROW EXECUTE FUNCTION review_comments_count();
