-- social: поиск коллекций по подстроке названия в админке (`GET /admin/collections?q=`, ILIKE).
-- Триграммы (pg_trgm уже включён каталогом) — без полного просмотра таблицы. Публичный поиск
-- коллекций — не здесь (публичные списки — через Meilisearch). Описание — .docs/backend/social.md.
CREATE INDEX collections_title_trgm ON collections USING gin (title gin_trgm_ops);
