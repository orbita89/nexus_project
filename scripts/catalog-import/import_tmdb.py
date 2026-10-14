#!/usr/bin/env python3
"""Массовый импорт фильмов из TMDB в каталог Nexus: entities, people, entity_credits, tags, entity_tags.

ETL:
  1. Extract — ежедневный дамп id (`movie_ids_MM_DD_YYYY.json.gz`), фильтр: не adult, не video,
     popularity > --min-popularity, не больше --limit самых популярных.
  2. Transform — пул воркеров, `GET /3/movie/{id}?append_to_response=credits&language=ru-RU`,
     общий ограничитель скорости (--rate) и повторы на 429/5xx/сетевые ошибки.
  3. Load — один писатель, пачки по --batch-size фильмов, каждая пачка — одна транзакция,
     вставка через unnest(массивы) + ON CONFLICT DO NOTHING. id генерирует база (DEFAULT uuidv7()).

Перезапуск безопасен: соответствие TMDB id → id Nexus хранится в служебной схеме `tmdb_import`
(создаётся скриптом, приложение её не читает). Уже загруженные фильмы и 404 при перезапуске
пропускаются, оборванная пачка не закоммичена и загрузится заново.

Существующие строки (сиды, правки админки) не перезаписываются: фильм со свободным slug
создаётся; занятый slug, ещё не привязанный к TMDB, — это тот же фильм (slug = название + год),
он привязывается без изменений, и его титры и жанры не трогаются; занятый другим фильмом TMDB —
новый получает slug с суффиксом `-<tmdb_id>`. Люди — так же по slug имени.

После загрузки перестройте поиск: scripts/reindex-search.sh (Meilisearch не знает о строках,
записанных в обход API).

Запуск: TMDB_API_KEY=... DATABASE_URL=postgres://... python3 import_tmdb.py [--help]
"""

import argparse
import asyncio
import gzip
import heapq
import json
import logging
import os
import random
import re
import signal
import sys
import time
import unicodedata
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

import aiohttp
import asyncpg

API = "https://api.themoviedb.org/3"
EXPORTS = "https://files.tmdb.org/p/exports"
POSTER = "https://image.tmdb.org/t/p/w780"
PROFILE = "https://image.tmdb.org/t/p/h632"
LANGUAGE = "ru-RU"
MAX_ACTORS = 15

# Лимиты metadata фильма из modules/catalog/src/metadata.rs: значения вне них API отверг бы (400).
MAX_RUNTIME = 10000
MAX_STUDIOS = 20
MAX_STUDIO = 120

# Жанры TMDB → slug тега. Совпадает со slug сидовых тегов (sci-fi, detective, ...), чтобы фильмы
# попадали в уже существующие жанры, а не в дубли. Новый жанр TMDB — slug из английского названия.
GENRE_SLUGS = {
    28: "action", 12: "adventure", 16: "animation", 35: "comedy", 80: "crime",
    99: "documentary", 18: "drama", 10751: "family", 14: "fantasy", 36: "history",
    27: "horror", 10402: "music", 9648: "detective", 10749: "romance", 878: "sci-fi",
    10770: "tv-movie", 53: "thriller", 10752: "war", 37: "western",
}

log = logging.getLogger("tmdb-import")


# ---------------------------------------------------------------- slug

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh", "з": "z",
    "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "у": "u", "ф": "f", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh",
    "щ": "shch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
    "і": "i", "ї": "yi", "є": "ye", "ґ": "g", "ў": "u",
    # Латиница, которую NFKD не раскладывает на букву + диакритику.
    "ß": "ss", "æ": "ae", "œ": "oe", "ø": "o", "đ": "d", "ł": "l", "þ": "th", "ð": "d",
}
MAX_SLUG_BASE = 80


def slugify(text: str) -> str:
    """«Дюна» → dyuna, «Timothée Chalamet» → timothee-chalamet, «J.R.R. Tolkien» → jrr-tolkien.
    Буквы других алфавитов (иероглифы и т. п.) отбрасываются — может вернуть пустую строку."""
    text = "".join(TRANSLIT.get(ch, ch) for ch in text.lower())
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    text = re.sub(r"['’.]", "", text)
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].rstrip("-")


