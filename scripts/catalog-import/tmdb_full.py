#!/usr/bin/env python3
"""Всё из TMDB для горячего слоя: полный ответ API по каждому фильму (и человеку) в БД.

Фильмы — строки `entities` со связью `entity_tmdb` (горячий слой, ~120 тыс.; архив Tantivy не
трогается). Для каждого — `GET /3/movie/{id}?language=ru-RU&append_to_response=<всё>`: титры
целиком, возрастные рейтинги по странам, ключевые слова, переводы, альтернативные названия,
картинки, видео, внешние id, где смотреть, рекомендации, похожие, рецензии TMDB, списки. Ответ
целиком — в `entity_tmdb_data.data` (jsonb, lz4), затем раскладывается (TMDB — источник правды
для горячего слоя):

- `entities`: русское название (кириллицу не меняем на латиницу, если у TMDB нет перевода),
  оригинальное, описание, дата выхода, постер; `metadata` — `runtime_min`, `countries`, `studios`,
  `age_rating` (RU, иначе US), `trailers` (YouTube: официальные трейлеры, русские первыми, до 5);
- `entity_stats`: popularity, число голосов TMDB, `imdb_id`;
- жанры → теги `kind = genre` (как `import_tmdb.py`), прочие теги сущности не трогаются;
- `entity_credits` заменяются: режиссёры, сценаристы, продюсеры, композиторы, операторы,
  монтажёры, затем **весь** актёрский состав по порядку TMDB; люди — по TMDB id
  (`catalog_import.people`), новые создаются (`slug` из имени, занят — `<slug>-<tmdb_id>`);
- в конце — сиквелы по коллекциям TMDB (`belongs_to_collection`): соседние по дате выхода фильмы
  одной коллекции → `entity_relations` (`sequel`), если связи ещё нет.

`--people` — второй этап: люди из титров горячего слоя, `GET /3/person/{id}?language=ru-RU&
append_to_response=external_ids,translations,images` → `person_tmdb_data`, `people.bio`
(русская биография, иначе прежняя), `birth_date`, `photo_url`. Фильмографию (`combined_credits`,
~200 КБ на человека) не берём: она дублирует титры фильмов.

Пачки фильмов пишутся без построчных триггеров (`session_replication_role = replica`, нужен
суперпользователь БД): счётчики людей и тегов пересчитываются в конце одним проходом, люди импорта
без работ удаляются. Перезапуск продолжает: уже скачанные пропускаются (`--refresh` — заново). Ключ —
`TMDB_API_KEY` (окружение или `infra/.env`). После — перестройка поиска
(`POST /api/v1/catalog/admin/search/reindex?force=true` или перезапуск приложения).

Запуск: DATABASE_URL=postgres://... python3 tmdb_full.py [--people] [--limit N] [--rate 40]
"""

import argparse
import asyncio
import json
import logging
import signal
import sys
import time

import aiohttp
import asyncpg

from fetch_tmdb import env
from import_tmdb import (POSTER, PROFILE, Fatal, Tmdb, clean, ensure_genres, movie_metadata,
                         parse_date, slugify, unique)
from merge import STATE_SCHEMA

MOVIE_APPEND = ",".join([
    "credits", "release_dates", "keywords", "translations", "alternative_titles", "images",
    "videos", "external_ids", "watch/providers", "recommendations", "similar", "reviews", "lists",
])
PERSON_APPEND = "external_ids,translations,images"
IMAGE_LANGUAGES = "ru,en,null"

# Должности TMDB → роль в `entity_credits` (a-z и `_`, как проверяет validate.rs).
CREW_ROLES = {
    "Director": "director",
    "Screenplay": "writer", "Writer": "writer", "Story": "writer", "Novel": "writer",
    "Author": "writer",
    "Producer": "producer",
    "Original Music Composer": "composer", "Music": "composer",
    "Director of Photography": "cinematographer",
    "Editor": "editor",
}
ROLE_ORDER = ["director", "writer", "producer", "composer", "cinematographer", "editor"]
MAX_TITLE = 300
MAX_TEXT = 20_000
MAX_TRAILERS = 5

