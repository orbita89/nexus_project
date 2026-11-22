-- Effort `none` у провайдера ИИ (`catalog::ai`): размышления выключены — самый дешёвый режим
-- для механической разметки. Anthropic — без `thinking` и `effort`, OpenAI-совместимые —
-- `thinking: {type: disabled}` (DeepSeek) без `reasoning_effort`.
ALTER TABLE catalog_ai_providers DROP CONSTRAINT IF EXISTS catalog_ai_providers_effort_check;
ALTER TABLE catalog_ai_providers
    ADD CONSTRAINT catalog_ai_providers_effort_check CHECK (effort IN ('none', 'low', 'medium', 'high'));
