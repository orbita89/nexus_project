#!/usr/bin/env python3
"""Слияние TMDB и Wikidata в каталог Nexus: один фильм — одна строка, данные из обоих источников.

Источники только скачивают (кэш в ~/.cache/nexus-catalog-import): fetch_tmdb.py → tmdb/movies.jsonl,
import_wikidata.py --stage wiki → wikidata/*.csv + wiki.jsonl. Этот скрипт читает кэш и пишет в БД.

Идентичность — служебная схема `catalog_import`: `movies` (entity_id ↔ tmdb_id ↔ qid) и `people`
(person_id ↔ tmdb_id ↔ qid). Wikidata связывает источники сама: P4947 — TMDB id фильма, P4985 —
TMDB id человека. Фильм ищется сначала по связке, потом среди готовых непривязанных (сиды) по
slug или «название + год», и только потом создаётся — дублей нет.

Поля (первый непустой): название — русское из TMDB, затем из Wikidata, затем любое; описание —
TMDB (ru), затем вступление ru.wikipedia; постер — TMDB, затем Commons (несвободные постеры
Википедии не используются); дата и длительность — TMDB, затем Wikidata. Страны, студии, жанры,
режиссёры — объединение. Актёры — порядок TMDB (как в титрах), до 15, добор из Wikidata;
персонаж — русский из Wikidata, если в TMDB не по-русски. Люди: имя — русское (Wikidata, затем
TMDB), фото — TMDB, затем Commons, дата рождения и bio — Wikidata.

Ручные правки не теряются: в `written` хранится то, что скрипт записал в каждое поле (и хэш титров
и жанров). Поле обновляется, только если оно пустое или в нём всё ещё записанное скриптом.
Готовые строки (сиды, админка) — только заполнение пустых полей, титры и жанры не трогаются.

Запуск: DATABASE_URL=postgres://... python3 merge.py [--tmdb-limit N]; затем
scripts/reindex-search.sh. Перезапуск безопасен: повторное слияние ничего не меняет.
"""

import argparse
import asyncio
import hashlib
import json
import logging
import re
import sys
import time
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path

import aiohttp
import asyncpg

import import_wikidata as wd
from fetch_tmdb import CACHE, env
from import_tmdb import (GENRE_SLUGS, MAX_RUNTIME, MAX_STUDIO, MAX_STUDIOS, clean, is_latin,
                         parse_date, slugify, unique)

POSTER = "https://image.tmdb.org/t/p/w780"   # как у постеров в сидах
PROFILE = "https://image.tmdb.org/t/p/h632"
MAX_ACTORS = 15
CYRILLIC = re.compile("[а-яё]", re.IGNORECASE)
TAG_NAMES = {**wd.GENRE_NAMES, "music": "Музыка", "tv-movie": "Телефильм"}
MOVIE_FIELDS = ("title", "original_title", "description", "release_date", "cover_url", "metadata")
PERSON_FIELDS = ("full_name", "birth_date", "photo_url", "bio")
# Ключи metadata из TMDB: не под защитой правок (их обновляют импорт и админка «Обновить
# popularity»), иначе свежая popularity выглядела бы ручной правкой metadata.
TMDB_KEYS = ("popularity", "vote_count")


def split_tmdb(metadata) -> tuple[dict, dict]:
    m = dict(metadata or {})
    return ({k: v for k, v in m.items() if k not in TMDB_KEYS},
            {k: m[k] for k in TMDB_KEYS if k in m})

log = logging.getLogger("merge")


# ---------------------------------------------------------------- merged records

@dataclass
class MPerson:
    tmdb_id: int | None
    qid: str | None
    full_name: str
    slug_base: str
    birth_date: date | None
    photo_url: str | None
    bio: str | None
    fame: int

    @property
    def key(self):
        return ("t", self.tmdb_id) if self.tmdb_id else ("q", self.qid)

    def fields(self) -> dict:
        return {"full_name": self.full_name, "birth_date": self.birth_date,
                "photo_url": self.photo_url, "bio": self.bio}


@dataclass
class MCredit:
    person: MPerson
    role: str
    character_name: str | None
    position: int


@dataclass
class MFilm:
    tmdb_id: int | None
    qid: str | None
    slug_base: str
    year: int | None
    title: str
    original_title: str | None
    description: str | None
    release_date: date | None
    cover_url: str | None
    metadata: dict
    tags: list[str]
    credits: list[MCredit] = field(default_factory=list)

    def fields(self) -> dict:
        return {"title": self.title, "original_title": self.original_title,
                "description": self.description, "release_date": self.release_date,
                "cover_url": self.cover_url, "metadata": self.metadata}


def first(values, cyrillic: bool = False):
    for v in values:
        if v and (not cyrillic or CYRILLIC.search(v)):
            return v
    return None


def free_image(url: str | None) -> str | None:
    """Commons — свободные файлы; загруженные в саму en/ru.wikipedia — нет."""
    return url if url and "/wikipedia/commons/" in url or url and "commons.wikimedia.org" in url else None


