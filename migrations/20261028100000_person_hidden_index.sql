-- Скрытые люди (все работы в неактивных сущностях) — единицы из ~900 тыс. Без частичного индекса
-- `NOT EXISTS` из `PUBLIC_PERSON` (merge anti join) на каждой пачке перестройки читал весь
-- `person_credit_counts` с начала: ~5 с на пачку, ~40 мин на индекс людей. С индексом — мс.
CREATE INDEX person_credit_counts_hidden ON person_credit_counts (person_id)
    WHERE active_credits = 0 AND credits > 0;
