-- Теги по разделам: счётчик активных сущностей с разбивкой по типу работы и описание тега.
--
-- Справочник `/tags` показывает вкладки разделов (Все / Фильмы / Сериалы / Книги / Игры /
-- Картины): нужно знать, сколько в теге фильмов, а сколько картин, и отбирать теги раздела.
-- Считать это запросом по `entity_tags` (2,4 млн связей) на каждый показ справочника нельзя —
-- счётчик держат те же триггеры, что и общий `tags.entities_count`.
--
-- Узкая таблица, а не колонка-jsonb в `tags`: у тега обычно один-два типа, строки нет — нет и
-- работ этого типа; строка кладётся по месту первой сущности такого типа.

CREATE TABLE tag_kind_counts (
    tag_id         uuid    NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    kind           text    NOT NULL,
    entities_count integer NOT NULL DEFAULT 0,
    PRIMARY KEY (tag_id, kind)
);

-- Теги раздела: `GET /tags?entity_kind=painting` идёт по типу и проверяет счётчик.
CREATE INDEX tag_kind_counts_kind_idx ON tag_kind_counts (kind, tag_id)
    WHERE entities_count > 0;

INSERT INTO tag_kind_counts (tag_id, kind, entities_count)
SELECT et.tag_id, e.kind, count(*)
FROM entity_tags et JOIN entities e ON e.id = et.entity_id
WHERE e.active
GROUP BY et.tag_id, e.kind;

-- Описание тега или жанра, 1–2 предложения. Пишется руками через админку, `NULL` — обычное
-- состояние.
ALTER TABLE tags ADD COLUMN description text;

-- Теги сущности: ±1 тегу и его счётчику по типу, если сущность активна. Тип берётся из той же
-- строки `entities`: при каскадном удалении сущности её строки уже нет, и триггер ничего не
-- вычитает (это сделал триггер на `entities`).
CREATE OR REPLACE FUNCTION entity_tags_count() RETURNS trigger AS $$
DECLARE
    entity_kind text;
BEGIN
    IF TG_OP = 'DELETE' THEN
        SELECT e.kind INTO entity_kind FROM entities e WHERE e.id = OLD.entity_id AND e.active;
        IF entity_kind IS NOT NULL THEN
            UPDATE tags SET entities_count = entities_count - 1 WHERE id = OLD.tag_id;
            UPDATE tag_kind_counts SET entities_count = entities_count - 1
            WHERE tag_id = OLD.tag_id AND kind = entity_kind;
        END IF;
    ELSE
        SELECT e.kind INTO entity_kind FROM entities e WHERE e.id = NEW.entity_id AND e.active;
        IF entity_kind IS NOT NULL THEN
            UPDATE tags SET entities_count = entities_count + 1 WHERE id = NEW.tag_id;
            INSERT INTO tag_kind_counts AS tk (tag_id, kind, entities_count)
            VALUES (NEW.tag_id, entity_kind, 1)
            ON CONFLICT (tag_id, kind) DO UPDATE
            SET entities_count = tk.entities_count + 1;
        END IF;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Массовое переключение `active`: один пересчёт на весь UPDATE, теперь и по типу работы
-- (`kind` сущности не меняется — берём из той же строки).
CREATE OR REPLACE FUNCTION entities_active_changed() RETURNS trigger AS $$
BEGIN
    WITH d AS (
        SELECT n.id, n.kind, CASE WHEN n.active THEN 1 ELSE -1 END AS delta
        FROM new_rows n JOIN old_rows o ON o.id = n.id
        WHERE o.active IS DISTINCT FROM n.active
    ),
    moved AS (
        UPDATE person_credit_counts pc SET active_credits = pc.active_credits + s.delta
        FROM (SELECT c.person_id, sum(d.delta) AS delta
              FROM d JOIN entity_credits c ON c.entity_id = d.id
              GROUP BY c.person_id) s
        WHERE pc.person_id = s.person_id
        RETURNING 1
    ),
    counted AS (
        UPDATE tags t SET entities_count = t.entities_count + s.delta
        FROM (SELECT et.tag_id, sum(d.delta) AS delta
              FROM d JOIN entity_tags et ON et.entity_id = d.id
              GROUP BY et.tag_id) s
        WHERE t.id = s.tag_id
        RETURNING 1
    )
    INSERT INTO tag_kind_counts AS tk (tag_id, kind, entities_count)
    SELECT et.tag_id, d.kind, sum(d.delta)
    FROM d JOIN entity_tags et ON et.entity_id = d.id
    GROUP BY et.tag_id, d.kind
    ON CONFLICT (tag_id, kind) DO UPDATE
    SET entities_count = tk.entities_count + EXCLUDED.entities_count;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Удаление активной сущности (построчно): сдвинуть счётчики её людей и тегов (общий и по типу
-- работы — тип не меняется, поэтому хватает `OLD.kind`). Смену `active` с 2026-10-17 считает
-- операторный `entities_active_changed()` выше.
CREATE OR REPLACE FUNCTION entities_active_counts() RETURNS trigger AS $$
DECLARE
    delta integer := -1;
BEGIN
    -- NEW при удалении не заполнен: смотреть его только при UPDATE.
    IF TG_OP = 'UPDATE' THEN
        IF NEW.active THEN
            delta := 1;
        END IF;
    END IF;
    UPDATE person_credit_counts pc SET active_credits = pc.active_credits + delta * c.n
    FROM (SELECT person_id, count(*) AS n FROM entity_credits
          WHERE entity_id = OLD.id GROUP BY person_id) c
    WHERE pc.person_id = c.person_id;
    UPDATE tags t SET entities_count = t.entities_count + delta
    FROM entity_tags et
    WHERE et.entity_id = OLD.id AND t.id = et.tag_id;
    INSERT INTO tag_kind_counts AS tk (tag_id, kind, entities_count)
    -- Строки нет — активных работ этого типа не было, вычитать нечего.
    SELECT et.tag_id, OLD.kind, GREATEST(delta, 0) FROM entity_tags et WHERE et.entity_id = OLD.id
    ON CONFLICT (tag_id, kind) DO UPDATE
    SET entities_count = tk.entities_count + delta;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