class Merger:
    def __init__(self, data: wd.Dataset):
        self.data = data
        # TMDB id фильма → QID: при нескольких — самый известный (top упорядочен по sitelinks).
        self.film_qid: dict[int, str] = {}
        for q in sorted(data.basics, key=lambda q: data.order[q]):
            if t := data.basics[q]["tmdb"]:
                self.film_qid.setdefault(t, q)
        self.person_qid: dict[int, str] = {}
        for q, p in data.people.items():
            if p.get("tmdb"):
                self.person_qid.setdefault(p["tmdb"], q)
        # TMDB id человека → (имя, оригинальное имя, фото) из уже прочитанных титров: человек
        # из фильма «только Wikidata» получает те же имя и фото, что и в фильме из TMDB.
        self.tmdb_people: dict[int, tuple] = {}

    def learn_tmdb_people(self, records):
        """Предпроход по кэшу TMDB: имя и фото людей, связанных с Wikidata (P4985). Без него
        человек, впервые встреченный в Wikidata-части фильма, обновлялся бы дважды за прогон."""
        for t in records:
            for raw in (t.get("directors") or []) + (t.get("cast") or []):
                if raw["id"] in self.person_qid:
                    known = self.tmdb_people.get(raw["id"])
                    if known is None or (raw.get("profile_path") and not known[2]):
                        self.tmdb_people[raw["id"]] = (raw.get("name"), raw.get("original_name"),
                                                       raw.get("profile_path"))

    # -- люди

    def person_tmdb(self, raw: dict) -> MPerson:
        if raw["id"] in self.tmdb_people:  # одна запись о человеке во всех фильмах
            name, original, profile = self.tmdb_people[raw["id"]]
            raw = {"id": raw["id"], "name": name, "original_name": original, "profile_path": profile}
        q = self.person_qid.get(raw["id"])
        w = self.data.person(q) if q else None
        p = self.data.people.get(q, {}) if q else {}
        tname, oname = clean(raw.get("name")), clean(raw.get("original_name"))
        name = (first([w and w.full_name, tname], cyrillic=True)
                or first([tname, oname, w and w.full_name]) or f"TMDB {raw['id']}")
        latin = first([x for x in (p.get("en"), oname, tname) if x and is_latin(x)])
        base = slugify(latin or name) or f"person-t{raw['id']}"
        photo = f"{PROFILE}{raw['profile_path']}" if raw.get("profile_path") else (w and w.photo_url)
        return MPerson(raw["id"], q, name[:wd.MAX_TITLE], base, w and w.birth_date, photo,
                       w and w.bio, (w.links if w else 0))

    def person_wd(self, w: wd.Person) -> MPerson:
        tmdb_id = self.data.people.get(w.qid, {}).get("tmdb")
        if tmdb_id and self.person_qid.get(tmdb_id) != w.qid:
            tmdb_id = None  # тот же TMDB id у другой записи Wikidata (дубль) — он её
        if tmdb_id and (known := self.tmdb_people.get(tmdb_id)):
            return self.person_tmdb({"id": tmdb_id, "name": known[0], "original_name": known[1],
                                     "profile_path": known[2]})
        return MPerson(tmdb_id, w.qid, w.full_name, w.slug_base, w.birth_date, w.photo_url, w.bio,
                       w.links)

    # -- фильм

    def film(self, t: dict | None, q: str | None) -> MFilm | None:
        w = self.data.film(q) if q else None
        b = self.data.basics.get(q) if q else None
        ttitle = clean(t.get("title")) if t else None
        title = first([ttitle, w and w.title], cyrillic=True) or first([ttitle, w and w.title])
        if not title:
            return None
        original = (clean(t.get("original_title")) if t else None) or (w and w.original_title)
        tdate = parse_date(t.get("release_date")) if t else None
        release = tdate or (w and w.release_date)
        year = tdate.year if tdate else (w and w.year)
        base = slugify(wd.slug_source(original, b and b["en"], title)) or slugify(title)
        base = base or (f"movie-t{t['id']}" if t else f"movie-{q.lower()}")

        metadata = {}
        runtime = t.get("runtime") if t else None
        if isinstance(runtime, int) and 1 <= runtime <= MAX_RUNTIME:
            metadata["runtime_min"] = runtime
        elif w and w.metadata.get("runtime_min"):
            metadata["runtime_min"] = w.metadata["runtime_min"]
        countries = [c.strip().upper() for c in (t.get("countries") if t else None) or [] if c]
        countries = unique(c for c in countries + (w.metadata.get("countries", []) if w else [])
                           if re.fullmatch(r"[A-Z]{2}", c))
        if countries:
            metadata["countries"] = countries
        studios, seen = [], set()
        for s in ((t.get("studios") if t else None) or []) + (w.metadata.get("studios", []) if w else []):
            s = (clean(s) or "")[:MAX_STUDIO].strip()
            if s and s.casefold() not in seen:
                seen.add(s.casefold())
                studios.append(s)
        if studios := studios[:MAX_STUDIOS]:
            metadata["studios"] = studios
        # Известность по TMDB: сортировки поиска и «Пересобрать активные» в админке.
        popularity = t.get("popularity") if t else None
        if isinstance(popularity, (int, float)) and 0 <= popularity <= 1_000_000:
            metadata["popularity"] = round(float(popularity), 3)
        votes = t.get("vote_count") if t else None
        if isinstance(votes, int) and votes >= 0:
            metadata["vote_count"] = votes

        tags = [GENRE_SLUGS[g] for g in (t.get("genres") if t else None) or [] if g in GENRE_SLUGS]
        poster = f"{POSTER}{t['poster_path']}" if t and t.get("poster_path") else None
        # TMDB id у записи Wikidata — только если он её (у фильма и его версии, например
        # Apocalypse Now и Redux, в Wikidata бывает один id): иначе вторая перезаписала бы первую.
        own_tmdb = b and b["tmdb"] if b and self.film_qid.get(b["tmdb"]) == q else None
        film = MFilm(
            tmdb_id=t["id"] if t else own_tmdb, qid=q, slug_base=base, year=year,
            title=title[:wd.MAX_TITLE], original_title=original[:wd.MAX_TITLE] if original else None,
            description=((clean(t.get("overview")) if t else None) or (w and w.description) or None),
            release_date=release, cover_url=poster or free_image(w and w.cover_url),
            metadata=metadata, tags=unique(tags + (w.tags if w else [])))
        if film.description:
            film.description = film.description[:wd.MAX_LONG_TEXT]

        # Титры: TMDB главный, Wikidata добирает. Один человек — одна роль каждого вида.
        wd_people = {c.person.qid: c for c in (w.credits if w else [])}
        added, pos = set(), 0

        def add(person: MPerson, role: str, character: str | None):
            nonlocal pos
            k = (person.key, role)
            if k in added or (person.qid and (("q", person.qid), role) in added):
                return False
            added.add(k)
            if person.qid:
                added.add((("q", person.qid), role))
            film.credits.append(MCredit(person, role, character, pos))
            pos += 1
            return True

        for raw in (t.get("directors") if t else None) or []:
            add(self.person_tmdb(raw), "director", None)
        for c in (w.credits if w else []):
            if c.role == "director":
                add(self.person_wd(c.person), "director", None)
        actors = 0
        for raw in (t.get("cast") if t else None) or []:
            if actors == MAX_ACTORS:
                break
            person = self.person_tmdb(raw)
            char = clean(raw.get("character"))
            wc = wd_people.get(person.qid) if person.qid else None
            if wc and wc.character_name and CYRILLIC.search(wc.character_name) and not (
                    char and CYRILLIC.search(char)):
                char = wc.character_name
            actors += add(person, "actor", char)
        for c in (w.credits if w else []):
            if actors == MAX_ACTORS:
                break
            if c.role == "actor":
                actors += add(self.person_wd(c.person), "actor", c.character_name)
        return film


