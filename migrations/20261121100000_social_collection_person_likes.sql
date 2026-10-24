-- social: «что мне нравится» — лайки подборкам (с дизлайком) и людям каталога (только лайк).
--
-- Реакции на подборки (`collection_reactions`) остаются рядом и пока никуда не деваются:
-- фронтенд показывает их сейчас и переключится на лайки отдельным релизом. Переносить `fire`
-- в лайки, а `bored` в дизлайки не будем — «скучно» не равно «не нравится».
--
-- Счётчики подборки не колонками, а подзапросами в `COLLECTION_COLUMNS` — ровно как
-- `comments_count`: колонка с триггером трогала бы `collections.updated_at`, а по нему идут
-- сортировки и ревалидация статики фронтенда.

-- Лайк и дизлайк подборке: один голос на зрителя, как у рецензий (`review_votes`).
CREATE TABLE collection_likes (
    collection_id uuid        NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    user_id       uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    value         smallint    NOT NULL CHECK (value IN (1, -1)),
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (collection_id, user_id)
);

-- «Что я лайкнул», новые сверху — раздел «Что нравится» в аналитике профиля.
CREATE INDEX collection_likes_user_idx ON collection_likes (user_id, created_at DESC);

-- Лайк человеку каталога (режиссёру, актёру, автору). Дизлайка нет — как у профилей.
-- FK на `people` допустим: колонка-ссылка своя, а данные человека social читает справочником.
CREATE TABLE person_likes (
    person_id  uuid        NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (person_id, user_id)
);

-- Счётчик на странице человека и «кто лайкнул», новые сверху.
CREATE INDEX person_likes_person_idx ON person_likes (person_id, created_at DESC);
-- «Что я лайкнул».
CREATE INDEX person_likes_user_idx ON person_likes (user_id, created_at DESC);
