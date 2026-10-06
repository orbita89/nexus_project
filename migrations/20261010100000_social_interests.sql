-- social: интересы — на какие сущности подписан пользователь. Из них realtime собирает
-- автоподписку на каналы сущностей, позже — лента. Описание — documents/modules/social.md.

CREATE TABLE user_interests (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    entity_id  uuid NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, entity_id)
);

-- Свои интересы, новые сверху (PK находит интересы пользователя, но не отдаёт их по дате).
CREATE INDEX user_interests_user_id_idx ON user_interests (user_id, created_at DESC);
-- Кто интересуется сущностью: лента и рассылки.
CREATE INDEX user_interests_entity_id_idx ON user_interests (entity_id);
