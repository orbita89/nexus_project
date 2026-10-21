-- Баннеры: вид custom — готовая вёрстка custom_html (мягкая санитизация в коде, ≤ 100 000).
ALTER TABLE banners
    ADD COLUMN custom_html text CHECK (char_length(custom_html) <= 100000);

ALTER TABLE banners DROP CONSTRAINT banners_kind_check;
ALTER TABLE banners
    ADD CONSTRAINT banners_kind_check CHECK (kind IN ('image', 'html', 'slides', 'custom'));

ALTER TABLE banners DROP CONSTRAINT banners_kind_content;
ALTER TABLE banners
    ADD CONSTRAINT banners_kind_content CHECK (
        (kind <> 'image' OR image_url IS NOT NULL)
        AND (kind <> 'html' OR content_html IS NOT NULL)
        AND (kind <> 'slides' OR jsonb_array_length(slides) >= 1)
        AND (kind <> 'custom' OR custom_html IS NOT NULL)
    );
