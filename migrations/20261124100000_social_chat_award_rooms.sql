-- social: комнаты чата премии и её церемонии — `award:oscar` и `award:oscar-2024`.
--
-- У премии и церемонии своя колонка и свой FK: снаружи это один вид комнаты (`award`), но
-- указывают они на разные таблицы каталога, и каскад должен уносить комнату вместе с премией
-- или церемонией. Данные за FK читаются только через `shared::directory::AwardDirectory` —
-- JOIN'ов в `awards` из `social` нет.

ALTER TABLE chat_rooms
    ADD COLUMN award_id         uuid REFERENCES awards (id) ON DELETE CASCADE,
    ADD COLUMN award_edition_id uuid REFERENCES award_editions (id) ON DELETE CASCADE;

-- Комната одна на премию и одна на церемонию. Ограничение, а не частичный индекс: по нему
-- работает `ON CONFLICT (award_id)` при создании комнаты (NULL'ы в UNIQUE не конфликтуют).
ALTER TABLE chat_rooms
    ADD CONSTRAINT chat_rooms_award_id_key UNIQUE (award_id),
    ADD CONSTRAINT chat_rooms_award_edition_id_key UNIQUE (award_edition_id);

-- Цель у комнаты по-прежнему ровно одна.
ALTER TABLE chat_rooms
    DROP CONSTRAINT chat_rooms_one_target,
    ADD CONSTRAINT chat_rooms_one_target
        CHECK (num_nonnulls(entity_id, person_id, tag_id, award_id, award_edition_id, slug,
                            dm_user_a) = 1);
