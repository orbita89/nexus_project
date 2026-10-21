-- Баннеры v2: вид (image | html | slides), HTML-текст, слайды, кнопки, тема, цели показа
-- (все страницы / страница / шаблон пути) с исключениями и приоритет. Старые баннеры остаются
-- kind = 'image', их page переезжает в targets = [{"type":"page","page":...}].
ALTER TABLE banners
    ADD COLUMN kind          text    NOT NULL DEFAULT 'image'
        CHECK (kind IN ('image', 'html', 'slides')),
    ADD COLUMN content_html  text,
    ADD COLUMN slides        jsonb   NOT NULL DEFAULT '[]'
        CHECK (jsonb_typeof(slides) = 'array' AND jsonb_array_length(slides) <= 10),
    ADD COLUMN buttons       jsonb   NOT NULL DEFAULT '[]'
        CHECK (jsonb_typeof(buttons) = 'array' AND jsonb_array_length(buttons) <= 3),
    ADD COLUMN theme         text,
    ADD COLUMN targets       jsonb   NOT NULL DEFAULT '[{"type": "all"}]'
        CHECK (jsonb_typeof(targets) = 'array' AND jsonb_array_length(targets) BETWEEN 1 AND 20),
    ADD COLUMN exclude_paths text[]  NOT NULL DEFAULT '{}'
        CHECK (cardinality(exclude_paths) <= 20),
    ADD COLUMN priority      int     NOT NULL DEFAULT 0,
    ALTER COLUMN image_url DROP NOT NULL;

UPDATE banners SET targets = jsonb_build_array(jsonb_build_object('type', 'page', 'page', page));

-- page теперь в targets; в ответе API он вычисляется (первый targets[].page).
DROP INDEX banners_page_active_idx;
ALTER TABLE banners DROP COLUMN page;

ALTER TABLE banners
    ADD CONSTRAINT banners_kind_content CHECK (
        (kind <> 'image' OR image_url IS NOT NULL)
        AND (kind <> 'html' OR content_html IS NOT NULL)
        AND (kind <> 'slides' OR jsonb_array_length(slides) >= 1)
    );

-- Публичный список: активные по priority desc, position.
CREATE INDEX banners_active_idx ON banners (priority DESC, position) WHERE is_active;