log = logging.getLogger("tmdb-full")


def has_cyrillic(text: str | None) -> bool:
    return any("а" <= ch.lower() <= "я" or ch in "ёЁ" for ch in text or "")


def age_rating(data: dict) -> str | None:
    """Возрастной рейтинг: российский, иначе американский (первый непустой по дате)."""
    by_country = {r.get("iso_3166_1"): r.get("release_dates") or []
                  for r in (data.get("release_dates") or {}).get("results") or []}
    for country in ("RU", "US"):
        for release in by_country.get(country, []):
            cert = clean(release.get("certification"))
            if cert and cert.upper() not in ("NR", "UNRATED"):  # «без рейтинга» — не рейтинг
                return cert[:16]
    return None


def trailers(data: dict) -> list[dict]:
    """YouTube-трейлеры: официальные первыми, русские раньше английских, до MAX_TRAILERS."""
    videos = [v for v in (data.get("videos") or {}).get("results") or []
              if v.get("site") == "YouTube" and v.get("type") == "Trailer"
              and isinstance(v.get("key"), str) and len(v["key"]) == 11]
    lang = {"ru": 0, "en": 1}
    videos.sort(key=lambda v: (not v.get("official"), lang.get(v.get("iso_639_1"), 2),
                               v.get("published_at") or ""))
    return [{"provider": "youtube", "id": key}
            for key in unique(v["key"] for v in videos)][:MAX_TRAILERS]


def film_fields(data: dict, cur: asyncpg.Record) -> dict:
    """Новые значения полей `entities` по ответу TMDB и текущей строке."""
    title = clean(data.get("title"))
    if not title or (has_cyrillic(cur["title"]) and not has_cyrillic(title)):
        title = cur["title"]  # у TMDB нет русского перевода — оставляем русское из БД
    metadata = json.loads(cur["metadata"]) if isinstance(cur["metadata"], str) else dict(cur["metadata"] or {})
    metadata.update(movie_metadata(data))
    if rating := age_rating(data):
        metadata["age_rating"] = rating
    if found := trailers(data):
        metadata["trailers"] = found
    poster = data.get("poster_path")
    return {
        "title": title[:MAX_TITLE],
        "original_title": (clean(data.get("original_title")) or cur["original_title"] or "")[:MAX_TITLE] or None,
        "description": (clean(data.get("overview")) or cur["description"] or "")[:MAX_TEXT] or None,
        "release_date": parse_date(data.get("release_date")) or cur["release_date"],
        "cover_url": f"{POSTER}{poster}" if poster else cur["cover_url"],
        "metadata": json.dumps(metadata, ensure_ascii=False),
    }


def film_credits(data: dict) -> list[tuple[dict, str, str | None]]:
    """(человек TMDB, роль, персонаж) по порядку: съёмочная группа по ROLE_ORDER, затем актёры."""
    credits = data.get("credits") or {}
    crew = {role: [] for role in ROLE_ORDER}
    for c in credits.get("crew") or []:
        if role := CREW_ROLES.get(c.get("job")):
            crew[role].append((c, role, None))
    cast = sorted(credits.get("cast") or [], key=lambda c: c.get("order", 1 << 30))
    result = [item for role in ROLE_ORDER for item in crew[role]]
    result += [(c, "actor", (clean(c.get("character")) or "")[:MAX_TITLE] or None) for c in cast]
    return result


# ---------------------------------------------------------------- запись

class Stats:
    def __init__(self):
        self.fetched = self.missing = self.applied = self.credits = self.people_created = 0
        self.started = time.monotonic()

    def line(self, total: int) -> str:
        rate = self.fetched / max(1.0, time.monotonic() - self.started)
        left = (total - self.fetched - self.missing) / rate / 60 if rate else 0
        return (f"{self.fetched + self.missing}/{total} ({rate:.1f}/s, ~{left:.0f} min left) | "
                f"applied {self.applied}, 404 {self.missing}, credits {self.credits}, "
                f"new people {self.people_created}")