# ---------------------------------------------------------------- DB

STATE_SCHEMA = """
CREATE SCHEMA IF NOT EXISTS catalog_import;
CREATE TABLE IF NOT EXISTS catalog_import.movies (
    entity_id uuid        PRIMARY KEY REFERENCES public.entities (id) ON DELETE CASCADE,
    tmdb_id   integer     UNIQUE,
    qid       text        UNIQUE,
    created   boolean     NOT NULL,  -- false: строка уже была (сиды, админка) — только пустые поля
    written   jsonb       NOT NULL DEFAULT '{}',  -- что скрипт записал: поля, хэши титров и жанров
    merged_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS catalog_import.people (
    person_id uuid    PRIMARY KEY REFERENCES public.people (id) ON DELETE CASCADE,
    tmdb_id   integer UNIQUE,
    qid       text    UNIQUE,
    created   boolean NOT NULL,
    written   jsonb   NOT NULL DEFAULT '{}'
);
"""

# Перенос связок первого импорта (import_wikidata.py): written = {} у созданных значит «всё
# в строке записал импорт», так что слияние вправе это обновить.
MIGRATE_WIKIDATA = """
INSERT INTO catalog_import.movies (entity_id, tmdb_id, qid, created)
SELECT w.entity_id,
       CASE WHEN row_number() OVER (PARTITION BY w.tmdb_id ORDER BY w.imported_at, w.qid) = 1
            THEN w.tmdb_id END,
       w.qid, w.created
FROM wikidata_import.movies w
ON CONFLICT DO NOTHING;
INSERT INTO catalog_import.people (person_id, qid, created)
SELECT w.person_id, w.qid,
       p.created_at >= (SELECT min(imported_at) FROM wikidata_import.movies) - interval '1 minute'
FROM wikidata_import.people w JOIN people p ON p.id = w.person_id
ON CONFLICT DO NOTHING;
"""


def to_json(value):
    if isinstance(value, date):
        return value.isoformat()
    return value


def is_ours(created: bool, written: dict, key: str, current) -> bool:
    """Значение поля записал скрипт (или это строка первого импорта без written)."""
    return created and (key not in written or written[key] == to_json(current))


def plan_fields(current: dict, written: dict, created: bool, new: dict, keys,
                clearable=()) -> tuple[dict, dict]:
    """Какие поля обновить: пустые — всегда, свои — у созданных импортом; чужие правки — никогда.
    Пустым значение заменяется только в полях `clearable` (несвободный постер)."""
    changes, written = {}, dict(written)
    for k in keys:
        cur, val = current[k], new[k]
        if to_json(cur) == to_json(val):
            if created:
                written[k] = to_json(val)
            continue
        if val is None and k not in clearable:
            continue
        if (cur is None and val is not None) or is_ours(created, written, k, cur):
            changes[k] = val
            written[k] = to_json(val)
    return changes, written


def digest(rows) -> str:
    rows = sorted(json.dumps(r, default=str) for r in rows)  # в строках бывает None
    return hashlib.sha1("\n".join(rows).encode()).hexdigest()


@dataclass
class Stats:
    films: int = 0
    created: int = 0
    updated: int = 0
    adopted: int = 0
    unchanged: int = 0
    conflicts: int = 0
    people_created: int = 0
    people_updated: int = 0
    credits_replaced: int = 0
    tags_replaced: int = 0


async def ensure_tags(conn) -> dict[str, object]:
    slugs = list(TAG_NAMES)
    await conn.execute(
        """INSERT INTO tags (slug, name, kind)
           SELECT s, n, 'genre' FROM unnest($1::text[], $2::text[]) AS u(s, n)
           ON CONFLICT (slug) DO NOTHING""", slugs, [TAG_NAMES[s] for s in slugs])
    rows = await conn.fetch("SELECT id, slug FROM tags WHERE slug = ANY($1::text[])", slugs)
    return {r["slug"]: r["id"] for r in rows}


NUMBERS = """
-- Поля TMDB (TMDB_KEYS) — не под защитой правок: vote_count — из кэша, popularity — только если
-- её нет (свежую пишет админка «Обновить popularity» из ежедневного дампа).
WITH want AS (
    SELECT e.id, e.metadata AS cur,
           (e.metadata - 'vote_count')
               || jsonb_strip_nulls(jsonb_build_object('vote_count', n.vote_count))
               || CASE WHEN e.metadata ? 'popularity' THEN '{}'::jsonb
                       ELSE jsonb_strip_nulls(jsonb_build_object('popularity', n.popularity)) END
               AS md
    FROM entity_tmdb t
    JOIN tmdb_numbers n ON n.tmdb_id = t.tmdb_id
    JOIN entities e ON e.id = t.entity_id
    WHERE e.kind = 'movie'
)
UPDATE entities e SET metadata = w.md FROM want w WHERE e.id = w.id AND w.md <> w.cur;
"""


