-- entity_active: флаг «активен» у сущностей и счётчики для публичных списков.
--
-- Каталог из импорта TMDB + Wikidata — ~1,2 млн фильмов, на сайте нужна отобранная часть.
-- Неактивная сущность хранится в БД, но не видна: нет в списках, поиске, похожих, связанных и
-- фильмографиях, карточка — 404 для всех, кроме админа. Включается в админке (`active` в PATCH)
-- или скриптом импорта.
--
-- Счётчики держат триггеры: проверка «есть ли у человека активная работа» запросом по 7 млн
-- строк титров — секунды на каждый список людей, подсчёт тегов по 2,4 млн связей — тоже.
-- Счётчики людей — отдельная узкая таблица, а не колонки `people`: заполнить её — один
-- INSERT … GROUP BY, а не перезапись 2 млн строк людей вместе с триграммным индексом имён.

ALTER TABLE entities ADD COLUMN active boolean NOT NULL DEFAULT true;

-- Горячий список: новинки среди активных (сортировка `new` по умолчанию).
CREATE INDEX entities_active_release_idx ON entities (release_date DESC NULLS LAST, title, id)
    WHERE active;

-- Работы человека: всего и в активных сущностях. Строки нет — работ нет. Публичный человек —
-- с активной работой или вовсе без работ (заведён в админке, титры ещё не добавлены).
CREATE TABLE person_credit_counts (
    person_id      uuid    PRIMARY KEY REFERENCES people (id) ON DELETE CASCADE,
    credits        integer NOT NULL DEFAULT 0,
    active_credits integer NOT NULL DEFAULT 0
);

INSERT INTO person_credit_counts (person_id, credits, active_credits)
SELECT c.person_id, count(*), count(*) FILTER (WHERE e.active)
FROM entity_credits c JOIN entities e ON e.id = c.entity_id
GROUP BY c.person_id;

-- Список людей по алфавиту: идёт по имени и проверяет счётчики точечно.
CREATE INDEX people_full_name_idx ON people (full_name, id);

-- Теги: число активных сущностей (`entities_count` в API). Тегов десятки — колонка.
ALTER TABLE tags ADD COLUMN entities_count integer NOT NULL DEFAULT 0;

UPDATE tags t SET entities_count = s.n
FROM (
    SELECT et.tag_id, count(*) AS n
    FROM entity_tags et JOIN entities e ON e.id = et.entity_id
    WHERE e.active
    GROUP BY et.tag_id
) s
WHERE t.id = s.tag_id;

-- Титры: ±1 работа человеку, активная — если сущность активна. При каскадном удалении
-- сущности её строка уже не видна (активные работы вычел триггер на entities).
CREATE FUNCTION entity_credits_count() RETURNS trigger AS $$
BEGIN
    IF TG_OP IN ('DELETE', 'UPDATE') THEN
        UPDATE person_credit_counts
        SET credits = credits - 1,
            active_credits = active_credits - (
                SELECT count(*) FROM entities e WHERE e.id = OLD.entity_id AND e.active)
        WHERE person_id = OLD.person_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        INSERT INTO person_credit_counts AS pc (person_id, credits, active_credits)
        SELECT NEW.person_id, 1,
               (SELECT count(*) FROM entities e WHERE e.id = NEW.entity_id AND e.active)
        ON CONFLICT (person_id) DO UPDATE
        SET credits = pc.credits + 1,
            active_credits = pc.active_credits + EXCLUDED.active_credits;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER entity_credits_count
    AFTER INSERT OR DELETE OR UPDATE OF entity_id, person_id ON entity_credits
    FOR EACH ROW EXECUTE FUNCTION entity_credits_count();

-- Теги сущности: ±1 тегу, если сущность активна.
CREATE FUNCTION entity_tags_count() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        UPDATE tags SET entities_count = entities_count - 1
        WHERE id = OLD.tag_id
          AND EXISTS (SELECT 1 FROM entities e WHERE e.id = OLD.entity_id AND e.active);
    ELSE
        UPDATE tags SET entities_count = entities_count + 1
        WHERE id = NEW.tag_id
          AND EXISTS (SELECT 1 FROM entities e WHERE e.id = NEW.entity_id AND e.active);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER entity_tags_count
    AFTER INSERT OR DELETE ON entity_tags
    FOR EACH ROW EXECUTE FUNCTION entity_tags_count();

-- Сущность включили/выключили или удаляют активную: сдвинуть счётчики её людей и тегов.
CREATE FUNCTION entities_active_counts() RETURNS trigger AS $$
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
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER entities_active_changed
    AFTER UPDATE OF active ON entities
    FOR EACH ROW WHEN (OLD.active IS DISTINCT FROM NEW.active)
    EXECUTE FUNCTION entities_active_counts();

CREATE TRIGGER entities_active_deleted
    BEFORE DELETE ON entities
    FOR EACH ROW WHEN (OLD.active)
    EXECUTE FUNCTION entities_active_counts();
