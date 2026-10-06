-- social: форум. Тема привязана к одной или нескольким сущностям, сообщения образуют ветки
-- (ответ ссылается на сообщение той же темы). Описание — documents/modules/social.md.

CREATE TABLE forum_threads (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    author_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    title        text NOT NULL,
    body         text NOT NULL,
    -- Закрыта для ответов (решает admin).
    is_locked    boolean NOT NULL DEFAULT false,
    -- Видимые сообщения (без заглушек удалённых) и время последнего: пишутся в одной транзакции
    -- с сообщением. Новая тема активна с момента создания.
    posts_count  integer NOT NULL DEFAULT 0,
    last_post_at timestamptz NOT NULL DEFAULT now(),
    -- Когда автор последний раз правил тему; updated_at меняют и счётчики.
    edited_at    timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT forum_threads_posts_count CHECK (posts_count >= 0)
);

CREATE TRIGGER forum_threads_set_updated_at
    BEFORE UPDATE ON forum_threads
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Свежие, активные, темы пользователя.
CREATE INDEX forum_threads_created_at_idx   ON forum_threads (created_at DESC);
CREATE INDEX forum_threads_last_post_at_idx ON forum_threads (last_post_at DESC);
CREATE INDEX forum_threads_author_id_idx    ON forum_threads (author_id, created_at DESC);

-- К каким сущностям привязана тема: «Дюна» книга и фильм — две строки.
CREATE TABLE forum_thread_entities (
    thread_id uuid NOT NULL REFERENCES forum_threads (id) ON DELETE CASCADE,
    entity_id uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    -- Порядок показа: первая — главная.
    position  smallint NOT NULL,
    PRIMARY KEY (thread_id, entity_id)
);

-- Все темы про сущность.
CREATE INDEX forum_thread_entities_entity_id_idx ON forum_thread_entities (entity_id);

CREATE TABLE forum_posts (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    thread_id  uuid NOT NULL REFERENCES forum_threads (id) ON DELETE CASCADE,
    author_id  uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Ответ на сообщение той же темы (составной ключ ниже).
    parent_id  uuid,
    -- NULL у заглушки удалённого сообщения, на которое есть ответы.
    body       text,
    deleted_at timestamptz,
    edited_at  timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT forum_posts_thread_id_id_key UNIQUE (thread_id, id),
    -- Сообщение без ответов удаляется целиком, с ответами — становится заглушкой. Ответы на
    -- сообщение удалённого пользователя (каскад по author_id) становятся сообщениями верхнего уровня.
    CONSTRAINT forum_posts_parent_fkey FOREIGN KEY (thread_id, parent_id)
        REFERENCES forum_posts (thread_id, id) ON DELETE SET NULL (parent_id),
    CONSTRAINT forum_posts_deleted_has_no_body CHECK ((deleted_at IS NULL) = (body IS NOT NULL))
);

CREATE TRIGGER forum_posts_set_updated_at
    BEFORE UPDATE ON forum_posts
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Сообщения темы по порядку (UNIQUE (thread_id, id) по времени не сортирует).
CREATE INDEX forum_posts_thread_id_idx ON forum_posts (thread_id, created_at);
-- Есть ли ответы на сообщение.
CREATE INDEX forum_posts_parent_id_idx ON forum_posts (parent_id) WHERE parent_id IS NOT NULL;
-- Счётчик сообщений в профиле.
CREATE INDEX forum_posts_author_id_idx ON forum_posts (author_id) WHERE deleted_at IS NULL;
