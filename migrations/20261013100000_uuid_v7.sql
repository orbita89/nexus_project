-- uuid_v7: новые первичные ключи — UUID v7 (PostgreSQL 18, встроенная uuidv7()).
-- Почему — documents/architecture.md («База данных: общие соглашения»).
--
-- В v7 первые 48 бит — время в миллисекундах: новые строки ложатся в конец индекса первичного
-- ключа, а не в случайную страницу, как у v4. Существующие v4-ключи остаются как есть: тип тот же,
-- внешние ключи и API не меняются.

ALTER TABLE users          ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE refresh_tokens ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE email_tokens   ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE entities       ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE tags           ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE people         ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE entity_credits ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE reviews        ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE collections    ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE forum_threads  ALTER COLUMN id SET DEFAULT uuidv7();
ALTER TABLE forum_posts    ALTER COLUMN id SET DEFAULT uuidv7();
