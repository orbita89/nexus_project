-- social: оформление особой коллекции — обои (широкий фон шапки страницы), обложка карточки и
-- тема (id пресета фронтенда, строка без enum). Только у особой: у обычной все три — NULL
-- (снятие из особых их сбрасывает). Описание — .docs/backend/social.md.

ALTER TABLE collections
    ADD COLUMN wallpaper_url text,
    ADD COLUMN cover_url text,
    ADD COLUMN theme text,
    ADD CONSTRAINT collections_style_featured_check
        CHECK (featured_position IS NOT NULL
               OR (wallpaper_url IS NULL AND cover_url IS NULL AND theme IS NULL));
