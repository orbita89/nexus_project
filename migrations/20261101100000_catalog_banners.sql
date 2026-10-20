-- Баннеры (промо/новости) по страницам сайта, настраиваются в админке каталога.
CREATE TABLE banners (
    id         uuid PRIMARY KEY DEFAULT uuidv7(),
    page       text NOT NULL CHECK (page IN ('home', 'films', 'series', 'books', 'games', 'people')),
    title      text NOT NULL,
    subtitle   text,
    image_url  text NOT NULL,
    link_url   text,
    position   int  NOT NULL DEFAULT 0,
    is_active  bool NOT NULL DEFAULT true,
    starts_at  timestamptz,
    ends_at    timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT banners_window CHECK (starts_at IS NULL OR ends_at IS NULL OR ends_at > starts_at)
);

-- Публичный список: активные баннеры страницы по порядку.
CREATE INDEX banners_page_active_idx ON banners (page, position) WHERE is_active;

CREATE TRIGGER banners_set_updated_at
    BEFORE UPDATE ON banners
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
