-- Реакции на произведение становятся любым эмодзи, и их у зрителя может быть несколько
-- (как в Slack): закрытый список из четырёх видов не покрывает ни один фильм, а кнопка «+»
-- без свободного значения бессмысленна.
--
-- Таблица та же (`entity_reactions`), меняется только её форма: `kind` → `emoji`, ключ
-- становится тройным. Старые четыре вида превращаются в те же эмодзи, что показывал фронтенд,
-- поэтому ничего не теряется, а счётчики `reactions` в сводке оценок остаются прежними —
-- теперь они считаются по этим четырём значениям.
ALTER TABLE entity_reactions ADD COLUMN emoji text;

UPDATE entity_reactions SET emoji = CASE kind
    WHEN 'fire'  THEN '🔥'
    WHEN 'sad'   THEN '😢'
    WHEN 'think' THEN '🤔'
    WHEN 'bored' THEN '😴'
END;

ALTER TABLE entity_reactions
    DROP CONSTRAINT entity_reactions_pkey,
    DROP COLUMN kind,
    -- 64 байта — с запасом на ZWJ-последовательность с тонами («👩🏽‍🚀»); проверку самого
    -- эмодзи делает бэкенд (`social::validate::emoji`), в БД — только длина.
    ALTER COLUMN emoji SET NOT NULL,
    ADD CONSTRAINT entity_reactions_emoji_len CHECK (octet_length(emoji) BETWEEN 1 AND 64),
    ADD PRIMARY KEY (entity_id, user_id, emoji);

-- Сводка реакций произведения: count(*) ... GROUP BY emoji по одному диапазону ключа.
-- Отдельный индекс не нужен — первичный ключ начинается с `entity_id`.
