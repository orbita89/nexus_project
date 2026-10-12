#!/usr/bin/env python3
"""Скачивание фильмов TMDB в кэш — только загрузка, без записи в БД (её делает merge.py).

Список — ежедневный дамп id (`movie_ids_*.json.gz`), все фильмы, кроме `video`, по убыванию
popularity: если остановить раньше, скачаны самые известные. Плюс TMDB id фильмов из кэша Wikidata
(P4947), даже если их нет в дампе. Для каждого — `GET /3/movie/{id}?append_to_response=credits&
language=ru-RU`, ответ ужимается до нужных полей (`compact`) и дописывается строкой в
`movies.jsonl`; 404 — в `missing.txt`. Перезапуск продолжает с места остановки.

Ключ — `TMDB_API_KEY` из окружения, корневого `.env` или `infra/.env` (v3-ключ или v4-токен `eyJ…`).
Около 1,2 млн запросов: при 40/с — ~8 ч, кэш ~2–3 ГБ.

Запуск: python3 fetch_tmdb.py [--limit N] [--rate 40] [--workers 32]
"""

import argparse
import asyncio
import csv
import gzip
import json
import logging
import os
import signal
import sys
import time
from pathlib import Path

import aiohttp

from import_tmdb import Fatal, Tmdb, download_export, LANGUAGE

ROOT = Path(__file__).resolve().parents[2]
CACHE = Path(os.path.expanduser("~/.cache/nexus-catalog-import"))
MAX_CAST = 30  # с запасом: merge берёт 15 актёров, добирая из Wikidata

log = logging.getLogger("fetch-tmdb")


def env(key: str) -> str | None:
    """Переменная окружения, иначе строка `KEY=value` из `.env` или `infra/.env` (оба в .gitignore)."""
    if value := os.environ.get(key):
        return value
    for path in (ROOT / ".env", ROOT / "infra" / ".env"):
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                name, sep, value = line.strip().partition("=")
                if sep and name.strip() == key and not line.lstrip().startswith("#"):
                    return value.strip().strip("'\"") or None
    return None


def compact(data: dict) -> dict:
    """Только то, что использует merge.py: ~2 КБ вместо ~20 КБ полного ответа с crew."""
    credits = data.get("credits") or {}
    person = lambda c: {"id": c["id"], "name": c.get("name"), "original_name": c.get("original_name"),
                        "profile_path": c.get("profile_path")}
    directors, seen = [], set()
    for c in credits.get("crew") or []:
        if c.get("job") == "Director" and c["id"] not in seen:
            seen.add(c["id"])
            directors.append(person(c))
    cast = sorted(credits.get("cast") or [], key=lambda c: c.get("order", 1 << 30))[:MAX_CAST]
    return {
        "id": data["id"],
        "imdb_id": data.get("imdb_id"),
        "title": data.get("title"),
        "original_title": data.get("original_title"),
        "original_language": data.get("original_language"),
        "overview": data.get("overview"),
        "release_date": data.get("release_date"),
        "runtime": data.get("runtime"),
        "poster_path": data.get("poster_path"),
        "popularity": data.get("popularity"),
        "vote_count": data.get("vote_count"),
        "genres": [g["id"] for g in data.get("genres") or []],
        "countries": [c.get("iso_3166_1") for c in data.get("production_countries") or []],
        "studios": [c.get("name") for c in data.get("production_companies") or []],
        "directors": directors,
        "cast": [{**person(c), "character": c.get("character"), "order": c.get("order")}
                 for c in cast],
    }


def wikidata_tmdb_ids() -> set[int]:
    ids = set()
    for path in (CACHE / "wikidata").glob("basics_l*.csv"):
        with path.open(newline="", encoding="utf-8") as f:
            ids.update(int(r["tmdb"]) for r in csv.DictReader(f) if r["tmdb"].isdigit())
    return ids


def done_ids(cache: Path) -> set[int]:
    done = set()
    movies = cache / "movies.jsonl"
    if movies.exists():
        with movies.open(encoding="utf-8") as f:
            for line in f:
                try:
                    done.add(json.loads(line)["id"])
                except (json.JSONDecodeError, KeyError):  # строка, оборванная при остановке
                    pass
    missing = cache / "missing.txt"
    if missing.exists():
        done.update(int(x) for x in missing.read_text().split() if x.isdigit())
    return done