async def resolve_people(conn, persons: dict[int, dict], stats: Stats) -> dict[int, object]:
    """TMDB id человека → people.id; новых создаёт (slug из имени, занят — с TMDB id)."""
    ids = list(persons)
    known = {r["tmdb_id"]: r["person_id"] for r in await conn.fetch(
        "SELECT tmdb_id, person_id FROM catalog_import.people WHERE tmdb_id = ANY($1::int[])", ids)}
    new = [persons[i] for i in ids if i not in known]
    if not new:
        return known

    def base(p):
        return (slugify(clean(p.get("name")) or "") or slugify(p.get("original_name") or "")
                or f"person-{p['id']}")[:90]
    candidates = unique(s for p in new for s in (base(p), f"{base(p)}-{p['id']}"))
    taken = {r["slug"] for r in await conn.fetch(
        "SELECT slug FROM people WHERE slug = ANY($1::text[])", candidates)}
    rows = []
    for p in new:
        for slug in (base(p), f"{base(p)}-{p['id']}"):
            if slug not in taken:
                taken.add(slug)
                name = clean(p.get("name")) or clean(p.get("original_name")) or f"TMDB {p['id']}"
                photo = p.get("profile_path")
                rows.append((slug, name[:MAX_TITLE], f"{PROFILE}{photo}" if photo else None, p["id"]))
                break
    created = await conn.fetch(
        """INSERT INTO people (slug, full_name, photo_url)
           SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
           ON CONFLICT (slug) DO NOTHING RETURNING id, slug""",
        [r[0] for r in rows], [r[1] for r in rows], [r[2] for r in rows])
    by_slug = {r["slug"]: r["id"] for r in created}
    links = [(by_slug[r[0]], r[3]) for r in rows if r[0] in by_slug]
    await conn.execute(
        """INSERT INTO catalog_import.people (person_id, tmdb_id, created)
           SELECT p, t, true FROM unnest($1::uuid[], $2::int[]) AS u(p, t)
           ON CONFLICT DO NOTHING""",
        [l[0] for l in links], [l[1] for l in links])
    stats.people_created += len(links)
    known.update({t: p for p, t in links})
    return known


