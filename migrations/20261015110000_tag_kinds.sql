-- Жанры отдельно от тегов: «Драма» и «Космическая опера» — жанры (в шапке карточки), «Экранизация»
-- и «Открытый мир» — теги. Какие теги жанры — решает админка; сидовые размечены в seeds/catalog.sql.
ALTER TABLE tags ADD COLUMN kind text NOT NULL DEFAULT 'tag' CHECK (kind IN ('genre', 'tag'));
