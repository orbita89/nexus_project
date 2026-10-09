-- Рецензия со спойлерами: фронтенд размывает текст, пока читатель его не откроет.
ALTER TABLE reviews ADD COLUMN is_spoiler boolean NOT NULL DEFAULT false;