async def apply_movies(conn, batch: list[tuple[object, int, dict]], genres: dict, stats: Stats):
    """Пачка фильмов в одной транзакции: сырьё, поля, статистика, жанры, люди, титры."""
    async with conn.transaction():
        # Построчные триггеры счётчиков (титры, теги) — главный тормоз: выключены, счётчики
        # пересчитываются в конце (FINALIZE). Нужен суперпользователь.
        await conn.execute("SET LOCAL session_replication_role = replica")
        ids = [b[0] for b in batch]
        await conn.execute(
            """INSERT INTO entity_tmdb_data (entity_id, tmdb_id, data)
               SELECT * FROM unnest($1::uuid[], $2::int[], $3::jsonb[])
               ON CONFLICT (entity_id) DO UPDATE
               SET tmdb_id = EXCLUDED.tmdb_id, data = EXCLUDED.data, fetched_at = now()""",
            ids, [b[1] for b in batch], [json.dumps(b[2], ensure_ascii=False) for b in batch])

        current = {r["id"]: r for r in await conn.fetch(
            """SELECT id, title, original_title, description, release_date, cover_url, metadata
               FROM entities WHERE id = ANY($1::uuid[])""", ids)}
        rows = [(eid, film_fields(data, current[eid])) for eid, _, data in batch if eid in current]
        await conn.execute(
            """UPDATE entities e SET title = u.t, original_title = u.o, description = u.d,
                      release_date = u.r, cover_url = u.c, metadata = u.m::jsonb,
                      updated_at = now()
               FROM unnest($1::uuid[], $2::text[], $3::text[], $4::text[], $5::date[],
                           $6::text[], $7::text[]) AS u(id, t, o, d, r, c, m)
               WHERE e.id = u.id""",
            [r[0] for r in rows], [r[1]["title"] for r in rows],
            [r[1]["original_title"] for r in rows], [r[1]["description"] for r in rows],
            [r[1]["release_date"] for r in rows], [r[1]["cover_url"] for r in rows],
            [r[1]["metadata"] for r in rows])

        await conn.execute(
            """INSERT INTO entity_stats (entity_id, imdb_id, tmdb_popularity, tmdb_votes)
               SELECT * FROM unnest($1::uuid[], $2::text[], $3::float8[], $4::int[])
               ON CONFLICT (entity_id) DO UPDATE
               SET imdb_id = COALESCE(EXCLUDED.imdb_id, entity_stats.imdb_id),
                   tmdb_popularity = COALESCE(EXCLUDED.tmdb_popularity, entity_stats.tmdb_popularity),
                   tmdb_votes = COALESCE(EXCLUDED.tmdb_votes, entity_stats.tmdb_votes)""",
            ids,
            [clean(d.get("imdb_id") or (d.get("external_ids") or {}).get("imdb_id")) for _, _, d in batch],
            [d.get("popularity") for _, _, d in batch], [d.get("vote_count") for _, _, d in batch])

        with_genres = [(eid, [genres[g["id"]] for g in d.get("genres") or [] if g.get("id") in genres])
                       for eid, _, d in batch]
        with_genres = [(eid, tags) for eid, tags in with_genres if tags]
        if with_genres:
            await conn.execute(
                """DELETE FROM entity_tags et USING tags t
                   WHERE et.tag_id = t.id AND t.kind = 'genre' AND et.entity_id = ANY($1::uuid[])""",
                [eid for eid, _ in with_genres])
            pairs = [(eid, tag) for eid, tags in with_genres for tag in unique(tags)]
            await conn.execute(
                """INSERT INTO entity_tags (entity_id, tag_id)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[]) ON CONFLICT DO NOTHING""",
                [p[0] for p in pairs], [p[1] for p in pairs])

        film_people = [(eid, film_credits(d)) for eid, _, d in batch]
        film_people = [(eid, cr) for eid, cr in film_people if cr]
        persons = {c["id"]: c for _, cr in film_people for c, _, _ in cr if isinstance(c.get("id"), int)}
        person_ids = await resolve_people(conn, persons, stats) if persons else {}
        credit_rows, seen = [], set()
        for eid, cr in film_people:
            positions = {}
            for c, role, character in cr:
                pid = person_ids.get(c.get("id"))
                key = (eid, pid, role, character)
                if pid is None or key in seen:
                    continue
                seen.add(key)
                positions[role] = positions.get(role, -1) + 1
                credit_rows.append((eid, pid, role, character, positions[role]))
        if film_people:
            await conn.execute("DELETE FROM entity_credits WHERE entity_id = ANY($1::uuid[])",
                               [eid for eid, _ in film_people])
            await conn.execute(
                """INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::text[], $5::int[])""",
                [r[0] for r in credit_rows], [r[1] for r in credit_rows], [r[2] for r in credit_rows],
                [r[3] for r in credit_rows], [r[4] for r in credit_rows])
        stats.applied += len(rows)
        stats.credits += len(credit_rows)