async def update_numbers(conn, movies: Path) -> int:
    """--numbers-only: vote_count (и недостающая popularity) из кэша TMDB — одним UPDATE, а не
    проходом по каждому фильму (полное слияние ради этого — часы)."""
    rows = []
    for t in tmdb_records(movies, 0):
        pop, votes = t.get("popularity"), t.get("vote_count")
        pop = round(float(pop), 3) if isinstance(pop, (int, float)) and 0 <= pop <= 1_000_000 else None
        votes = votes if isinstance(votes, int) and votes >= 0 else None
        if pop is not None or votes is not None:
            rows.append((t["id"], pop, votes))
    log.info("numbers: %d films with TMDB popularity/vote_count", len(rows))
    async with conn.transaction():
        await conn.execute("""CREATE TEMP TABLE tmdb_numbers (
                                  tmdb_id integer PRIMARY KEY, popularity float8, vote_count integer
                              ) ON COMMIT DROP""")
        await conn.copy_records_to_table("tmdb_numbers", records=rows)
        return int((await conn.execute(NUMBERS)).split()[-1])


def suffix(tmdb_id, qid) -> str:
    return f"t{tmdb_id}" if tmdb_id else qid.lower()


async def unlinked_by_title(conn) -> dict[tuple, object]:
    """(название, год) → id готовых фильмов без связки (сиды, админка). Их единицы, поэтому они
    читаются один раз, а не полным просмотром entities в каждой пачке."""
    by_title = {}
    for r in await conn.fetch(
            """SELECT e.id, e.title, e.original_title, extract(year FROM e.release_date)::int AS year
               FROM entities e
               WHERE e.kind = 'movie' AND e.release_date IS NOT NULL
                 AND NOT EXISTS (SELECT 1 FROM catalog_import.movies m WHERE m.entity_id = e.id)"""):
        for t in (r["title"], r["original_title"]):
            if t:
                by_title.setdefault((wd.norm(t), r["year"]), r["id"])
    return by_title


async def resolve_films(conn, films: list[MFilm], by_title: dict,
                        stats: Stats) -> list[tuple[MFilm, dict]]:
    """Каждому фильму — строка связки (существующая, привязанная готовая или новая)."""
    rows = await conn.fetch(
        """SELECT entity_id, tmdb_id, qid, created, written FROM catalog_import.movies
           WHERE tmdb_id = ANY($1::int[]) OR qid = ANY($2::text[])""",
        [f.tmdb_id for f in films if f.tmdb_id], [f.qid for f in films if f.qid])
    by_t = {r["tmdb_id"]: r for r in rows if r["tmdb_id"]}
    by_q = {r["qid"]: r for r in rows if r["qid"]}
    result, unlinked = [], []
    for f in films:
        link = (f.tmdb_id and by_t.get(f.tmdb_id)) or (f.qid and by_q.get(f.qid))
        if not link:
            unlinked.append(f)
            continue
        other = (f.qid and by_q.get(f.qid)) or (f.tmdb_id and by_t.get(f.tmdb_id))
        if other and other["entity_id"] != link["entity_id"]:
            stats.conflicts += 1  # TMDB и Wikidata уже привязаны к разным строкам — не склеиваем
            log.warning("tmdb %s / %s linked to different entities", f.tmdb_id, f.qid)
        result.append((f, dict(link)))
    if not unlinked:
        return result

    # Готовые непривязанные фильмы (сиды): тот же slug (с годом или без, если год совпадает)
    # или то же название и год. Иначе — новый, slug с суффиксом источника при занятом.
    with_year = lambda f: f"{f.slug_base}-{f.year}" if f.year else f.slug_base
    fallback = lambda f: f"{with_year(f)}-{suffix(f.tmdb_id, f.qid)}"
    slugs = unique(s for f in unlinked for s in (with_year(f), f.slug_base, fallback(f)))
    by_slug = {r["slug"]: r for r in await conn.fetch(
        """SELECT e.id, e.slug, e.kind::text = 'movie' AS movie,
                  extract(year FROM e.release_date)::int AS year,
                  EXISTS (SELECT 1 FROM catalog_import.movies m WHERE m.entity_id = e.id) AS linked
           FROM entities e WHERE e.slug = ANY($1::text[])""", slugs)}
    taken_slugs, taken_ids, adopt, create = set(), set(), [], []
    for f in unlinked:
        decision = None
        for t in (f.original_title, f.title):
            eid = by_title.get((wd.norm(t), f.year)) if t and f.year else None
            if eid and eid not in taken_ids:
                decision = ("adopt", eid)
                break
        if not decision:
            for slug, adopt_only in ((with_year(f), False), (f.slug_base, True), (fallback(f), False)):
                if slug in taken_slugs:
                    continue
                r = by_slug.get(slug)
                if r is None:
                    if adopt_only:
                        continue
                    decision = ("create", slug)
                elif (r["movie"] and not r["linked"] and r["id"] not in taken_ids
                      and (not adopt_only or r["year"] == f.year)):
                    decision = ("adopt", r["id"])
                else:
                    continue
                break
        if decision is None:
            stats.conflicts += 1
            log.warning("tmdb %s / %s: no free slug for %s", f.tmdb_id, f.qid, with_year(f))
        elif decision[0] == "adopt":
            taken_ids.add(decision[1])
            adopt.append((f, decision[1]))
        else:
            taken_slugs.add(decision[1])
            create.append((f, decision[1]))

    inserted = {r["slug"]: r["id"] for r in await conn.fetch(
        """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                 cover_url, metadata, active)
           SELECT 'movie', s, t, o, d, r, c, m::jsonb, false  -- включает «Пересобрать активные»
           FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::date[], $6::text[],
                       $7::text[]) AS u(s, t, o, d, r, c, m)
           ON CONFLICT (slug) DO NOTHING
           RETURNING id, slug""",
        [s for _, s in create], [f.title for f, _ in create], [f.original_title for f, _ in create],
        [f.description for f, _ in create], [f.release_date for f, _ in create],
        [f.cover_url for f, _ in create],
        [json.dumps(f.metadata, ensure_ascii=False) for f, _ in create])}
    links = []
    for f, slug in create:
        if slug in inserted:
            written = {k: to_json(v) for k, v in f.fields().items()}
            links.append((inserted[slug], f, True, written))
            stats.created += 1
        else:
            stats.conflicts += 1  # slug заняли параллельно — следующий запуск повторит
    for f, eid in adopt:
        links.append((eid, f, False, {}))
        stats.adopted += 1
        for k in [k for k, v in by_title.items() if v == eid]:
            del by_title[k]  # привязана — больше не кандидат
    if links:
        await conn.execute(
            """INSERT INTO catalog_import.movies (entity_id, tmdb_id, qid, created, written)
               SELECT * FROM unnest($1::uuid[], $2::int[], $3::text[], $4::bool[], $5::jsonb[])""",
            [l[0] for l in links], [l[1].tmdb_id for l in links], [l[1].qid for l in links],
            [l[2] for l in links], [json.dumps(l[3], ensure_ascii=False) for l in links])
    for eid, f, created, written in links:
        result.append((f, {"entity_id": eid, "tmdb_id": f.tmdb_id, "qid": f.qid,
                           "created": created, "written": written, "new": created}))
    return result


