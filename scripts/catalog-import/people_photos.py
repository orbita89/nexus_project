#!/usr/bin/env python3
"""Галерея людей: несколько фото у самых известных (`person_stats.fame`).

Источник — TMDB `/person/{id}/images` (поле `profiles`): десятки портретов разных лет, с оценкой
голосов, тот же CDN, что и у главного фото. Wikidata (P18) даёт одну картинку на человека, поэтому
как источник галереи не годится.

Правила:
  * главное фото не меняется: текущий `photo_url` остаётся первым, остальные идут за ним;
  * берутся портреты шириной ≥ 400 px по убыванию `vote_average`, затем `vote_count`, затем ширины;
    дубли (тот же файл в другом размере) отбрасываются;
  * до `--per-person` фото (по умолчанию 6; бэкенд принимает до 20);
  * пишется только `people.photos` — триггер `people_sync_photos_trg` сам держит `photo_url`
    равным первому фото, поэтому поисковый индекс не меняется. Человек, у которого уже больше
    одного фото (правили руками), пропускается — без `--force` ничего не затирается.

По умолчанию сухой прогон (ничего не пишет); запись — `--apply`.

    TMDB_API_KEY=... DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        python3 people_photos.py --top 100            # посмотреть
    ... python3 people_photos.py --top 100 --apply    # записать

Ключ берётся только из окружения (`.env` скрипт не читает). Страницы людей на фронте обновит ISR
по расписанию или ручной публикацией.
"""

import argparse
import logging
import os
import sys
from concurrent.futures import ThreadPoolExecutor

import psycopg2

from tmdb_client import Limiter, Tmdb

log = logging.getLogger("people_photos")

PROFILE = "https://image.tmdb.org/t/p/h632"
MIN_WIDTH = 400
MAX_PHOTOS = 20  # `catalog::models::MAX_PERSON_PHOTOS`


def pick(current: str | None, profiles: list[dict], limit: int) -> list[str]:
    """Новая галерея: текущее главное фото, затем лучшие портреты TMDB без дублей."""
    photos = [current] if current else []
    seen = {current.rsplit("/", 1)[-1]} if current else set()
    ranked = sorted(
        (p for p in profiles if (p.get("width") or 0) >= MIN_WIDTH and p.get("file_path")),
        key=lambda p: (-(p.get("vote_average") or 0), -(p.get("vote_count") or 0),
                       -(p.get("width") or 0)),
    )
    for profile in ranked:
        name = profile["file_path"].lstrip("/")
        if name in seen:
            continue
        seen.add(name)
        photos.append(f"{PROFILE}/{name}")
        if len(photos) >= limit:
            break
    return photos


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--top", type=int, default=100, help="сколько самых известных (100)")
    parser.add_argument("--per-person", type=int, default=6, help="фото на человека (6)")
    parser.add_argument("--rps", type=float, default=20, help="запросов к TMDB в секунду (20)")
    parser.add_argument("--force", action="store_true", help="перезаписать и готовые галереи")
    parser.add_argument("--apply", action="store_true", help="записать в БД (иначе сухой прогон)")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s")

    key, dsn = os.environ.get("TMDB_API_KEY"), os.environ.get("DATABASE_URL")
    if not key or not dsn:
        log.error("нужны TMDB_API_KEY и DATABASE_URL в окружении")
        return 2
    limit = max(1, min(args.per_person, MAX_PHOTOS))

    conn = psycopg2.connect(dsn)
    with conn, conn.cursor() as cur:
        cur.execute(
            """SELECT p.id, p.slug, p.photo_url, cardinality(p.photos), ip.tmdb_id
                 FROM person_stats ps
                 JOIN people p ON p.id = ps.person_id
                 JOIN catalog_import.people ip ON ip.person_id = p.id
                ORDER BY ps.fame DESC, p.id LIMIT %s""",
            (args.top,),
        )
        rows = cur.fetchall()
    log.info("людей: %d", len(rows))

    tmdb = Tmdb(key, Limiter(args.rps), workers=8)

    def fetch(row):
        return row, tmdb.get(f"/person/{row[4]}/images")

    updates, skipped = [], 0
    with ThreadPoolExecutor(max_workers=8) as pool:
        for (person_id, slug, current, count, tmdb_id), data in pool.map(fetch, rows):
            if data is None:
                log.warning("%s: TMDB %s не найден", slug, tmdb_id)
                continue
            if count > 1 and not args.force:
                skipped += 1
                continue
            photos = pick(current, data.get("profiles", []), limit)
            log.info("%-28s было %d → стало %d", slug, count, len(photos))
            if len(photos) > count:
                updates.append((photos, person_id))

    log.info("к записи: %d, пропущено (уже галерея): %d", len(updates), skipped)
    if not args.apply:
        log.info("сухой прогон — ничего не записано; добавьте --apply")
        return 0
    with conn, conn.cursor() as cur:
        cur.executemany("UPDATE people SET photos = %s WHERE id = %s", updates)
    log.info("записано: %d", len(updates))
    return 0


if __name__ == "__main__":
    sys.exit(main())