def is_latin(text: str) -> bool:
    letters = [ch for ch in text if ch.isalpha()]
    return bool(letters) and all(unicodedata.name(ch, "").startswith("LATIN") for ch in letters)


# ---------------------------------------------------------------- transform

@dataclass
class Person:
    tmdb_id: int
    full_name: str
    slug_base: str
    photo_url: str | None


@dataclass
class Credit:
    person: Person
    role: str  # director | actor
    character_name: str | None
    position: int


@dataclass
class Movie:
    tmdb_id: int
    slug_base: str
    title: str
    original_title: str | None
    description: str | None
    release_date: date | None
    cover_url: str | None
    metadata: dict
    genre_ids: list[int]
    credits: list[Credit] = field(default_factory=list)
    details: bool = True  # False — строка из дампа (--export-only), без запроса деталей


def clean(value) -> str | None:
    value = (value or "").strip()
    return value or None


def unique(values):
    seen, result = set(), []
    for value in values:
        if value not in seen:
            seen.add(value)
            result.append(value)
    return result


def parse_date(value) -> date | None:
    try:
        return date.fromisoformat(value) if value else None
    except ValueError:
        return None


def movie_metadata(data: dict) -> dict:
    """Только поля фильма, уже нормализованные так, как это делает metadata.rs."""
    metadata = {}
    runtime = data.get("runtime")
    if isinstance(runtime, int) and 1 <= runtime <= MAX_RUNTIME:
        metadata["runtime_min"] = runtime
    countries = unique(
        code
        for c in data.get("production_countries") or []
        if re.fullmatch(r"[A-Z]{2}", code := (c.get("iso_3166_1") or "").strip().upper())
    )
    if countries:
        metadata["countries"] = countries
    studios = unique(
        name[:MAX_STUDIO].strip()
        for c in data.get("production_companies") or []
        if (name := clean(c.get("name")))
    )[:MAX_STUDIOS]
    if studios:
        metadata["studios"] = studios
    return metadata


def person_from(raw: dict) -> Person:
    name = clean(raw.get("name")) or clean(raw.get("original_name")) or f"TMDB {raw['id']}"
    base = slugify(name) or slugify(raw.get("original_name") or "") or f"person-{raw['id']}"
    profile = raw.get("profile_path")
    return Person(raw["id"], name, base, f"{PROFILE}{profile}" if profile else None)


def transform(data: dict) -> Movie | None:
    tmdb_id = data["id"]
    original_title = clean(data.get("original_title"))
    title = clean(data.get("title")) or original_title
    if not title:
        return None
    release_date = parse_date(data.get("release_date"))

    # Латинский оригинал — как в сидах (dune-2021), иначе транслит русского названия.
    source = original_title if original_title and is_latin(original_title) else title
    base = slugify(source) or slugify(title) or f"movie-{tmdb_id}"
    if release_date:
        base = f"{base}-{release_date.year}"

    poster = data.get("poster_path")
    movie = Movie(
        tmdb_id=tmdb_id,
        slug_base=base,
        title=title,
        original_title=original_title,
        description=clean(data.get("overview")),
        release_date=release_date,
        cover_url=f"{POSTER}{poster}" if poster else None,
        metadata=movie_metadata(data),
        genre_ids=unique(g["id"] for g in data.get("genres") or []),
    )

    credits = data.get("credits") or {}
    seen = set()  # UNIQUE NULLS NOT DISTINCT (entity_id, person_id, role, character_name)
    position = 0

    def add(raw, role, character) -> bool:
        nonlocal position
        key = (raw["id"], role, character)
        if key in seen:
            return False
        seen.add(key)
        movie.credits.append(Credit(person_from(raw), role, character, position))
        position += 1
        return True

    for raw in credits.get("crew") or []:
        if raw.get("job") == "Director":
            add(raw, "director", None)
    actors = 0
    for raw in sorted(credits.get("cast") or [], key=lambda c: c.get("order", 1 << 30)):
        if actors == MAX_ACTORS:
            break
        actors += add(raw, "actor", clean(raw.get("character")))
    return movie