async def update_films(conn, pairs: list[tuple[MFilm, dict]], stats: Stats):
    """Поля существующих строк по правилам plan_fields; связке дописываются недостающие id."""
    existing = [(f, l) for f, l in pairs if not l.get("new")]
    if not existing:
        return
    rows = {r["id"]: r for r in await conn.fetch(
        f"SELECT id, {', '.join(MOVIE_FIELDS)} FROM entities WHERE id = ANY($1::uuid[])",
        [l["entity_id"] for _, l in existing])}
    updates, link_updates = [], []
    for f, l in existing:
        r = rows.get(l["entity_id"])
        if r is None:
            continue
        current = {k: r[k] for k in MOVIE_FIELDS}
        current["metadata"] = json.loads(current["metadata"]) if isinstance(current["metadata"], str) else current["metadata"]
        clear = ("cover_url",) if current["cover_url"] and not free_image(current["cover_url"]) \
            and "image.tmdb.org" not in current["cover_url"] else ()
        # metadata сравнивается без полей TMDB; они дописываются после: vote_count — из TMDB,
        # popularity — текущая (её обновляет админка), иначе из TMDB.
        own_cur, tmdb_cur = split_tmdb(current["metadata"])
        new = f.fields()
        own_new, tmdb_new = split_tmdb(new["metadata"])
        old_written = dict(l["written"])
        if "metadata" in old_written:
            old_written["metadata"] = split_tmdb(old_written["metadata"])[0]
        changes, written = plan_fields({**current, "metadata": own_cur}, old_written, l["created"],
                                       {**new, "metadata": own_new}, MOVIE_FIELDS, clear)
        tmdb = {**tmdb_new, **({"popularity": tmdb_cur["popularity"]} if "popularity" in tmdb_cur else {})}
        metadata = {**changes.get("metadata", own_cur), **tmdb}
        if metadata != current["metadata"]:
            changes["metadata"] = metadata
        else:
            changes.pop("metadata", None)
        l["written"] = written
        if changes:
            updates.append((l["entity_id"], {**current, **changes}))
            stats.updated += 1
        else:
            stats.unchanged += 1
        link_updates.append((l["entity_id"], f.tmdb_id if not l.get("tmdb_id") else l["tmdb_id"],
                             f.qid if not l.get("qid") else l["qid"], written))
    if updates:
        v = [u[1] for u in updates]
        await conn.execute(
            """UPDATE entities e
               SET title = u.t, original_title = u.o, description = u.d, release_date = u.r,
                   cover_url = u.c, metadata = u.m::jsonb
               FROM unnest($1::uuid[], $2::text[], $3::text[], $4::text[], $5::date[], $6::text[],
                           $7::text[]) AS u(id, t, o, d, r, c, m)
               WHERE e.id = u.id""",
            [u[0] for u in updates], [x["title"] for x in v], [x["original_title"] for x in v],
            [x["description"] for x in v], [x["release_date"] for x in v], [x["cover_url"] for x in v],
            [json.dumps(x["metadata"], ensure_ascii=False) for x in v])
    await save_links(conn, "movies", "entity_id", link_updates)


async def save_links(conn, table: str, key: str, rows: list[tuple]):
    """written и недостающие tmdb_id/qid связок — одним запросом. id, уже закреплённый за другой
    строкой (или за двумя в одной пачке), не дописывается: UNIQUE не нарушается."""
    if not rows:
        return
    taken_t = {r[0] for r in await conn.fetch(
        f"SELECT tmdb_id FROM catalog_import.{table} WHERE tmdb_id = ANY($1::int[])",
        [r[1] for r in rows if r[1]])}
    taken_q = {r[0] for r in await conn.fetch(
        f"SELECT qid FROM catalog_import.{table} WHERE qid = ANY($1::text[])",
        [r[2] for r in rows if r[2]])}
    ids, tmdb, qids, written = [], [], [], []
    for rid, t, q, w in rows:
        ids.append(rid)
        tmdb.append(t if t and t not in taken_t else None)
        qids.append(q if q and q not in taken_q else None)
        taken_t.add(t)
        taken_q.add(q)
        written.append(json.dumps(w, ensure_ascii=False))
    await conn.execute(
        f"""UPDATE catalog_import.{table} m
            SET written = u.w::jsonb, tmdb_id = COALESCE(m.tmdb_id, u.t), qid = COALESCE(m.qid, u.q)
            FROM unnest($1::uuid[], $2::int[], $3::text[], $4::text[]) AS u(id, t, q, w)
            WHERE m.{key} = u.id""", ids, tmdb, qids, written)


