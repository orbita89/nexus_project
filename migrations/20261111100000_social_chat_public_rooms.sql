-- social: чат, этап 4 — системные комнаты, свои комнаты пользователей и жалобы на них.
-- Системная комната (`system`): участник — каждый пользователь, строк в chat_room_members нет;
-- её нельзя создать, покинуть, изменить или удалить. Своя комната пользователя — `owner_id`
-- (у комнат админа и системных он NULL; `created_by` остаётся «кто завёл строку»).
-- Описание — .docs/backend/social.md.

ALTER TABLE chat_rooms
    ADD COLUMN system   boolean NOT NULL DEFAULT false,
    ADD COLUMN owner_id uuid REFERENCES users (id) ON DELETE SET NULL,
    -- Системная — всегда своя комната (со slug) и всегда ничья.
    ADD CONSTRAINT chat_rooms_system_custom CHECK (NOT system OR (slug IS NOT NULL AND owner_id IS NULL));

-- Лимиты владельца: сколько у него живых комнат и сколько заведено за сутки.
CREATE INDEX chat_rooms_owner_idx ON chat_rooms (owner_id, created_at DESC) WHERE owner_id IS NOT NULL;
-- Системные комнаты списком: их подписывает каждое соединение.
CREATE INDEX chat_rooms_system_idx ON chat_rooms (id) WHERE system;

-- Жалобы на комнату: одна на пользователя, повторная перезаписывает причину.
CREATE TABLE chat_room_reports (
    room_id    uuid NOT NULL REFERENCES chat_rooms (id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    reason     text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (room_id, user_id)
);
-- Число жалоб на комнату для админского листинга.
CREATE INDEX chat_room_reports_room_idx ON chat_room_reports (room_id);

-- Системные комнаты — вкладки панели чата.
INSERT INTO chat_rooms (slug, title, description, system) VALUES
    ('general', 'Общий чат', 'Обо всём сразу', true),
    ('movies',  'Кино',      'Фильмы: премьеры, старое, любимое', true),
    ('series',  'Сериалы',   'Сезоны, финалы и что смотреть дальше', true),
    ('books',   'Книги',     'Чтение: новинки, классика, находки', true),
    ('games',   'Игры',      'Релизы, прохождения и впечатления', true);

-- Десять основных комнат — обычные (system = false, owner_id = NULL): из них можно выйти.
INSERT INTO chat_rooms (slug, title, description) VALUES
    ('releases',      'Новинки и премьеры',   'Что выходит на этой неделе и что из этого стоит внимания'),
    ('what-to-watch', 'Что посмотреть',       'Просите совет и советуйте сами'),
    ('what-to-read',  'Что почитать',         'Книжные советы и находки'),
    ('classics',      'Классика',             'Проверенное временем: кино и литература'),
    ('scifi-fantasy', 'Фантастика и фэнтези', 'От твёрдой НФ до большого фэнтези'),
    ('horror',        'Ужасы и триллеры',     'Страшное, напряжённое и всё, что мешает спать'),
    ('detectives',    'Детективы',            'Расследования, нуар и головоломки'),
    ('anime',         'Аниме и манга',        'Сериалы, фильмы, тайтлы и сезоны'),
    ('indie',         'Инди и артхаус',       'Фестивальное, авторское, малоизвестное'),
    ('offtop',        'Оффтоп',               'Всё остальное');
