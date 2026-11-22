-- Промпты ИИ, настраиваемые из админки. Встроенные тексты — в коде (`catalog::tutor::templates`);
-- здесь — только переопределения: для всех сущностей (scope = 'default') или для одного типа
-- (scope = 'book', 'movie', …). Порядок поиска: тип → default → встроенный.
CREATE TABLE ai_prompts (
    key        text        NOT NULL CHECK (key ~ '^[a-z_.]{1,60}$'),
    scope      text        NOT NULL CHECK (scope ~ '^[a-z_]{1,20}$'),
    template   text        NOT NULL CHECK (length(template) BETWEEN 1 AND 6000),
    updated_by uuid        REFERENCES users (id) ON DELETE SET NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (key, scope)
);
