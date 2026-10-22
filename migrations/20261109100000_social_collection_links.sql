-- social: ручные связи между подборками — какие коллекции админ показывает рядом с данной и в
-- каком порядке («Похожие подборки»). Связь направленная; до 12 у коллекции (проверка в коде);
-- связывать можно только публичные (проверка в коде), приватная связанная пропускается в выдаче.
-- Описание — .docs/backend/social.md.

CREATE TABLE collection_links (
    collection_id uuid        NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    related_id    uuid        NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    position      integer     NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (collection_id, related_id),
    CHECK (collection_id <> related_id)
);

-- Связи коллекции по порядку.
CREATE INDEX collection_links_order_idx ON collection_links (collection_id, position);
-- Каскад и «на кого ссылаются».
CREATE INDEX collection_links_related_idx ON collection_links (related_id);