# ---------------------------------------------------------------- extract

async def download_export(session: aiohttp.ClientSession, cache: Path) -> Path:
    """Дамп публикуется около 08:00 UTC — если сегодняшнего ещё нет, берём вчерашний и т. д."""
    cache.mkdir(parents=True, exist_ok=True)
    today = datetime.now(timezone.utc).date()
    for back in range(4):
        day = today - timedelta(days=back)
        name = f"movie_ids_{day:%m_%d_%Y}.json.gz"
        path = cache / name
        if path.exists():
            log.info("export: %s (cache)", path)
            return path
        async with session.get(f"{EXPORTS}/{name}") as resp:
            if resp.status in (403, 404):
                log.info("export: %s not published yet", name)
                continue
            resp.raise_for_status()
            tmp = path.with_suffix(".part")
            with tmp.open("wb") as f:
                async for chunk in resp.content.iter_chunked(1 << 20):
                    f.write(chunk)
            tmp.rename(path)
            log.info("export: downloaded %s (%.1f MB)", name, path.stat().st_size / 1e6)
            return path
    raise RuntimeError("no TMDB movie export for the last 4 days")


def select_ids(path: Path, min_popularity: float, limit: int) -> list[int]:
    """Строки дампа: {"adult":false,"id":550,"original_title":"...","popularity":61.4,"video":false}."""
    top: list[tuple[float, int]] = []  # min-heap: в памяти только limit лучших
    total = 0
    with gzip.open(path, "rt", encoding="utf-8") as f:
        for line in f:
            total += 1
            row = json.loads(line)
            popularity = row.get("popularity") or 0.0
            if row.get("adult") or row.get("video") or popularity <= min_popularity:
                continue
            item = (popularity, row["id"])
            if len(top) < limit:
                heapq.heappush(top, item)
            elif item > top[0]:
                heapq.heapreplace(top, item)
    ids = [tmdb_id for _, tmdb_id in sorted(top, reverse=True)]
    log.info("export: %d rows, selected %d (popularity > %g, limit %d)",
             total, len(ids), min_popularity, limit)
    return ids


def export_movies(path: Path) -> list[tuple[int, str]]:
    """--export-only: (tmdb_id, original_title) всех фильмов дампа, кроме adult, по убыванию
    popularity — самым популярным достаются slug без суффикса."""
    rows = []
    with gzip.open(path, "rt", encoding="utf-8") as f:
        for line in f:
            row = json.loads(line)
            if row.get("adult") or not (title := clean(row.get("original_title"))):
                continue
            rows.append((-(row.get("popularity") or 0.0), row["id"], title))
    rows.sort()
    log.info("export: %d films (without adult)", len(rows))
    return [(tmdb_id, title) for _, tmdb_id, title in rows]


def movie_from_export(tmdb_id: int, title: str) -> Movie:
    """В дампе нет ни года, ни русского названия: slug — только из оригинального названия."""
    return Movie(tmdb_id=tmdb_id, slug_base=slugify(title) or f"movie-{tmdb_id}", title=title,
                 original_title=title, description=None, release_date=None, cover_url=None,
                 metadata={}, genre_ids=[], details=False)


