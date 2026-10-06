-- Чем заменён refresh-токен при обновлении. Нужно, чтобы отличать кражу от обычного выхода:
--   * токен отозван ротацией (replaced_by задан) и пришёл снова — его украли, отзываем все сессии;
--   * токен отозван выходом или завершением сессии (replaced_by пуст) — просто 401.
ALTER TABLE refresh_tokens
    ADD COLUMN replaced_by uuid REFERENCES refresh_tokens (id) ON DELETE SET NULL;
