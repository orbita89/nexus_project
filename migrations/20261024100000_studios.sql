-- studios: страницы студий (кликнули по студии в карточке фильма — её страница и список фильмов).
--
-- Фильмы студии — по `entities.metadata.studios` (названия, как их отдаёт TMDB; правит админка):
-- `metadata @> '{"studios": ["<name>"]}'` идёт по GIN `entities_metadata_gin`, таблица связей не
-- нужна. Здесь — сама студия: `name` совпадает с названием в `metadata.studios`, slug для URL,
-- данные компании TMDB. Строки создаёт синхронизация `catalog::studios::sync` (новые названия из
-- `metadata.studios`; TMDB id, логотип и страна — из сырья `entity_tmdb_data`): при перестройке
-- поиска и после правок. Описание, штаб-квартира, сайт и материнская компания — из
-- `GET /3/company/{id}` при первом открытии страницы (`details_fetched_at`).

CREATE TABLE studios (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    slug               text NOT NULL UNIQUE,
    name               text NOT NULL UNIQUE,
    tmdb_id            integer UNIQUE,
    logo_url           text,
    origin_country     text,
    headquarters       text,
    homepage           text,
    description        text,
    parent_tmdb_id     integer,
    details_fetched_at timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER studios_set_updated_at
    BEFORE UPDATE ON studios
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
