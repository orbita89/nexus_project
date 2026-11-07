-- Сезоны сериалов и их серии (задача фронтенда «Сезоны и серии сериалов»).
--
-- Сезон — не вложенный объект сериала, а строка `entities` с собственным slug
-- (`arcane-season-2`) и новым `kind = 'season'`: оценка, рецензия, лайк, отметка, коллекция и
-- тема форума работают по `{slug}` в `social` без единой новой ручки. Вложенным объектом под
-- него пришлось бы дублировать весь социальный слой.
--
-- ALTER TYPE ... ADD VALUE работает внутри транзакции (PostgreSQL 12+), пока новое значение не
-- используется в ней же: ни `parent_id`, ни `episodes` его не касаются. Поэтому условие
-- `kind <> 'season'` (сезонов нет в каталоге и в поиске) живёт в SQL модуля, а не здесь.
ALTER TYPE entity_kind ADD VALUE IF NOT EXISTS 'season';

-- Родитель сущности: у сезона — его сериал. Своя таблица связи не нужна — связь 1:N внутри
-- `entities`, и каскад снимает сезоны вместе с сериалом.
ALTER TABLE entities ADD COLUMN parent_id uuid REFERENCES entities (id) ON DELETE CASCADE;

-- Сезоны сериала: один запрос на шапку страницы.
CREATE INDEX entities_parent_idx ON entities (parent_id) WHERE parent_id IS NOT NULL;

-- Серия — не сущность: своего slug'а, рецензий и подборок у неё нет (решение в задаче).
-- Видео у нас нет: ни «просмотрено», ни прогресса, ни позиции в серии здесь не будет.
CREATE TABLE episodes (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    -- Сезон (`entities.kind = 'season'`); удаление сезона снимает серии.
    season_id       uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    number          int  NOT NULL CHECK (number >= 0),
    title           text,
    original_title  text,
    overview        text,
    air_date        date,
    -- Целые минуты: источник, отдающий секунды, округляет при импорте.
    runtime_minutes int CHECK (runtime_minutes > 0),
    -- Кадр из серии, 16:9 (`still_path` у TMDB), не постер сериала и не постер сезона.
    still_url       text,
    -- Размеры этой самой копии кадра: карточка держит точное `aspect-ratio` до загрузки.
    still_width     int CHECK (still_width > 0),
    still_height    int CHECK (still_height > 0),
    external_ids    jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    -- Номер серии в сезоне уникален: повторный импорт обновляет строку, а не множит её.
    UNIQUE (season_id, number)
);

CREATE TRIGGER episodes_set_updated_at
    BEFORE UPDATE ON episodes
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