async def resolve_people(conn, persons: list[MPerson], done: set, stats: Stats) -> dict:
    """MPerson.key → person_id: по связке, по slug среди непривязанных (сиды), иначе новый."""
    rows = await conn.fetch(
        """SELECT person_id, tmdb_id, qid, created, written FROM catalog_import.people
           WHERE tmdb_id = ANY($1::int[]) OR qid = ANY($2::text[])""",
        [p.tmdb_id for p in persons if p.tmdb_id], [p.qid for p in persons if p.qid])
    by_t = {r["tmdb_id"]: r for r in rows if r["tmdb_id"]}
    by_q = {r["qid"]: r for r in rows if r["qid"]}
    ids, linked, fresh = {}, [], []
    for p in persons:
        link = (p.tmdb_id and by_t.get(p.tmdb_id)) or (p.qid and by_q.get(p.qid))
        if link:
            ids[p.key] = link["person_id"]
            if link["person_id"] not in done:  # одна запись о человеке за прогон
                done.add(link["person_id"])
                linked.append((p, link))
        else:
            fresh.append(p)

    # Обновление связанных: те же правила полей; недостающие id — в связку.
    if linked:
        cur = {r["id"]: r for r in await conn.fetch(
            f"SELECT id, {', '.join(PERSON_FIELDS)} FROM people WHERE id = ANY($1::uuid[])",
            [l["person_id"] for _, l in linked])}
        upd, links = [], []
        for p, l in linked:
            r = cur.get(l["person_id"])
            if r is None:
                continue
            written = json.loads(l["written"]) if isinstance(l["written"], str) else l["written"]
            current = {k: r[k] for k in PERSON_FIELDS}
            changes, written = plan_fields(current, written, l["created"], p.fields(), PERSON_FIELDS)
            if changes:
                upd.append((l["person_id"], {**current, **changes}))
            missing = (p.tmdb_id and not l["tmdb_id"]) or (p.qid and not l["qid"])
            if changes or missing or written != l["written"]:
                links.append((l["person_id"], p.tmdb_id, p.qid, written))
        if upd:
            v = [u[1] for u in upd]
            await conn.execute(
                """UPDATE people x SET full_name = u.n, birth_date = u.b, photo_url = u.p, bio = u.o
                   FROM unnest($1::uuid[], $2::text[], $3::date[], $4::text[], $5::text[])
                        AS u(id, n, b, p, o)
                   WHERE x.id = u.id""",
                [u[0] for u in upd], [x["full_name"] for x in v], [x["birth_date"] for x in v],
                [x["photo_url"] for x in v], [x["bio"] for x in v])
            stats.people_updated += len(upd)
        await save_links(conn, "people", "person_id", links)
    if not fresh:
        return ids

    # Новые: известные первыми — им slug без суффикса; готовый непривязанный slug — привязка.
    fresh.sort(key=lambda p: (-p.fame, p.tmdb_id or 1 << 31, p.qid or ""))
    fallback = lambda p: f"{p.slug_base}-{suffix(p.tmdb_id, p.qid)}"
    existing = {r["slug"]: r for r in await conn.fetch(
        """SELECT p.id, p.slug,
                  EXISTS (SELECT 1 FROM catalog_import.people m WHERE m.person_id = p.id) AS linked
           FROM people p WHERE p.slug = ANY($1::text[])""",
        unique(s for p in fresh for s in (p.slug_base, fallback(p))))}
    taken, adopted_ids, create, adopt = set(), set(), [], []
    for p in fresh:
        for slug in (p.slug_base, fallback(p)):
            if slug in taken:
                continue
            r = existing.get(slug)
            if r is None:
                taken.add(slug)
                create.append((p, slug))
                break
            if not r["linked"] and r["id"] not in adopted_ids:
                adopted_ids.add(r["id"])
                adopt.append((p, r["id"]))
                break
    inserted = {r["slug"]: r["id"] for r in await conn.fetch(
        """INSERT INTO people (slug, full_name, birth_date, photo_url, bio)
           SELECT * FROM unnest($1::text[], $2::text[], $3::date[], $4::text[], $5::text[])
           ON CONFLICT (slug) DO NOTHING
           RETURNING id, slug""",
        [s for _, s in create], [p.full_name for p, _ in create], [p.birth_date for p, _ in create],
        [p.photo_url for p, _ in create], [p.bio for p, _ in create])}
    links = []
    for p, slug in create:
        if slug in inserted:
            links.append((inserted[slug], p, True,
                          {k: to_json(v) for k, v in p.fields().items()}))
    links += [(pid, p, False, {}) for p, pid in adopt]
    stats.people_created += len(inserted)
    if links:
        await conn.execute(
            """INSERT INTO catalog_import.people (person_id, tmdb_id, qid, created, written)
               SELECT * FROM unnest($1::uuid[], $2::int[], $3::text[], $4::bool[], $5::jsonb[])
               ON CONFLICT DO NOTHING""",
            [l[0] for l in links], [l[1].tmdb_id for l in links], [l[1].qid for l in links],
            [l[2] for l in links], [json.dumps(l[3], ensure_ascii=False) for l in links])
        for pid, p, _, _ in links:
            ids[p.key] = pid
            done.add(pid)
    return ids