async def run_export_only(args, dsn: str):
    async with aiohttp.ClientSession() as session:
        rows = export_movies(await download_export(session, Path(args.cache_dir)))
    pool = await asyncpg.create_pool(dsn, min_size=1, max_size=1)
    try:
        async with pool.acquire() as conn:
            await conn.execute(STATE_SCHEMA)
            done = {r["tmdb_id"] for r in await conn.fetch("SELECT tmdb_id FROM tmdb_import.movies")}
            todo = [r for r in rows if r[0] not in done]
            log.info("todo: %d (already imported: %d)", len(todo), len(rows) - len(todo))
            stats, started = Stats(), time.monotonic()
            for i in range(0, len(todo), args.batch_size):
                batch = [movie_from_export(*r) for r in todo[i:i + args.batch_size]]
                await load_batch(conn, batch, [], {}, stats)
                processed = i + len(batch)
                if processed % (args.batch_size * 50) < args.batch_size or processed == len(todo):
                    speed = processed / (time.monotonic() - started)
                    log.info("%d/%d (%.0f films/s, ETA %dm) | entities +%d, adopted %d",
                             processed, len(todo), speed, (len(todo) - processed) / speed // 60,
                             stats.created, stats.adopted)
    finally:
        await pool.close()
    log.info("done: entities +%d, adopted %d", stats.created, stats.adopted)
    log.info("next: scripts/reindex-search.sh — rebuild Meilisearch")


# ---------------------------------------------------------------- TMDB client

class RateLimiter:
    """Равномерно не чаще rate запросов в секунду на все воркеры."""

    def __init__(self, rate: float):
        self.interval = 1.0 / rate
        self.next = time.monotonic()
        self.lock = asyncio.Lock()

    async def wait(self):
        async with self.lock:
            now = time.monotonic()
            self.next = max(self.next, now)
            delay = self.next - now
            self.next += self.interval
        if delay > 0:
            await asyncio.sleep(delay)


class Fatal(Exception):
    pass


class Tmdb:
    def __init__(self, session: aiohttp.ClientSession, key: str, rate: float, retries: int):
        self.session = session
        self.limiter = RateLimiter(rate)
        self.retries = retries
        # Ключ API v3 — параметром, токен чтения v4 (JWT) — заголовком.
        self.headers = {"Accept": "application/json"}
        self.params = {}
        if key.startswith("eyJ"):
            self.headers["Authorization"] = f"Bearer {key}"
        else:
            self.params["api_key"] = key
        self.requests = 0

    async def get(self, path: str, **params) -> dict | None:
        """JSON ответа; None — 404 (фильм удалён из TMDB)."""
        for attempt in range(self.retries + 1):
            await self.limiter.wait()
            self.requests += 1
            delay = min(60.0, 2 ** attempt) + random.random()
            try:
                async with self.session.get(f"{API}{path}", headers=self.headers,
                                            params={**self.params, **params}) as resp:
                    if resp.status == 200:
                        return await resp.json()
                    if resp.status == 404:
                        return None
                    if resp.status == 401:
                        raise Fatal("TMDB 401: check TMDB_API_KEY")
                    if resp.status == 429:
                        delay = float(resp.headers.get("Retry-After") or delay)
                    elif resp.status < 500:
                        raise Fatal(f"TMDB {resp.status} for {path}: {await resp.text()}")
                    reason = f"HTTP {resp.status}"
            except (aiohttp.ClientError, asyncio.TimeoutError) as e:
                reason = repr(e)
            log.debug("%s: %s, retry %d in %.1fs", path, reason, attempt + 1, delay)
            await asyncio.sleep(delay)
        raise RuntimeError(f"{path}: gave up after {self.retries} retries ({reason})")


# ---------------------------------------------------------------- load

STATE_SCHEMA = """
CREATE SCHEMA IF NOT EXISTS tmdb_import;
CREATE TABLE IF NOT EXISTS tmdb_import.movies (
    tmdb_id     integer     PRIMARY KEY,
    entity_id   uuid        NOT NULL REFERENCES public.entities (id) ON DELETE CASCADE,
    created     boolean     NOT NULL,  -- false: строка уже была (сиды, админка), её не меняем
    details     boolean     NOT NULL DEFAULT true,  -- false: только название из дампа (--export-only)
    imported_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS movies_entity_id_idx ON tmdb_import.movies (entity_id);
CREATE TABLE IF NOT EXISTS tmdb_import.people (
    tmdb_id   integer PRIMARY KEY,
    person_id uuid    NOT NULL REFERENCES public.people (id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS people_person_id_idx ON tmdb_import.people (person_id);
CREATE TABLE IF NOT EXISTS tmdb_import.skipped (
    tmdb_id integer     PRIMARY KEY,
    reason  text        NOT NULL,
    at      timestamptz NOT NULL DEFAULT now()
);
"""