# Счётчики, которые при записи держали бы триггеры (как в миграции entity_active), и люди
# импорта, оставшиеся без работ после замены титров.
FINALIZE = """
INSERT INTO person_credit_counts AS pc (person_id, credits, active_credits)
SELECT c.person_id, count(*), count(*) FILTER (WHERE e.active)
FROM entity_credits c JOIN entities e ON e.id = c.entity_id
GROUP BY c.person_id
ON CONFLICT (person_id) DO UPDATE
SET credits = EXCLUDED.credits, active_credits = EXCLUDED.active_credits
WHERE (pc.credits, pc.active_credits) IS DISTINCT FROM (EXCLUDED.credits, EXCLUDED.active_credits);
UPDATE person_credit_counts pc SET credits = 0, active_credits = 0
WHERE (pc.credits, pc.active_credits) <> (0, 0)
  AND NOT EXISTS (SELECT 1 FROM entity_credits c WHERE c.person_id = pc.person_id);
UPDATE tags t SET entities_count = (
    SELECT count(*) FROM entity_tags et JOIN entities e ON e.id = et.entity_id
    WHERE et.tag_id = t.id AND e.active);
DELETE FROM people p
WHERE NOT EXISTS (SELECT 1 FROM entity_credits c WHERE c.person_id = p.id)
  AND EXISTS (SELECT 1 FROM catalog_import.people i WHERE i.person_id = p.id);
"""

COLLECTION_RELATIONS = """
WITH c AS (
    SELECT d.entity_id, (d.data -> 'belongs_to_collection' ->> 'id')::int AS collection,
           e.release_date
    FROM entity_tmdb_data d JOIN entities e ON e.id = d.entity_id
    WHERE d.data -> 'belongs_to_collection' ->> 'id' IS NOT NULL
), o AS (
    SELECT entity_id AS from_id,
           lead(entity_id) OVER (PARTITION BY collection
                                 ORDER BY release_date NULLS LAST, entity_id) AS to_id
    FROM c
)
INSERT INTO entity_relations (from_id, to_id, kind)
SELECT from_id, to_id, 'sequel' FROM o
WHERE to_id IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM entity_relations r
                  WHERE (r.from_id, r.to_id) IN ((o.from_id, o.to_id), (o.to_id, o.from_id)))
ON CONFLICT DO NOTHING
"""


async def apply_people(conn, batch: list[tuple[object, int, dict]], stats: Stats):
    async with conn.transaction():
        ids = [b[0] for b in batch]
        await conn.execute(
            """INSERT INTO person_tmdb_data (person_id, tmdb_id, data)
               SELECT * FROM unnest($1::uuid[], $2::int[], $3::jsonb[])
               ON CONFLICT (person_id) DO UPDATE
               SET tmdb_id = EXCLUDED.tmdb_id, data = EXCLUDED.data, fetched_at = now()""",
            ids, [b[1] for b in batch], [json.dumps(b[2], ensure_ascii=False) for b in batch])
        await conn.execute(
            """UPDATE people p SET bio = COALESCE(u.b, p.bio), birth_date = COALESCE(u.d, p.birth_date),
                      photo_url = COALESCE(u.f, p.photo_url)
               FROM unnest($1::uuid[], $2::text[], $3::date[], $4::text[]) AS u(id, b, d, f)
               WHERE p.id = u.id""",
            ids,
            [(clean(d.get("biography")) or "")[:MAX_TEXT] or None for _, _, d in batch],
            [parse_date(d.get("birthday")) for _, _, d in batch],
            [f"{PROFILE}{d['profile_path']}" if d.get("profile_path") else None for _, _, d in batch])
        stats.applied += len(batch)


# ---------------------------------------------------------------- загрузка

