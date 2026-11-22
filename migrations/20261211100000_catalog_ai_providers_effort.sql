-- Усилие рассуждения модели у провайдера ИИ (`catalog::ai`): low — дёшево и быстро, по
-- умолчанию; medium / high — для сложных задач. Anthropic — `output_config.effort`,
-- OpenAI-совместимые — `reasoning_effort` (модель без рассуждений — параметр не шлётся).
ALTER TABLE catalog_ai_providers
    ADD COLUMN effort text NOT NULL DEFAULT 'low' CHECK (effort IN ('low', 'medium', 'high'));
