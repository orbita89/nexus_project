"""Добор картинок из TMDB к уже залитому каталогу.

Что берётся из одного запроса по сущности (`/tv/{id}` или `/movie/{id}` с
`append_to_response=credits`):

    фото людей     ← `credits.cast[].profile_path` (и `created_by` у сериалов)
    логотипы студий← `production_companies[].logo_path`
    обложка        ← `poster_path`, если в базе её нет

Почему так, а не по одному человеку: фото 760 тыс. людей через `/person/{id}` — это 760 тыс.
запросов, а те же фото приходят пачками по 15 вместе с сериалом или фильмом, которых всего
295 тыс. Людей, оставшихся без фото после этого прохода, добирает `--mode person`.

Человек ищется сначала по TMDB id (`catalog_import.people`), иначе по точному имени — и только
если имя в базе одно: тёзкам чужое фото не ставим. Найденной паре «человек ↔ TMDB id» проставляем
связь, чтобы следующий проход не искал по имени.

Запись идемпотентна: `photo_url`/`logo_url`/`cover_url` заполняются только там, где они пустые,
поэтому прогон можно повторять. Прогресс пишется в чекпоинт, прерывание по Ctrl+C — мягкое.

    python images_tmdb.py --mode credits            # сериалы и фильмы (по умолчанию)
    python images_tmdb.py --mode credits --kind tv  # только сериалы
    python images_tmdb.py --mode person             # добор по оставшимся людям с TMDB id
"""

from __future__ import annotations

import argparse
import logging
import os
import sys
import threading
import time
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from pathlib import Path

import psycopg2
import psycopg2.extras

from tmdb_client import Limiter, Tmdb, append_checkpoint, load_checkpoint, stop_event

LANGUAGE = "ru-RU"
IMAGE = "https://image.tmdb.org/t/p"
PROFILE = f"{IMAGE}/w300"
LOGO = f"{IMAGE}/w300"
POSTER = f"{IMAGE}/w500"
BATCH = 200

log = logging.getLogger("images")


def url(prefix: str, path: str | None) -> str | None:
    return f"{prefix}{path}" if path else None


class Writer(threading.Thread):
    """Пишет пачками в своём соединении: очередь даёт бэкпрешер, если БД отстаёт."""

    def __init__(self, dsn: str, queue, checkpoint: Path | None, fast_commit: bool):
        super().__init__(daemon=True)
        self.dsn = dsn
        self.queue = queue
        self.checkpoint = checkpoint
        self.fast_commit = fast_commit
        self.stats = {"photos": 0, "logos": 0, "covers": 0, "links": 0, "failed": 0}

    def run(self) -> None:
        conn = psycopg2.connect(self.dsn)
        try:
            batch, done_ids, deadline = [], [], time.monotonic() + 0.5
            while True:
                item = self.queue.get()
                if item is not None:
                    batch.append(item)
                    done_ids.append(item["id"])
                full = len(batch) >= BATCH or time.monotonic() >= deadline
                if batch and (full or item is None):
                    self.flush(conn, batch, done_ids)
                    batch, done_ids, deadline = [], [], time.monotonic() + 0.5
                if item is None:
                    return
        finally:
            conn.close()

    def flush(self, conn, batch: list[dict], done_ids: list[int]) -> None:
        try:
            with conn.cursor() as cur:
                if self.fast_commit:
                    cur.execute("SET LOCAL synchronous_commit = off")
                written = self.write(cur, batch)
            conn.commit()
            # Считаем только то, что пережило коммит: при откате пачки ничего не записано.
            for key, value in written.items():
                self.stats[key] += value
            append_checkpoint(self.checkpoint, done_ids)
        except psycopg2.Error as error:
            conn.rollback()
            self.stats["failed"] += len(batch)
            log.warning("пачка не записана (%s шт.): %s", len(batch), error)

    def write(self, cur, batch: list[dict]) -> dict[str, int]:
        written = {"photos": 0, "logos": 0, "covers": 0, "links": 0}
        by_tmdb = {}
        by_name = {}
        logos = {}
        covers = []
        for row in batch:
            for person in row["people"]:
                if person["photo"]:
                    by_tmdb[person["tmdb_id"]] = person["photo"]
                    by_name.setdefault(person["name"], person["photo"])
            for name, logo in row["studios"]:
                if logo:
                    logos.setdefault(name, logo)
            if row["cover"]:
                covers.append((row["entity_id"], row["cover"]))

        if by_tmdb:
            # Человек уже связан с TMDB — самый надёжный путь.
            updated = psycopg2.extras.execute_values(
                cur,
                """UPDATE people p SET photo_url = v.photo
                     FROM (VALUES %s) AS v(tmdb_id, photo), catalog_import.people ip
                    WHERE ip.tmdb_id = v.tmdb_id::int AND p.id = ip.person_id
                      AND p.photo_url IS NULL
                RETURNING p.id""",
                list(by_tmdb.items()),
                fetch=True,
            )
            written["photos"] += len(updated)

        if by_name:
            # Иначе — по имени, но только если носитель имени в базе один (тёзкам не ставим).
            updated = psycopg2.extras.execute_values(
                cur,
                """UPDATE people p SET photo_url = v.photo
                     FROM (VALUES %s) AS v(full_name, photo)
                    WHERE p.full_name = v.full_name AND p.photo_url IS NULL
                      AND NOT EXISTS (SELECT 1 FROM people q
                                       WHERE q.full_name = v.full_name AND q.id <> p.id)
                RETURNING p.id""",
                list(by_name.items()),
                fetch=True,
            )
            written["photos"] += len(updated)

        if by_tmdb:
            # Связь «человек ↔ TMDB id», чтобы следующий проход не искал по имени.
            pairs = [(name, tmdb_id) for tmdb_id, _ in by_tmdb.items()
                     for name in [next((p["name"] for row in batch for p in row["people"]
                                        if p["tmdb_id"] == tmdb_id), None)] if name]
            if pairs:
                linked = psycopg2.extras.execute_values(
                    cur,
                    """INSERT INTO catalog_import.people (person_id, tmdb_id, created)
                       SELECT p.id, v.tmdb_id::int, false
                         FROM (VALUES %s) AS v(full_name, tmdb_id)
                        JOIN people p ON p.full_name = v.full_name
                        WHERE NOT EXISTS (SELECT 1 FROM people q
                                           WHERE q.full_name = v.full_name AND q.id <> p.id)
                       ON CONFLICT DO NOTHING
                    RETURNING person_id""",
                    pairs,
                    fetch=True,
                )
                written["links"] += len(linked)

        if logos:
            updated = psycopg2.extras.execute_values(
                cur,
                """UPDATE studios s SET logo_url = v.logo
                     FROM (VALUES %s) AS v(name, logo)
                    WHERE s.name = v.name AND s.logo_url IS NULL
                RETURNING s.id""",
                list(logos.items()),
                fetch=True,
            )
            written["logos"] += len(updated)

        if covers:
            updated = psycopg2.extras.execute_values(
                cur,
                """UPDATE entities e SET cover_url = v.cover
                     FROM (VALUES %s) AS v(id, cover)
                    WHERE e.id = v.id::uuid AND e.cover_url IS NULL
                RETURNING e.id""",
                covers,
                fetch=True,
            )
            written["covers"] += len(updated)

        return written


