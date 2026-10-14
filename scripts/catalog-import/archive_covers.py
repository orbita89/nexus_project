#!/usr/bin/env python3
"""Постеры для холодного архива: дописывает `cover_url` в дамп `nexus archive dump`.

Архив Tantivy до 2026-10-14 хранил фильмы без постера, а полные данные TMDB (`tmdb_full.py`)
есть только у горячего слоя. Скрипт берёт дамп и для строк с TMDB id без `cover_url` запрашивает
`GET /3/movie/{id}?language=ru-RU` (постер русской версии, нет — основной), затем пишет новый
дамп с колонкой `cover_url` (`https://image.tmdb.org/t/p/w780/...`, как у карточек). Строки
горячего слоя тоже проходят (в PostgreSQL сборка их постер не меняет).

Ответы копятся в кэше `--cache` (TSV `tmdb_id`, `cover_url`; пусто — у TMDB нет постера или фильм
удалён): перезапуск продолжает, повторный прогон по свежему дампу запросов почти не делает.
Ключ — `TMDB_API_KEY` (окружение или `infra/.env`).

Порядок (миллион фильмов при 40 запросах/с — ~7,5 ч):
    nexus archive dump /tmp/archive.tsv
    python3 archive_covers.py /tmp/archive.tsv /tmp/archive-covers.tsv
    nexus archive dump /tmp/archive.tsv                  # свежий дамп перед сборкой
    python3 archive_covers.py /tmp/archive.tsv /tmp/archive-covers.tsv   # из кэша, секунды
    nexus archive build /tmp/archive-covers.tsv --fresh  # архив в новой схеме, с постерами
"""

import argparse
import asyncio
import logging
import os
import signal
import sys
import time

import aiohttp

from fetch_tmdb import env
from import_tmdb import POSTER, Fatal, Tmdb

log = logging.getLogger("archive_covers")


def columns(header: list[str]) -> tuple[int, int | None]:
    """Номера колонок `tmdb_id` и `cover_url` (её может не быть — допишется)."""
    if "tmdb_id" not in header:
        sys.exit("dump has no tmdb_id column")
    return header.index("tmdb_id"), header.index("cover_url") if "cover_url" in header else None


def read_cache(path: str) -> dict[int, str]:
    cache = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            for line in f:
                tmdb_id, _, cover = line.rstrip("\n").partition("\t")
                if tmdb_id.isdigit():
                    cache[int(tmdb_id)] = cover
    return cache


def todo(dump: str, cache: dict[int, str]) -> list[int]:
    """TMDB id строк без постера, которых нет в кэше; популярные первыми (прогон можно
    прервать — заметные фильмы уже с постерами)."""
    ids = []
    with open(dump, encoding="utf-8") as f:
        header = f.readline().rstrip("\n").split("\t")
        id_col, cover_col = columns(header)
        pop_col = header.index("popularity_score") if "popularity_score" in header else None
        for line in f:
            fields = line.rstrip("\n").split("\t")
            tmdb_id = fields[id_col] if id_col < len(fields) else ""
            has_cover = cover_col is not None and cover_col < len(fields) and fields[cover_col]
            if tmdb_id.isdigit() and not has_cover and int(tmdb_id) not in cache:
                try:
                    popularity = float(fields[pop_col]) if pop_col is not None else 0.0
                except (ValueError, IndexError):
                    popularity = 0.0
                ids.append((popularity, int(tmdb_id)))
    ids.sort(reverse=True)
    return [tmdb_id for _, tmdb_id in ids]


async def fetch(ids: list[int], cache: dict[int, str], cache_path: str, args) -> bool:
    """Скачивает постеры в кэш. False — остановлено сигналом."""
    key = env("TMDB_API_KEY")
    if not key:
        sys.exit("TMDB_API_KEY is required")
    queue: asyncio.Queue = asyncio.Queue()
    for tmdb_id in ids:
        queue.put_nowait(tmdb_id)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    loop.add_signal_handler(signal.SIGINT, stop.set)
    loop.add_signal_handler(signal.SIGTERM, stop.set)
    total, done, found, started = len(ids), 0, 0, time.monotonic()
    last_log = started

    with open(cache_path, "a", encoding="utf-8") as out:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=60)) as session:
            tmdb = Tmdb(session, key, args.rate, retries=6)

            async def worker():
                nonlocal done, found, last_log
                while not stop.is_set():
                    try:
                        tmdb_id = queue.get_nowait()
                    except asyncio.QueueEmpty:
                        return
                    data = await tmdb.get(f"/movie/{tmdb_id}", language="ru-RU")
                    poster = (data or {}).get("poster_path")
                    cover = f"{POSTER}{poster}" if poster else ""
                    cache[tmdb_id] = cover
                    out.write(f"{tmdb_id}\t{cover}\n")
                    done += 1
                    found += bool(cover)
                    if time.monotonic() - last_log > 30:
                        out.flush()
                        last_log = time.monotonic()
                        rate = done / max(1.0, last_log - started)
                        log.info("%d/%d (%.1f/s, ~%.0f min left), with cover %d",
                                 done, total, rate, (total - done) / rate / 60, found)

            workers = [asyncio.create_task(worker()) for _ in range(args.workers)]
            await asyncio.gather(*workers)
    log.info("fetched %d/%d, with cover %d%s", done, total, found,
             " (interrupted)" if stop.is_set() else "")
    return not stop.is_set()


def write(dump: str, output: str, cache: dict[int, str]) -> tuple[int, int]:
    """Новый дамп: `cover_url` из кэша там, где его не было. Возвращает (строк, с постером)."""
    rows = covered = 0
    with open(dump, encoding="utf-8") as f, open(output, "w", encoding="utf-8") as out:
        header = f.readline().rstrip("\n").split("\t")
        id_col, cover_col = columns(header)
        if cover_col is None:
            cover_col = len(header)
            header.append("cover_url")
        out.write("\t".join(header) + "\n")
        for line in f:
            fields = line.rstrip("\n").split("\t")
            fields += [""] * (len(header) - len(fields))
            if not fields[cover_col] and fields[id_col].isdigit():
                fields[cover_col] = cache.get(int(fields[id_col]), "")
            rows += 1
            covered += bool(fields[cover_col])
            out.write("\t".join(fields) + "\n")
    return rows, covered


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("dump", help="дамп `nexus archive dump`")
    p.add_argument("output", help="дамп с постерами для `nexus archive build`")
    p.add_argument("--cache", help="кэш ответов (по умолчанию OUTPUT.cache)")
    p.add_argument("--limit", type=int, default=0, help="не больше N запросов (0 — все)")
    p.add_argument("--rate", type=float, default=40, help="запросов в секунду (TMDB пускает ~50)")
    p.add_argument("--workers", type=int, default=32)
    args = p.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    cache_path = args.cache or f"{args.output}.cache"

    cache = read_cache(cache_path)
    ids = todo(args.dump, cache)
    if args.limit:
        ids = ids[:args.limit]
    log.info("cached %d, to fetch %d", len(cache), len(ids))
    if ids:
        try:
            complete = asyncio.run(fetch(ids, cache, cache_path, args))
        except Fatal as e:
            sys.exit(str(e))
        if not complete:
            sys.exit("interrupted: run again to continue")
    rows, covered = write(args.dump, args.output, cache)
    log.info("%s: %d rows, %d with cover", args.output, rows, covered)


if __name__ == "__main__":
    main()
