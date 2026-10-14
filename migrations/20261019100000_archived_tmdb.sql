-- archived_tmdb: фильмы TMDB, перенесённые из PostgreSQL в холодный архив (индекс Tantivy,
-- catalog::archive). Сами строки `entities` удалены — здесь только id TMDB (~1 млн × 4 байта):
-- импорт (`scripts/catalog-import/merge.py`) их пропускает, иначе удалённые фильмы вернулись бы
-- при следующем слиянии. Строку удаляют промоушен из архива и сборка горячего слоя.

CREATE TABLE archived_tmdb (
    tmdb_id     integer PRIMARY KEY,
    archived_at timestamptz NOT NULL DEFAULT now()
);