@dataclass
class Stats:
    created: int = 0
    adopted: int = 0
    skipped: int = 0
    people: int = 0
    credits: int = 0
    tags: int = 0


def claim_slugs(items, base_of, fallback_of, existing: dict) -> dict:
    """slug → решение для каждого элемента: ("create", slug) | ("adopt", id) | ("conflict", None).
    existing: slug → (id, привязан ли к TMDB, подходит ли для привязки)."""
    taken, result = set(), {}
    for item in items:
        decision = ("conflict", None)
        for slug in (base_of(item), fallback_of(item)):
            if slug in taken:
                continue
            row = existing.get(slug)
            if row is None:
                decision = ("create", slug)
            elif not row[1] and row[2]:
                decision = ("adopt", row[0])
            else:
                continue
            taken.add(slug)
            break
        result[id(item)] = decision
    return result


async def ensure_genres(conn: asyncpg.Connection, tmdb: Tmdb) -> dict[int, object]:
    ru = await tmdb.get("/genre/movie/list", language=LANGUAGE) or {}
    en = await tmdb.get("/genre/movie/list", language="en-US") or {}
    en_names = {g["id"]: g["name"] for g in en.get("genres", [])}
    slugs, names = [], []
    for g in ru.get("genres", []):
        slug = GENRE_SLUGS.get(g["id"]) or slugify(en_names.get(g["id"], "")) or f"genre-{g['id']}"
        name = (g["name"] or en_names.get(g["id"]) or slug).strip()
        slugs.append(slug)
        names.append(name[:1].upper() + name[1:])
    # Существующие теги (сиды, админка) не меняем: ни название, ни kind.
    await conn.execute(
        """INSERT INTO tags (slug, name, kind)
           SELECT s, n, 'genre' FROM unnest($1::text[], $2::text[]) AS u(s, n)
           ON CONFLICT (slug) DO NOTHING""", slugs, names)
    rows = await conn.fetch("SELECT id, slug FROM tags WHERE slug = ANY($1::text[])", slugs)
    by_slug = {r["slug"]: r["id"] for r in rows}
    genres = {g["id"]: by_slug[slug] for g, slug in zip(ru.get("genres", []), slugs)}
    log.info("genres: %d tags ready", len(genres))
    return genres


