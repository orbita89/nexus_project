-- Реакции зрителей на произведение: 🔥 😢 🤔 😴. Одна реакция на зрителя, повторная заменяет.
CREATE TABLE entity_reactions (
    entity_id  uuid        NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       text        NOT NULL CHECK (kind IN ('fire', 'sad', 'think', 'bored')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (entity_id, user_id)
);

CREATE INDEX entity_reactions_user_idx ON entity_reactions (user_id);
