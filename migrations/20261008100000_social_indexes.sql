-- social: рецензии пользователя на его странице, свежие сверху.
-- UNIQUE (user_id, entity_id) находит рецензии пользователя, но не отдаёт их по дате.
CREATE INDEX reviews_user_id_idx ON reviews (user_id, created_at DESC);