async def load_batch(conn: asyncpg.Connection, movies: list[Movie], missing: list[tuple[int, str]],
                     genres: dict[int, object], stats: Stats):
    async with conn.transaction():
        if missing:
            await conn.execute(
                """INSERT INTO tmdb_import.skipped (tmdb_id, reason)
                   SELECT * FROM unnest($1::int[], $2::text[]) ON CONFLICT DO NOTHING""",
                [m[0] for m in missing], [m[1] for m in missing])
            stats.skipped += len(missing)
        if not movies:
            return

        # 1. entities: создать, привязать готовую или взять slug с суффиксом.
        fallback = lambda m: f"{m.slug_base}-{m.tmdb_id}"
        candidates = unique(s for m in movies for s in (m.slug_base, fallback(m)))
        rows = await conn.fetch(
            """SELECT e.id, e.slug, e.kind::text = 'movie' AS movie, (t.tmdb_id IS NOT NULL) AS linked
               FROM entities e LEFT JOIN tmdb_import.movies t ON t.entity_id = e.id
               WHERE e.slug = ANY($1::text[])""", candidates)
        existing = {r["slug"]: (r["id"], r["linked"], r["movie"]) for r in rows}
        decisions = claim_slugs(movies, lambda m: m.slug_base, fallback, existing)

        to_create = [(m, d[1]) for m in movies if (d := decisions[id(m)])[0] == "create"]
        inserted = await conn.fetch(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, metadata)
               SELECT 'movie', s, t, o, d, r, c, m::jsonb
               FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::date[], $6::text[],
                           $7::text[]) AS u(s, t, o, d, r, c, m)
               ON CONFLICT (slug) DO NOTHING
               RETURNING id, slug""",
            [s for _, s in to_create], [m.title for m, _ in to_create],
            [m.original_title for m, _ in to_create], [m.description for m, _ in to_create],
            [m.release_date for m, _ in to_create], [m.cover_url for m, _ in to_create],
            [json.dumps(m.metadata, ensure_ascii=False) for m, _ in to_create])
        new_ids = {r["slug"]: r["id"] for r in inserted}

        links: list[tuple[int, object, bool]] = []
        created: list[tuple[Movie, object]] = []
        for m in movies:
            kind, value = decisions[id(m)]
            if kind == "create" and value in new_ids:
                links.append((m.tmdb_id, new_ids[value], True, m.details))
                created.append((m, new_ids[value]))
            elif kind == "adopt":
                links.append((m.tmdb_id, value, False, m.details))
            else:
                # Оба slug заняты другими фильмами или slug заняли параллельно (админка) —
                # не записываем, следующий запуск попробует снова.
                log.warning("tmdb %d: slug %s is taken, skipped", m.tmdb_id, m.slug_base)
        await conn.execute(
            """INSERT INTO tmdb_import.movies (tmdb_id, entity_id, created, details)
               SELECT * FROM unnest($1::int[], $2::uuid[], $3::bool[], $4::bool[])
               ON CONFLICT DO NOTHING""",
            *map(list, zip(*links)) if links else ([], [], [], []))
        stats.created += len(created)
        stats.adopted += sum(1 for l in links if not l[2])

        # Титры и жанры — только у созданных импортом: готовые карточки ведёт админка.
        if not created:
            return

        # 2. people: известные по TMDB id, затем создать / привязать по slug.
        persons = {c.person.tmdb_id: c.person for m, _ in created for c in m.credits}
        rows = await conn.fetch(
            "SELECT tmdb_id, person_id FROM tmdb_import.people WHERE tmdb_id = ANY($1::int[])",
            list(persons))
        person_ids = {r["tmdb_id"]: r["person_id"] for r in rows}
        fresh = sorted((p for t, p in persons.items() if t not in person_ids), key=lambda p: p.tmdb_id)
        if fresh:
            pfallback = lambda p: f"{p.slug_base}-{p.tmdb_id}"
            rows = await conn.fetch(
                """SELECT p.id, p.slug, (t.tmdb_id IS NOT NULL) AS linked
                   FROM people p LEFT JOIN tmdb_import.people t ON t.person_id = p.id
                   WHERE p.slug = ANY($1::text[])""",
                unique(s for p in fresh for s in (p.slug_base, pfallback(p))))
            existing = {r["slug"]: (r["id"], r["linked"], True) for r in rows}
            decisions = claim_slugs(fresh, lambda p: p.slug_base, pfallback, existing)
            to_create = [(p, d[1]) for p in fresh if (d := decisions[id(p)])[0] == "create"]
            inserted = await conn.fetch(
                """INSERT INTO people (slug, full_name, photo_url)
                   SELECT * FROM unnest($1::text[], $2::text[], $3::text[])
                   ON CONFLICT (slug) DO NOTHING
                   RETURNING id, slug""",
                [s for _, s in to_create], [p.full_name for p, _ in to_create],
                [p.photo_url for p, _ in to_create])
            new_people = {r["slug"]: r["id"] for r in inserted}
            stats.people += len(new_people)
            new_links = []
            for p in fresh:
                kind, value = decisions[id(p)]
                if kind == "create" and value in new_people:
                    new_links.append((p.tmdb_id, new_people[value]))
                elif kind == "adopt":
                    new_links.append((p.tmdb_id, value))
            await conn.execute(
                """INSERT INTO tmdb_import.people (tmdb_id, person_id)
                   SELECT * FROM unnest($1::int[], $2::uuid[]) ON CONFLICT DO NOTHING""",
                [l[0] for l in new_links], [l[1] for l in new_links])
            person_ids.update(new_links)

        # 3. entity_credits
        credits = [(eid, person_ids[c.person.tmdb_id], c.role, c.character_name, c.position)
                   for m, eid in created for c in m.credits if c.person.tmdb_id in person_ids]
        result = await conn.execute(
            """INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
               SELECT * FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::text[], $5::int[])
               ON CONFLICT DO NOTHING""",
            *map(list, zip(*credits))) if credits else "INSERT 0 0"
        stats.credits += int(result.split()[-1])

        # 4. entity_tags
        pairs = [(eid, genres[g]) for m, eid in created for g in m.genre_ids if g in genres]
        if pairs:
            result = await conn.execute(
                """INSERT INTO entity_tags (entity_id, tag_id)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[]) ON CONFLICT DO NOTHING""",
                [p[0] for p in pairs], [p[1] for p in pairs])
            stats.tags += int(result.split()[-1])


# ---------------------------------------------------------------- pipeline

DONE = object()


async def worker(tmdb: Tmdb, ids: asyncio.Queue, results: asyncio.Queue):
    while (tmdb_id := await ids.get()) is not DONE:
        try:
            data = await tmdb.get(f"/movie/{tmdb_id}", append_to_response="credits", language=LANGUAGE)
            if data is None:
                await results.put(("missing", tmdb_id, "not found"))
            elif (movie := transform(data)) is None:
                await results.put(("missing", tmdb_id, "no title"))
            else:
                await results.put(("movie", movie))
        except Fatal:
            raise
        except Exception as e:  # один битый фильм не роняет импорт; повторится при перезапуске
            log.error("tmdb %d: %r", tmdb_id, e)
            await results.put(("error", tmdb_id))
    await results.put(DONE)


async def writer(pool: asyncpg.Pool, results: asyncio.Queue, workers: int, total: int,
                 batch_size: int, genres: dict, tmdb: Tmdb):
    stats, movies, missing = Stats(), [], []
    finished = processed = errors = 0
    started = time.monotonic()

    async def flush():
        nonlocal movies, missing
        if not movies and not missing:
            return
        async with pool.acquire() as conn:
            await load_batch(conn, movies, missing, genres, stats)
        movies, missing = [], []
        elapsed = time.monotonic() - started
        speed = processed / elapsed if elapsed else 0
        eta = (total - processed) / speed if speed else 0
        log.info("%d/%d (%.1f films/s, %d req, ETA %dm) | entities +%d, adopted %d, people +%d, "
                 "credits +%d, tags +%d, skipped %d, errors %d",
                 processed, total, speed, tmdb.requests, eta // 60, stats.created, stats.adopted,
                 stats.people, stats.credits, stats.tags, stats.skipped, errors)

    while finished < workers:
        try:
            item = await asyncio.wait_for(results.get(), timeout=5)
        except asyncio.TimeoutError:
            await flush()  # медленный поток — всё равно коммитим каждые 5 секунд
            continue
        if item is DONE:
            finished += 1
            continue
        processed += 1
        if item[0] == "movie":
            movies.append(item[1])
        elif item[0] == "missing":
            missing.append((item[1], item[2]))
        else:
            errors += 1
        if len(movies) + len(missing) >= batch_size:
            await flush()
    await flush()
    return stats, errors


async def run(args):
    key, dsn = os.environ.get("TMDB_API_KEY"), os.environ.get("DATABASE_URL")
    if args.export_only:
        if not dsn:
            sys.exit("DATABASE_URL is required")
        return await run_export_only(args, dsn)
    if not key or (not dsn and not args.dry_run):
        sys.exit("TMDB_API_KEY and DATABASE_URL are required")

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    loop.add_signal_handler(signal.SIGINT, lambda: (log.warning("stopping: finishing in-flight "
                            "requests and the last batch (Ctrl+\\ to kill)"), stop.set()))

    timeout = aiohttp.ClientTimeout(total=60, sock_connect=10, sock_read=30)
    connector = aiohttp.TCPConnector(limit=args.workers, ttl_dns_cache=300)
    async with aiohttp.ClientSession(timeout=timeout, connector=connector) as session:
        tmdb = Tmdb(session, key, args.rate, args.retries)

        if args.ids:
            ids = [int(i) for i in args.ids.split(",")]
        else:
            ids = select_ids(await download_export(session, Path(args.cache_dir)),
                             args.min_popularity, args.limit)

        if args.dry_run:
            for tmdb_id in ids[:args.dry_run]:
                data = await tmdb.get(f"/movie/{tmdb_id}", append_to_response="credits",
                                      language=LANGUAGE)
                movie = transform(data) if data else None
                print(json.dumps(movie, default=lambda o: o.__dict__ if hasattr(o, "__dict__")
                                 else str(o), ensure_ascii=False, indent=2))
            return

        pool = await asyncpg.create_pool(dsn, min_size=1, max_size=2)
        try:
            async with pool.acquire() as conn:
                await conn.execute(STATE_SCHEMA)
                done = {r["tmdb_id"] for r in await conn.fetch(
                    "SELECT tmdb_id FROM tmdb_import.movies UNION SELECT tmdb_id FROM tmdb_import.skipped")}
                genres = await ensure_genres(conn, tmdb)
            todo = [i for i in ids if i not in done]
            log.info("todo: %d (already imported or missing: %d)", len(todo), len(ids) - len(todo))

            id_queue: asyncio.Queue = asyncio.Queue(maxsize=args.workers * 2)
            results: asyncio.Queue = asyncio.Queue(maxsize=args.batch_size * 2)
            workers = [asyncio.create_task(worker(tmdb, id_queue, results))
                       for _ in range(args.workers)]
            write = asyncio.create_task(writer(pool, results, args.workers, len(todo),
                                               args.batch_size, genres, tmdb))

            async def produce():
                for tmdb_id in todo:
                    if stop.is_set():
                        break
                    await id_queue.put(tmdb_id)
                for _ in workers:
                    await id_queue.put(DONE)

            producer = asyncio.create_task(produce())
            try:
                # Первая же ошибка (Fatal у воркера, сбой БД у писателя) останавливает всё.
                await asyncio.gather(producer, write, *workers)
            except BaseException:
                for task in (producer, write, *workers):
                    task.cancel()
                raise
            stats, errors = write.result()
        finally:
            await pool.close()

    log.info("done: entities +%d, adopted %d, people +%d, credits +%d, tags +%d, skipped %d, "
             "errors %d (rerun to retry errors)", stats.created, stats.adopted, stats.people,
             stats.credits, stats.tags, stats.skipped, errors)
    log.info("next: scripts/reindex-search.sh — rebuild Meilisearch")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    p.add_argument("--min-popularity", type=float, default=15.0,
                   help="keep films with popularity above this (0 = only --limit)")
    p.add_argument("--limit", type=int, default=50000, help="at most N most popular films")
    p.add_argument("--workers", type=int, default=32, help="concurrent TMDB requests")
    p.add_argument("--rate", type=float, default=40.0,
                   help="max TMDB requests per second (TMDB allows ~50 per IP)")
    p.add_argument("--retries", type=int, default=6, help="retries on 429/5xx/network errors")
    p.add_argument("--batch-size", type=int, default=500, help="films per DB transaction")
    p.add_argument("--cache-dir", default=os.path.expanduser("~/.cache/nexus-catalog-import/tmdb"),
                   help="where to keep the daily export; outside the repo (cargo-watch)")
    p.add_argument("--ids", help="comma-separated TMDB ids instead of the export (e.g. 438631,693134)")
    p.add_argument("--export-only", action="store_true",
                   help="no API key: load every non-adult film of the export, title only")
    p.add_argument("--dry-run", type=int, metavar="N", default=0,
                   help="fetch and print N transformed films, write nothing")
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