async def replace_credits_and_tags(conn, pairs, person_ids: dict, tags: dict, stats: Stats):
    """Титры и жанры — только у созданных импортом и только если их не правили вручную."""
    own = [(f, l) for f, l in pairs if l["created"]]
    if not own:
        return
    eids = [l["entity_id"] for _, l in own]
    cur_credits, cur_tags = {}, {}
    for r in await conn.fetch(
            """SELECT entity_id, person_id, role, character_name, position FROM entity_credits
               WHERE entity_id = ANY($1::uuid[])""", eids):
        cur_credits.setdefault(r["entity_id"], []).append(
            (str(r["person_id"]), r["role"], r["character_name"], r["position"]))
    for r in await conn.fetch(
            "SELECT entity_id, tag_id FROM entity_tags WHERE entity_id = ANY($1::uuid[])", eids):
        cur_tags.setdefault(r["entity_id"], []).append(str(r["tag_id"]))

    credit_rows, credit_eids, tag_rows, tag_eids, written_updates = [], [], [], [], []
    for f, l in own:
        eid, written = l["entity_id"], dict(l["written"])
        want_c = [(str(person_ids[c.person.key]), c.role, c.character_name, c.position)
                  for c in f.credits if c.person.key in person_ids]
        want_t = [str(tags[t]) for t in f.tags if t in tags]
        have_c, have_t = cur_credits.get(eid, []), cur_tags.get(eid, [])
        changed = False
        if digest(want_c) != digest(have_c) and ("credits" not in written
                                                 or written["credits"] == digest(have_c)):
            credit_eids.append(eid)
            credit_rows += [(eid, *c) for c in want_c]
            written["credits"] = digest(want_c)
            changed = True
        elif "credits" not in written and digest(want_c) == digest(have_c):
            written["credits"] = digest(have_c)
            changed = True
        if digest(want_t) != digest(have_t) and ("tags" not in written
                                                 or written["tags"] == digest(have_t)):
            tag_eids.append(eid)
            tag_rows += [(eid, t) for t in want_t]
            written["tags"] = digest(want_t)
            changed = True
        elif "tags" not in written and digest(want_t) == digest(have_t):
            written["tags"] = digest(have_t)
            changed = True
        if changed:
            written_updates.append((eid, written))
    if credit_eids:
        await conn.execute("DELETE FROM entity_credits WHERE entity_id = ANY($1::uuid[])", credit_eids)
        if credit_rows:
            await conn.execute(
                """INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::text[], $5::int[])
                   ON CONFLICT DO NOTHING""",
                [r[0] for r in credit_rows], [r[1] for r in credit_rows],
                [r[2] for r in credit_rows], [r[3] for r in credit_rows], [r[4] for r in credit_rows])
        stats.credits_replaced += len(credit_eids)
    if tag_eids:
        await conn.execute("DELETE FROM entity_tags WHERE entity_id = ANY($1::uuid[])", tag_eids)
        if tag_rows:
            await conn.execute(
                """INSERT INTO entity_tags (entity_id, tag_id)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[]) ON CONFLICT DO NOTHING""",
                [r[0] for r in tag_rows], [r[1] for r in tag_rows])
        stats.tags_replaced += len(tag_eids)
    if written_updates:
        await conn.execute(
            """UPDATE catalog_import.movies m SET written = u.w::jsonb
               FROM unnest($1::uuid[], $2::text[]) AS u(id, w) WHERE m.entity_id = u.id""",
            [w[0] for w in written_updates],
            [json.dumps(w[1], ensure_ascii=False) for w in written_updates])


async def merge_batch(conn, films: list[MFilm], tags: dict, by_title: dict, done: set,
                      done_people: set, stats: Stats):
    async with conn.transaction():
        pairs = []
        for f, l in await resolve_films(conn, films, by_title, stats):
            if l["entity_id"] in done:  # уже слита в этом прогоне (TMDB и запись Wikidata)
                stats.conflicts += 1
                continue
            done.add(l["entity_id"])
            pairs.append((f, l))
        for _, l in pairs:
            if isinstance(l["written"], str):
                l["written"] = json.loads(l["written"])
        await update_films(conn, pairs, stats)
        # Связь с TMDB — в таблице каталога (её читают операции админки); готовые строки,
        # привязанные импортом (сиды, админка), — «ручные»: «Пересобрать активные» их не трогает.
        tmdb_links = [(l["entity_id"], f.tmdb_id) for f, l in pairs if f.tmdb_id]
        if tmdb_links:
            await conn.execute(
                """INSERT INTO entity_tmdb (entity_id, tmdb_id)
                   SELECT * FROM unnest($1::uuid[], $2::int[]) ON CONFLICT DO NOTHING""",
                [x[0] for x in tmdb_links], [x[1] for x in tmdb_links])
        adopted = [l["entity_id"] for _, l in pairs if not l["created"]]
        if adopted:
            await conn.execute(
                "UPDATE entities SET active_manual = true WHERE id = ANY($1::uuid[]) AND NOT active_manual",
                adopted)
        persons = {}
        for f, l in pairs:
            if l["created"]:
                for c in f.credits:
                    persons.setdefault(c.person.key, c.person)
        person_ids = (await resolve_people(conn, list(persons.values()), done_people, stats)
                      if persons else {})
        await replace_credits_and_tags(conn, pairs, person_ids, tags, stats)
        stats.films += len(films)


# ---------------------------------------------------------------- main

def tmdb_records(path: Path, limit: int):
    seen = set()
    with path.open(encoding="utf-8") as f:
        for line in f:
            try:
                r = json.loads(line)
            except json.JSONDecodeError:  # строка, оборванная при остановке загрузки
                continue
            if r["id"] in seen:
                continue
            seen.add(r["id"])
            yield r
            if limit and len(seen) >= limit:
                return


