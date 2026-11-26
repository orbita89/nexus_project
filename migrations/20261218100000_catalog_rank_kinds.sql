-- Балл «Рекомендуемые» и вес популярности в поиске — по видам (раньше только фильмы,
-- `catalog_ranking`). Балл считается одним SQL-проходом на вид в `entity_stats.rank_score`;
-- слой Б читает только (номер, балл), Tantivy не перестраивается.
--
-- score = weight_quality·q + weight_popularity·p + weight_recency·r + weight_site·s + weight_complete·c
--   q — байесовский рейтинг: IMDb (голосов ≥ min_votes), иначе оценки сайта; 5 → 0 … 9 → 1;
--   p — популярность: TMDB popularity (фильмы, сериалы) или работы автора (книги), ln к 99-му перцентилю;
--   r — свежесть 0.5^(возраст / half_life_years);
--   s — активность на сайте: число оценок, ln к 99-му перцентилю;
--   c — полнота карточки: обложка 0.6 + описание 0.4.
-- search_popularity — вес популярности в релевантности поиска (0 — только текст).
-- Строка `person`: только search_popularity (известность людей — `person_stats.fame`).
CREATE TABLE catalog_rank_kinds (
    kind              text             PRIMARY KEY
                      CHECK (kind IN ('movie', 'series', 'book', 'game', 'painting', 'person')),
    weight_quality    double precision NOT NULL DEFAULT 0.5,
    weight_popularity double precision NOT NULL DEFAULT 0.3,
    weight_recency    double precision NOT NULL DEFAULT 0.2,
    weight_site       double precision NOT NULL DEFAULT 0.1,
    weight_complete   double precision NOT NULL DEFAULT 0,
    search_popularity double precision NOT NULL DEFAULT 0.6,
    trust_votes       integer          NOT NULL DEFAULT 5000,
    half_life_years   double precision NOT NULL DEFAULT 4,
    min_votes         integer          NOT NULL DEFAULT 1000,
    updated_at        timestamptz      NOT NULL DEFAULT now(),
    updated_by        uuid
);

-- Фильмы — с прежними настройками.
INSERT INTO catalog_rank_kinds (kind, weight_quality, weight_popularity, weight_recency,
                                weight_site, weight_complete, trust_votes, half_life_years,
                                min_votes)
SELECT 'movie', weight_quality, weight_popularity, weight_recency, 0.1, 0, trust_votes,
       half_life_years, min_votes
FROM catalog_ranking;

INSERT INTO catalog_rank_kinds (kind, weight_quality, weight_popularity, weight_recency,
                                weight_site, weight_complete, trust_votes, half_life_years,
                                min_votes)
VALUES ('series',   0.3, 0.5, 0.2, 0.1, 0.1, 5000, 4,  1000),
       ('book',     0.2, 0.4, 0.1, 0.3, 0.3, 5,    20, 0),
       ('game',     0.3, 0.0, 0.2, 0.4, 0.3, 5,    4,  0),
       ('painting', 0.3, 0.0, 0.0, 0.4, 0.3, 5,    4,  0),
       ('person',   0,   0,   0,   0,   0,   0,    4,  0)
ON CONFLICT (kind) DO NOTHING;
