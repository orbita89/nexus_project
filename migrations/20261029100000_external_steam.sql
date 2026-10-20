-- Игры из Steam (scripts/games-import/steam_games.py): appid — entities.external_ids.steam.
-- Один appid — одна сущность; импорт находит уже записанные по тому же условию.
CREATE UNIQUE INDEX entities_external_steam_uq ON entities (((external_ids ->> 'steam')::int))
    WHERE external_ids ? 'steam';
