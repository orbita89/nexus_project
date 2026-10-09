-- Поиск пользователей в админке по подстроке username и email (ILIKE '%...%').
CREATE INDEX users_username_trgm ON users USING gin ((username::text) gin_trgm_ops);
CREATE INDEX users_email_trgm    ON users USING gin ((email::text) gin_trgm_ops);
