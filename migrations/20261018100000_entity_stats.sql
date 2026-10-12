-- entity_stats: статистика TMDB и IMDb и балл «Рекомендуемые»; catalog_ranking: настройки формулы.
--
-- Отдельная узкая таблица, а не `metadata`: обновление `metadata` у ~1 млн строк перестраивает
-- GIN-индексы (25 мин на каждое «обновить popularity»), а пересчёт балла после правки формулы в
-- админке должен идти минуты. `metadata.popularity`/`vote_count` (миграция entity_active) больше не
-- используются: переносятся сюда, API их отбрасывает, `merge.py --cleanup-metadata` удаляет.

CREATE TABLE entity_stats (
    entity_id       uuid PRIMARY KEY REFERENCES entities (id) ON DELETE CASCADE,
    imdb_id         text,              -- tt0111161; из TMDB (merge.py)
    tmdb_popularity double precision,  -- «смотрят сейчас», ежедневный дамп TMDB
    tmdb_votes      integer,           -- число оценок на TMDB
    imdb_rating     double precision,  -- IMDb title.ratings: averageRating
    imdb_votes      integer,           -- IMDb: numVotes
    quality         double precision,  -- байесовский рейтинг IMDb (сортировка top_rated)
    rank_score      double precision,  -- балл «Рекомендуемые»; NULL — не вышел или мало голосов
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX entity_stats_imdb_id_idx ON entity_stats (imdb_id);
CREATE INDEX entity_stats_rank_idx ON entity_stats (rank_score DESC NULLS LAST, entity_id);

CREATE TRIGGER entity_stats_set_updated_at
    BEFORE UPDATE ON entity_stats
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

INSERT INTO entity_stats (entity_id, tmdb_popularity, tmdb_votes)
SELECT e.id, (e.metadata->>'popularity')::float8, (e.metadata->>'vote_count')::int
FROM entities e
WHERE e.metadata ? 'popularity' OR e.metadata ? 'vote_count';

-- Настройки балла и окна «Скоро премьера»: одна строка, правит админка.
--   score = weight_quality·q + weight_popularity·p + weight_recency·r, где
--   q — байесовский рейтинг IMDb (trust_votes «доверительных» голосов среднего), 5→0 … 9→1;
--   p — ln(1+popularity)/ln(1+p99); r — 0.5^(возраст/half_life_years).
-- Балл — у вышедших фильмов с imdb_votes ≥ min_votes.
CREATE TABLE catalog_ranking (
    id                boolean          PRIMARY KEY DEFAULT true CHECK (id),
    weight_quality    double precision NOT NULL DEFAULT 0.5,
    weight_popularity double precision NOT NULL DEFAULT 0.3,
    weight_recency    double precision NOT NULL DEFAULT 0.2,
    trust_votes       integer          NOT NULL DEFAULT 5000,
    half_life_years   double precision NOT NULL DEFAULT 4,
    min_votes         integer          NOT NULL DEFAULT 1000,
    soon_days         integer          NOT NULL DEFAULT 30,
    updated_at        timestamptz      NOT NULL DEFAULT now(),
    updated_by        uuid
);

INSERT INTO catalog_ranking DEFAULT VALUES;
