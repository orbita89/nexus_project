-- social: гибкое оформление страницы особой коллекции — где обои (шапка или вся страница),
-- затемнение и размытие обоев, эффекты темы, свой акцентный цвет. У обычной коллекции — значения
-- по умолчанию (снятие из особых сбрасывает). Описание — .docs/backend/social.md.

ALTER TABLE collections
    ADD COLUMN wallpaper_mode text NOT NULL DEFAULT 'header'
        CHECK (wallpaper_mode IN ('header', 'page')),
    -- NULL — затемнение выбирает тема на фронтенде.
    ADD COLUMN wallpaper_dim smallint CHECK (wallpaper_dim BETWEEN 0 AND 100),
    ADD COLUMN wallpaper_blur smallint NOT NULL DEFAULT 0 CHECK (wallpaper_blur BETWEEN 0 AND 20),
    ADD COLUMN effects boolean NOT NULL DEFAULT true,
    ADD COLUMN accent_color text CHECK (accent_color ~ '^#[0-9a-f]{6}$'),
    DROP CONSTRAINT collections_style_featured_check,
    ADD CONSTRAINT collections_style_featured_check
        CHECK (featured_position IS NOT NULL
               OR (wallpaper_url IS NULL AND cover_url IS NULL AND theme IS NULL
                   AND wallpaper_mode = 'header' AND wallpaper_dim IS NULL
                   AND wallpaper_blur = 0 AND effects AND accent_color IS NULL));
