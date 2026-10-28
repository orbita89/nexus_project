-- catalog: раздел «Награды» как сущности — премия, церемония, категория, номинация.
--
-- До этого премии были справочником названий, выведенным из `entities.metadata.awards`
-- (Flat First): по нему нельзя собрать страницу «Оскар 2024» — нет категорий, нет номинаций
-- людей, нет обратного обхода. Награды — не описательное поле каталога, а связь «церемония ↔
-- работа ↔ человек ↔ категория», поэтому у неё свои таблицы.
--
-- Старый справочник (`awards` с `name` и `entities_count`) уходит вместе с `catalog::awards::sync`:
-- строки в нём выводились из метаданных и удалялись, как только счётчик падал до нуля, поэтому
-- курируемую премию в той же таблице держать нельзя. Строчки `metadata.awards` на карточке
-- остаются как есть — они наполняют блок «Производство и факты», пока контент не переехал.

DROP TABLE IF EXISTS awards;

-- Премия: «Оскар», «Хьюго», The Game Awards.
CREATE TABLE awards (
    id          uuid        PRIMARY KEY DEFAULT uuidv7(),
    slug        text        NOT NULL UNIQUE,
    title       text        NOT NULL,
    -- Полное название: «Премия Американской академии кинематографических искусств и наук».
    full_title  text,
    -- Типы работ премии: у «Золотого глобуса» и фильмы, и сериалы — отсюда вкладки раздела.
    kinds       text[]      NOT NULL DEFAULT '{}'
                            CHECK (kinds <@ ARRAY['movie', 'series', 'book', 'game']::text[]),
    country     text,
    founded     int         CHECK (founded BETWEEN 1800 AND 2100),
    description text,
    logo_url    text,
    site_url    text,
    -- Ручной порядок в списке: крупные премии выше.
    position    int         NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER awards_set_updated_at
    BEFORE UPDATE ON awards
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Список раздела: ручной порядок, затем алфавит.
CREATE INDEX awards_position_idx ON awards (position DESC, title);
-- Вкладки по типу работ (`kinds @> '{movie}'`).
CREATE INDEX awards_kinds_idx ON awards USING gin (kinds);

-- Церемония: премия и год. `season_year` — год работ, если он другой: «Оскар 2024 за 2023-й».
CREATE TABLE award_editions (
    id          uuid        PRIMARY KEY DEFAULT uuidv7(),
    award_id    uuid        NOT NULL REFERENCES awards (id) ON DELETE CASCADE,
    year        int         NOT NULL CHECK (year BETWEEN 1880 AND 2100),
    season_year int         CHECK (season_year BETWEEN 1880 AND 2100),
    -- «96-я церемония».
    number      int         CHECK (number > 0),
    held_on     date,
    place       text,
    cover_url   text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (award_id, year)
);

CREATE TRIGGER award_editions_set_updated_at
    BEFORE UPDATE ON award_editions
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Годы на странице премии: новые сверху.
CREATE INDEX award_editions_year_idx ON award_editions (award_id, year DESC);

-- Категория принадлежит премии, а не церемонии: они появляются и исчезают по годам, а что было
-- в конкретном году — видно по номинациям. Переименование в отдельном году — `title_override`
-- номинации.
CREATE TABLE award_categories (
    id         uuid        PRIMARY KEY DEFAULT uuidv7(),
    award_id   uuid        NOT NULL REFERENCES awards (id) ON DELETE CASCADE,
    slug       text        NOT NULL,
    title      text        NOT NULL,
    position   int         NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (award_id, slug)
);

CREATE TRIGGER award_categories_set_updated_at
    BEFORE UPDATE ON award_categories
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Номинация: работа и человек сразу (актёрская категория — это и фильм, и актёр), либо просто
-- имя, если ни того, ни другого в каталоге нет. Победителей в категории может быть несколько
-- (ничья, совместная награда), поэтому `won` — флаг номинации, а не `winner_id` у категории.
CREATE TABLE award_nominations (
    id             uuid        PRIMARY KEY DEFAULT uuidv7(),
    edition_id     uuid        NOT NULL REFERENCES award_editions (id) ON DELETE CASCADE,
    -- RESTRICT: категорию с номинациями не удалить молча — сначала разобрать номинации.
    category_id    uuid        NOT NULL REFERENCES award_categories (id) ON DELETE RESTRICT,
    entity_id      uuid        REFERENCES entities (id) ON DELETE CASCADE,
    person_id      uuid        REFERENCES people (id) ON DELETE CASCADE,
    -- Кого/что номинировали, если в каталоге этого нет: «Pixar», песня, иностранный фильм.
    name           text,
    -- «режиссёр», «за роль Оппенгеймера».
    role           text,
    -- Название категории в этот год, если оно отличалось от нынешнего.
    title_override text,
    won            boolean     NOT NULL DEFAULT false,
    position       int         NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CHECK (entity_id IS NOT NULL OR person_id IS NOT NULL OR name IS NOT NULL)
);

CREATE TRIGGER award_nominations_set_updated_at
    BEFORE UPDATE ON award_nominations
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Страница церемонии: категории по порядку, внутри — номинации.
CREATE INDEX award_nominations_edition_idx
    ON award_nominations (edition_id, category_id, position);
-- Награды работы и человека на их карточках (и фасеты Meilisearch при переиндексации).
CREATE INDEX award_nominations_entity_idx ON award_nominations (entity_id)
    WHERE entity_id IS NOT NULL;
CREATE INDEX award_nominations_person_idx ON award_nominations (person_id)
    WHERE person_id IS NOT NULL;
-- Категория с номинациями (проверка перед удалением) — по FK с RESTRICT.
CREATE INDEX award_nominations_category_idx ON award_nominations (category_id);
