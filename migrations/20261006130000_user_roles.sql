-- Роли пользователей. Порядок значений задаёт старшинство (user < author < admin),
-- поэтому в SQL можно писать role >= 'author'.
--   user   — отзывы, оценки, коллекции, ответы в темах форума
--   author — плюс создание форумов и тем
--   admin  — плюс админские эндпоинты
CREATE TYPE user_role AS ENUM ('user', 'author', 'admin');

ALTER TABLE users
    ADD COLUMN role user_role NOT NULL DEFAULT 'user';
