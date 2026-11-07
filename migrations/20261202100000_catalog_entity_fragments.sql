-- Фрагменты полотна (задача фронтенда «Страница картины»): лента «Интересные детали полотна».
--
-- Фрагмент — прямоугольник на исходнике, а не отдельно залитый файл: с координатами фронтенд из
-- той же ленты перелетает зумом к этому месту на самом холсте («Скрытый автопортрет» → лицо
-- Рембрандта в толпе). Отдельными JPEG это не сохранить — их можно только показать.
--
-- Координаты — доли от ширины и высоты исходника (0–1), а не пиксели: исходник перезальют в
-- другом разрешении, и доли это переживут. Сам исходник — `metadata.image_url` / `iiif_base`.
--
-- Фрагмент не сущность: ни оценок, ни рецензий, ни своей страницы у «золотой девочки» нет.
CREATE TABLE entity_fragments (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    entity_id   uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    title       text NOT NULL,
    description text,
    -- Левый верхний угол и размеры прямоугольника в долях исходника. Прямоугольник целиком
    -- внутри картины: вылезший за край кроп IIIF-сервер отдаст обрезанным, а фронтенд об этом
    -- не узнает.
    crop_x      double precision NOT NULL CHECK (crop_x BETWEEN 0 AND 1),
    crop_y      double precision NOT NULL CHECK (crop_y BETWEEN 0 AND 1),
    crop_width  double precision NOT NULL CHECK (crop_width > 0 AND crop_width <= 1),
    crop_height double precision NOT NULL CHECK (crop_height > 0 AND crop_height <= 1),
    CONSTRAINT entity_fragments_crop_inside CHECK (
        crop_x + crop_width <= 1.0000001 AND crop_y + crop_height <= 1.0000001),
    -- Порядок рассказа, как `position` у титров: детали идут по смыслу, а не слева направо.
    position    int         NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Лента картины: выбор и порядок одним индексом.
CREATE INDEX entity_fragments_entity_idx ON entity_fragments (entity_id, position, id);

CREATE TRIGGER entity_fragments_set_updated_at
    BEFORE UPDATE ON entity_fragments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