async def run(args):
    dsn = env("DATABASE_URL")
    key = env("TMDB_API_KEY")
    if not dsn or not key:
        sys.exit("DATABASE_URL and TMDB_API_KEY are required")
    pool = await asyncpg.create_pool(dsn, min_size=1, max_size=2)
    async with pool.acquire() as conn:
        await conn.execute(STATE_SCHEMA)
        if args.people:
            todo = await conn.fetch(
                f"""SELECT p.id, i.tmdb_id FROM people p
                    JOIN catalog_import.people i ON i.person_id = p.id
                    WHERE i.tmdb_id IS NOT NULL
                      AND EXISTS (SELECT 1 FROM entity_credits c WHERE c.person_id = p.id)
                      {"" if args.refresh else
                       "AND NOT EXISTS (SELECT 1 FROM person_tmdb_data d WHERE d.person_id = p.id)"}
                    ORDER BY i.tmdb_id""")
        else:
            todo = await conn.fetch(
                f"""SELECT e.id, t.tmdb_id FROM entities e
                    JOIN entity_tmdb t ON t.entity_id = e.id
                    LEFT JOIN entity_stats s ON s.entity_id = e.id
                    {"" if args.refresh else
                     "WHERE NOT EXISTS (SELECT 1 FROM entity_tmdb_data d WHERE d.entity_id = e.id)"}
                    ORDER BY s.tmdb_popularity DESC NULLS LAST""")
    if args.limit:
        todo = todo[:args.limit]
    total = len(todo)
    log.info("%s to fetch: %d", "people" if args.people else "films", total)

    stats = Stats()
    queue: asyncio.Queue = asyncio.Queue()
    for row in todo:
        queue.put_nowait((row["id"], row["tmdb_id"]))
    results: asyncio.Queue = asyncio.Queue(maxsize=args.batch_size * 4)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    loop.add_signal_handler(signal.SIGINT, stop.set)
    loop.add_signal_handler(signal.SIGTERM, stop.set)

    async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=60)) as session:
        tmdb = Tmdb(session, key, args.rate, retries=6)
        async with pool.acquire() as conn:
            genres = {} if args.people else await ensure_genres(conn, tmdb)

        async def worker():
            while not stop.is_set():
                try:
                    local_id, tmdb_id = queue.get_nowait()
                except asyncio.QueueEmpty:
                    return
                if args.people:
                    data = await tmdb.get(f"/person/{tmdb_id}", language="ru-RU",
                                          append_to_response=PERSON_APPEND,
                                          include_image_language=IMAGE_LANGUAGES)
                else:
                    data = await tmdb.get(f"/movie/{tmdb_id}", language="ru-RU",
                                          append_to_response=MOVIE_APPEND,
                                          include_image_language=IMAGE_LANGUAGES,
                                          include_video_language=IMAGE_LANGUAGES)
                await results.put((local_id, tmdb_id, data))

        async def writer():
            batch = []
            last_log = time.monotonic()
            async with pool.acquire() as conn:
                while True:
                    item = await results.get()
                    if item is not None:
                        if item[2] is None:
                            stats.missing += 1
                        else:
                            stats.fetched += 1
                            batch.append(item)
                    if batch and (item is None or len(batch) >= args.batch_size):
                        if args.people:
                            await apply_people(conn, batch, stats)
                        else:
                            await apply_movies(conn, batch, genres, stats)
                        batch = []
                    if time.monotonic() - last_log > 30 or item is None:
                        log.info(stats.line(total))
                        last_log = time.monotonic()
                    if item is None:
                        return

        writer_task = asyncio.create_task(writer())
        workers = [asyncio.create_task(worker()) for _ in range(args.workers)]
        try:
            await asyncio.gather(*workers)
        finally:
            await results.put(None)
            await writer_task

    if not args.people:
        async with pool.acquire() as conn:
            # Счётчики — и после остановки: пачки уже записаны без триггеров.
            started = time.monotonic()
            await conn.execute(FINALIZE)
            log.info("counters recomputed, orphan people removed (%.0fs)", time.monotonic() - started)
            if not stop.is_set():
                added = await conn.execute(COLLECTION_RELATIONS)
                log.info("collections → sequel relations: %s", added)
    await pool.close()
    log.info("done%s: %s", " (interrupted)" if stop.is_set() else "", stats.line(total))


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--people", action="store_true", help="второй этап: люди из титров")
    p.add_argument("--refresh", action="store_true", help="скачать заново уже скачанное")
    p.add_argument("--limit", type=int, default=0, help="не больше N (0 — все)")
    p.add_argument("--rate", type=float, default=40, help="запросов в секунду (TMDB пускает ~50)")
    p.add_argument("--workers", type=int, default=32)
    p.add_argument("--batch-size", type=int, default=100)
    args = p.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    try:
        asyncio.run(run(args))
    except Fatal as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
