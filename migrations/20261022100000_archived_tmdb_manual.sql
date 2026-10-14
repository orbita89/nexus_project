-- archived_tmdb.manual: фильм перенесён в архив вручную из админки (POST /admin/archive/demote).
-- Сборка горячего слоя (`nexus archive build`) такие фильмы на сайт не возвращает, даже если по
-- правилу (русское название, топ иностранных) они горячие. Флаг снимает только промоушен
-- (строка удаляется).

ALTER TABLE archived_tmdb ADD COLUMN manual boolean NOT NULL DEFAULT false;