def targets(conn, kinds: tuple[str, ...], limit: int) -> list[tuple[int, str, str, bool]]:
    """`(tmdb_id, entity_id, путь TMDB, нужна ли обложка)` — сущности с id TMDB."""
    rows: list[tuple[int, str, str, bool]] = []
    with conn.cursor() as cur:
        if "tv" in kinds:
            cur.execute(
                """SELECT (external_ids->>'tmdb_tv')::int, id::text, cover_url IS NULL
                     FROM entities WHERE kind = 'series' AND external_ids ? 'tmdb_tv'
                    ORDER BY created_at, id"""
            )
            rows += [(r[0], r[1], "tv", r[2]) for r in cur.fetchall()]
        if "movie" in kinds:
            cur.execute(
                """SELECT (external_ids->>'tmdb')::int, id::text, cover_url IS NULL
                     FROM entities WHERE kind = 'movie' AND external_ids ? 'tmdb'
                    ORDER BY created_at, id"""
            )
            rows += [(r[0], r[1], "movie", r[2]) for r in cur.fetchall()]
    return rows[:limit] if limit else rows


def credits_row(tmdb: Tmdb, tmdb_id: int, entity_id: str, path: str, want_cover: bool):
    data = tmdb.get(f"/{path}/{tmdb_id}", language=LANGUAGE, append_to_response="credits")
    if not data:
        return None
    people = []
    for person in (data.get("created_by") or []):
        if person.get("id") and person.get("name"):
            people.append({"tmdb_id": person["id"], "name": person["name"],
                           "photo": url(PROFILE, person.get("profile_path"))})
    for person in ((data.get("credits") or {}).get("cast") or [])[:15]:
        if person.get("id") and person.get("name"):
            people.append({"tmdb_id": person["id"], "name": person["name"],
                           "photo": url(PROFILE, person.get("profile_path"))})
    studios = [(c["name"], url(LOGO, c.get("logo_path")))
               for c in (data.get("production_companies") or []) if c.get("name")]
    return {
        "id": tmdb_id,
        "entity_id": entity_id,
        "people": people,
        "studios": studios,
        "cover": url(POSTER, data.get("poster_path")) if want_cover else None,
    }


