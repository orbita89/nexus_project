-- Уникальность участия должна работать и для ролей без персонажа (режиссёр, автор):
-- обычный UNIQUE считает NULL'ы разными, и один режиссёр добавлялся к фильму дважды.
ALTER TABLE entity_credits
    DROP CONSTRAINT entity_credits_entity_id_person_id_role_character_name_key;

ALTER TABLE entity_credits
    ADD CONSTRAINT entity_credits_unique
    UNIQUE NULLS NOT DISTINCT (entity_id, person_id, role, character_name);
