-- Сводки оценок в каталоге для сортировок списка (popular, rating). Источник — рецензии social:
-- он сообщает сводки через shared::directory::EntityRatings после каждой записи и при старте.
CREATE TABLE entity_ratings (
    entity_id  uuid             PRIMARY KEY REFERENCES entities (id) ON DELETE CASCADE,
    average    double precision CHECK (average BETWEEN 1 AND 10),
    count      bigint           NOT NULL DEFAULT 0 CHECK (count >= 0),
    updated_at timestamptz      NOT NULL DEFAULT now()
);

-- «Популярное» и «по оценке» без сортировки всей таблицы.
CREATE INDEX entity_ratings_count_idx   ON entity_ratings (count DESC);
CREATE INDEX entity_ratings_average_idx ON entity_ratings (average DESC NULLS LAST, count DESC);
