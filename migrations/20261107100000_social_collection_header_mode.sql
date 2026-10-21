-- social: режим шапки особой коллекции — `styled` (в теме / с обоями), `plain` (нейтральная),
-- `none` (без шапки, заголовок над списком). У обычной — `styled` (снятие из особых сбрасывает).
-- Описание — .docs/backend/social.md.

ALTER TABLE collections
    ADD COLUMN header_mode text NOT NULL DEFAULT 'styled'
        CHECK (header_mode IN ('styled', 'plain', 'none')),
    DROP CONSTRAINT collections_style_featured_check,
    ADD CONSTRAINT collections_style_featured_check
        CHECK (featured_position IS NOT NULL
               OR (wallpaper_url IS NULL AND cover_url IS NULL AND theme IS NULL
                   AND wallpaper_mode = 'header' AND wallpaper_dim IS NULL
                   AND wallpaper_blur = 0 AND effects AND accent_color IS NULL
                   AND header_mode = 'styled'));