async def run(args):
    dsn = env("DATABASE_URL")
    if not dsn:
        sys.exit("DATABASE_URL is required")
    if args.numbers_only:
        conn = await asyncpg.connect(dsn)
        try:
            started = time.monotonic()
            updated = await update_numbers(conn, Path(args.tmdb_cache) / "movies.jsonl")
            log.info("numbers: metadata of %d films updated (%.0fs)", updated,
                     time.monotonic() - started)
        finally:
            await conn.close()
        log.info(NEXT)
        return
    wd_cache = Path(args.wikidata_cache)

    class WdArgs:  # import_wikidata.download читает кэш, сеть — только если файлов нет
        limit, min_links, endpoint, cache_dir, refresh = args.wikidata_limit, 2, wd.QLEVER, str(wd_cache), False
    async with aiohttp.ClientSession(headers={"User-Agent": wd.USER_AGENT}) as session:
        top, threshold = await wd.download(WdArgs, session)
    data = wd.Dataset(wd_cache, threshold, top, wd.wiki_cache(str(wd_cache)))
    merger = Merger(data)
    log.info("wikidata: %d films, %d people (%d with TMDB id)", len(data.basics),
             len(data.people), len(merger.person_qid))

    conn = await asyncpg.connect(dsn)
    try:
        await conn.execute(STATE_SCHEMA)
        if (await conn.fetchval("SELECT to_regclass('wikidata_import.movies') IS NOT NULL")
                and not await conn.fetchval("SELECT EXISTS (SELECT 1 FROM catalog_import.movies)")):
            await conn.execute(MIGRATE_WIKIDATA)
            log.info("migrated links of the Wikidata import: %d films, %d people",
                     await conn.fetchval("SELECT count(*) FROM catalog_import.movies"),
                     await conn.fetchval("SELECT count(*) FROM catalog_import.people"))
        tags = await ensure_tags(conn)
        by_title = await unlinked_by_title(conn)
        log.info("unlinked films to match by title + year: %d", len(by_title))
        stats, started, used_qids = Stats(), time.monotonic(), set()
        timing = {"db": 0.0, "build": 0.0, "mark": time.monotonic()}
        done_entities: set = set()
        done_people: set = set()

        async def flush(batch):
            now = time.monotonic()
            timing["build"] += now - timing["mark"]
            await merge_batch(conn, batch, tags, by_title, done_entities, done_people, stats)
            timing["mark"] = time.monotonic()
            timing["db"] += timing["mark"] - now
            if stats.films % (args.batch_size * 20) < len(batch):
                log.info("merge: %d films (%.0f/s; python %.0fs, db %.0fs) | created %d, "
                         "updated %d, unchanged %d, "
                         "adopted %d, conflicts %d | people +%d ~%d | credits %d, tags %d",
                         stats.films, stats.films / (time.monotonic() - started),
                         timing["build"], timing["db"], stats.created,
                         stats.updated, stats.unchanged, stats.adopted, stats.conflicts,
                         stats.people_created, stats.people_updated, stats.credits_replaced,
                         stats.tags_replaced)

        # 1. Все фильмы TMDB (с данными Wikidata, если есть пара), 2. фильмы только из Wikidata.
        batch, skipped = [], 0
        movies = Path(args.tmdb_cache) / "movies.jsonl"
        if movies.exists():
            merger.learn_tmdb_people(tmdb_records(movies, args.tmdb_limit))
            log.info("tmdb: %d people linked to Wikidata", len(merger.tmdb_people))
        for t in (tmdb_records(movies, args.tmdb_limit) if movies.exists() else []):
            q = merger.film_qid.get(t["id"])
            if q:
                used_qids.add(q)
            if film := merger.film(t, q if q in data.basics else None):
                batch.append(film)
            else:
                skipped += 1
            if len(batch) >= args.batch_size:
                await flush(batch)
                batch = []
        for q in top:
            if q in used_qids:
                continue
            if film := merger.film(None, q):
                batch.append(film)
            else:
                skipped += 1
            if len(batch) >= args.batch_size:
                await flush(batch)
                batch = []
        if batch:
            await flush(batch)
    finally:
        await conn.close()
    log.info("done: %d films (no title: %d) | created %d, updated %d, unchanged %d, adopted %d, "
             "conflicts %d | people +%d ~%d | credits %d, tags %d", stats.films, skipped,
             stats.created, stats.updated, stats.unchanged, stats.adopted, stats.conflicts,
             stats.people_created, stats.people_updated, stats.credits_replaced, stats.tags_replaced)
    log.info(NEXT)


NEXT = ("next: admin → «Пересобрать активные» (POST /api/v1/catalog/admin/tmdb/active/stream): "
        "new films are hidden until then; it also rebuilds search")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    p.add_argument("--tmdb-cache", default=str(CACHE / "tmdb"))
    p.add_argument("--wikidata-cache", default=str(CACHE / "wikidata"))
    p.add_argument("--wikidata-limit", type=int, default=100_000,
                   help="must match the limit the Wikidata cache was downloaded with")
    p.add_argument("--tmdb-limit", type=int, default=0, help="only the first N cached TMDB films")
    p.add_argument("--batch-size", type=int, default=500, help="films per DB transaction")
    p.add_argument("--numbers-only", action="store_true",
                   help="only refresh vote_count (and missing popularity) from the TMDB cache, "
                        "one UPDATE (e.g. after a fresh fetch_tmdb.py)")
    p.add_argument("-v", "--verbose", action="store_true")
    args = p.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(asctime)s %(levelname)s %(message)s", datefmt="%H:%M:%S")
    asyncio.run(run(args))


if __name__ == "__main__":
    main()