async def run(args):
    key = env("TMDB_API_KEY")
    if not key:
        sys.exit("TMDB_API_KEY: set it in the environment, .env or infra/.env")
    cache = Path(args.cache_dir)
    cache.mkdir(parents=True, exist_ok=True)
    stop = asyncio.Event()
    asyncio.get_running_loop().add_signal_handler(signal.SIGINT, stop.set)

    timeout = aiohttp.ClientTimeout(total=60, sock_connect=10, sock_read=30)
    connector = aiohttp.TCPConnector(limit=args.workers, ttl_dns_cache=300)
    async with aiohttp.ClientSession(timeout=timeout, connector=connector) as session:
        export = await download_export(session, cache)
        rows = []
        with gzip.open(export, "rt", encoding="utf-8") as f:
            for line in f:
                r = json.loads(line)
                if not r.get("video"):
                    rows.append((-(r.get("popularity") or 0.0), r["id"]))
        rows.sort()
        ids = [i for _, i in rows]
        extra = wikidata_tmdb_ids() - set(ids)
        log.info("tmdb: %d films in the export (without video), +%d only in Wikidata",
                 len(ids), len(extra))
        ids += sorted(extra)
        if args.limit:
            ids = ids[:args.limit]
        done = done_ids(cache)
        todo = [i for i in ids if i not in done]
        log.info("tmdb: cached %d, to fetch %d", len(ids) - len(todo), len(todo))

        tmdb = Tmdb(session, key, args.rate, args.retries)
        queue: asyncio.Queue = asyncio.Queue(maxsize=args.workers * 4)
        movies = (cache / "movies.jsonl").open("a", encoding="utf-8")
        missing = (cache / "missing.txt").open("a", encoding="utf-8")
        stats = {"ok": 0, "missing": 0, "errors": 0}
        started = time.monotonic()

        async def worker():
            while (tmdb_id := await queue.get()) is not None:
                try:
                    data = await tmdb.get(f"/movie/{tmdb_id}", append_to_response="credits",
                                          language=LANGUAGE)
                except Fatal:
                    raise
                except Exception as e:  # повторится при следующем запуске
                    log.error("tmdb %d: %r", tmdb_id, e)
                    stats["errors"] += 1
                    continue
                if data is None:
                    missing.write(f"{tmdb_id}\n")
                    stats["missing"] += 1
                else:
                    movies.write(json.dumps(compact(data), ensure_ascii=False) + "\n")
                    stats["ok"] += 1
                n = stats["ok"] + stats["missing"]
                if n % 5000 == 0:
                    movies.flush()
                    missing.flush()
                    speed = n / (time.monotonic() - started)
                    log.info("tmdb: %d/%d (%.1f/s, ETA %.1f h) ok %d, 404 %d, errors %d", n,
                             len(todo), speed, (len(todo) - n) / speed / 3600, stats["ok"],
                             stats["missing"], stats["errors"])

        async def produce():
            for tmdb_id in todo:
                if stop.is_set():
                    log.warning("stopping: finishing in-flight requests")
                    break
                await queue.put(tmdb_id)
            for _ in range(args.workers):
                await queue.put(None)

        try:
            await asyncio.gather(produce(), *(worker() for _ in range(args.workers)))
        finally:
            movies.close()
            missing.close()
    log.info("done: ok %d, 404 %d, errors %d (rerun to retry errors)", stats["ok"],
             stats["missing"], stats["errors"])


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    p.add_argument("--limit", type=int, default=0, help="only the N most popular (0 = all)")
    p.add_argument("--workers", type=int, default=32)
    p.add_argument("--rate", type=float, default=40.0, help="requests per second (TMDB ~50/IP)")
    p.add_argument("--retries", type=int, default=6)
    p.add_argument("--cache-dir", default=str(CACHE / "tmdb"))
    p.add_argument("-v", "--verbose", action="store_true")
    args = p.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(asctime)s %(levelname)s %(message)s", datefmt="%H:%M:%S")
    try:
        asyncio.run(run(args))
    except Fatal as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