def person_targets(conn, limit: int) -> list[tuple[int, str]]:
    """Люди с известным TMDB id, но без фото — для `--mode person`."""
    with conn.cursor() as cur:
        cur.execute(
            """SELECT ip.tmdb_id, p.id::text FROM catalog_import.people ip
                 JOIN people p ON p.id = ip.person_id
                WHERE p.photo_url IS NULL ORDER BY ip.tmdb_id"""
        )
        rows = [(r[0], r[1]) for r in cur.fetchall()]
    return rows[:limit] if limit else rows


def person_row(tmdb: Tmdb, tmdb_id: int, person_id: str):
    data = tmdb.get(f"/person/{tmdb_id}", language=LANGUAGE)
    if not data:
        return None
    photo = url(PROFILE, data.get("profile_path"))
    name = data.get("name")
    if not photo or not name:
        return None
    return {"id": tmdb_id, "entity_id": None, "cover": None, "studios": [],
            "people": [{"tmdb_id": tmdb_id, "name": name, "photo": photo}]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("credits", "person"), default="credits")
    parser.add_argument("--kind", choices=("tv", "movie", "all"), default="all")
    parser.add_argument("--limit", type=int, default=0, help="0 — без ограничения")
    parser.add_argument("--workers", type=int, default=48)
    parser.add_argument("--rps", type=float, default=45)
    parser.add_argument("--checkpoint", type=Path, default=None)
    parser.add_argument("--fast-commit", action="store_true")
    parser.add_argument("--insecure", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    key = os.environ.get("TMDB_API_KEY")
    dsn = os.environ.get("DATABASE_URL")
    if not key or not dsn:
        log.error("нужны TMDB_API_KEY и DATABASE_URL")
        return 2

    checkpoint = args.checkpoint or Path(__file__).parent / "cache" / f"images-{args.mode}.checkpoint"
    done = load_checkpoint(checkpoint)
    log.info("чекпоинт: пропущено %s", len(done))

    conn = psycopg2.connect(dsn)
    try:
        if args.mode == "credits":
            kinds = ("tv", "movie") if args.kind == "all" else (args.kind,)
            tasks = [t for t in targets(conn, kinds, args.limit) if t[0] not in done]
        else:
            tasks = [t for t in person_targets(conn, args.limit) if t[0] not in done]
    finally:
        conn.close()
    log.info("в работе: %s", len(tasks))
    if not tasks:
        return 0

    import queue as queue_mod

    stop = stop_event()
    limiter = Limiter(args.rps)
    tmdb = Tmdb(key, limiter, workers=args.workers, verify=not args.insecure)
    sink: queue_mod.Queue = queue_mod.Queue(maxsize=args.workers * 8)
    writer = Writer(dsn, sink, checkpoint, args.fast_commit)
    writer.start()

    started = time.monotonic()
    processed = 0
    pending = set()
    tasks_iter = iter(tasks)
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        def submit_more() -> None:
            while len(pending) < args.workers * 4 and not stop.is_set():
                try:
                    task = next(tasks_iter)
                except StopIteration:
                    return
                if args.mode == "credits":
                    tmdb_id, entity_id, path, want_cover = task
                    pending.add(pool.submit(credits_row, tmdb, tmdb_id, entity_id, path, want_cover))
                else:
                    tmdb_id, person_id = task
                    pending.add(pool.submit(person_row, tmdb, tmdb_id, person_id))

        submit_more()
        last = started
        while pending:
            done_now, pending_set = wait(pending, return_when=FIRST_COMPLETED, timeout=10)
            pending = set(pending_set)
            for future in done_now:
                processed += 1
                try:
                    row = future.result()
                except Exception as error:  # noqa: BLE001 — одна сущность не валит прогон
                    log.warning("ошибка запроса: %s", error)
                    row = None
                if row:
                    sink.put(row)
            if not stop.is_set():
                submit_more()
            now = time.monotonic()
            if now - last >= 10:
                last = now
                log.info(
                    "обработано %s/%s (%.1f/с) · БД: фото %s, логотипов %s, обложек %s,"
                    " связей %s, ошибок %s · сеть: %s запросов, 429 %s",
                    processed, len(tasks), processed / max(now - started, 1e-9),
                    writer.stats["photos"], writer.stats["logos"], writer.stats["covers"],
                    writer.stats["links"], writer.stats["failed"],
                    tmdb.stats.get("requests", 0), tmdb.stats.get("retry_429", 0),
                )

    sink.put(None)
    writer.join()
    log.info("done за %.0f с: %s, сеть %s", time.monotonic() - started, writer.stats, tmdb.stats)
    return 0


if __name__ == "__main__":
    sys.exit(main())
