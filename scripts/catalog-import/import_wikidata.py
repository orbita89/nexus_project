#!/usr/bin/env python3
"""Импорт фильмов из Wikidata (+ описания и постеры из Википедии) в каталог Nexus, без ключей API.

Этапы:
  1. download — несколько больших SPARQL-запросов к QLever (публичный движок по полной Wikidata,
     отдаёт сотни тысяч строк за секунды; официальный query.wikidata.org режет запросы на 60 с).
     Фильмы: P31 ∈ {фильм, полнометражный, анимационный, полнометражный мультфильм}, топ --limit по
     числу статей в Википедиях (sitelinks — замена популярности). Ответы — CSV в --cache-dir.
  2. wiki — MediaWiki API ru.wikipedia: вступление статьи (описание) и картинка статьи (постер),
     постер без русской статьи — из en.wikipedia. Кэш — wiki.jsonl, докачивается после обрыва.
  3. load — пачки по --batch-size фильмов, каждая пачка — одна транзакция, unnest + ON CONFLICT DO
     NOTHING; id — DEFAULT uuidv7() базы. Соответствие QID → id Nexus — служебная схема
     `wikidata_import`, поэтому перезапуск ничего не дублирует.

Что берём: название (ru, иначе en), оригинальное (P1476), дата выхода (P577, только с точностью до
дня; год — для slug), длительность (P2047), страны (P495 → P297), студии (P272), жанры (P136 и P31 →
теги-жанры по таблице GENRES), режиссёры (P57), первые 15 актёров (P161 и озвучивание P725: порядок P1545, иначе
известность актёра; персонаж — P453/P4633), у людей — имя, дата рождения (P569), фото (P18, Commons),
краткое описание (ru) как bio. id TMDB (P4947) сохраняется в wikidata_import.movies.

Существующие фильмы (сиды, админка) не перезаписываются: совпадение по slug или по «название +
год» — привязка без изменений. Люди — так же по slug имени.

После загрузки: scripts/reindex-search.sh (Meilisearch о новых строках не знает).
Вместе с TMDB: этот скрипт — только этапы download и wiki, в БД пишет merge.py (этап load
отказывается работать, если есть схема catalog_import).
Запуск: WIKIMEDIA_CONTACT=<почта или URL> DATABASE_URL=postgres://... \
        python3 import_wikidata.py --wiki-concurrency 4 --wiki-rate 4 [--help]
"""

import argparse
import asyncio
import csv
import json
import logging
import os
import re
import sys
import time
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path
from urllib.parse import unquote

import aiohttp
import asyncpg

from import_tmdb import (MAX_RUNTIME, MAX_STUDIO, MAX_STUDIOS, RateLimiter, clean, is_latin,
                         slugify, unique)

QLEVER = "https://qlever.cs.uni-freiburg.de/api/wikidata"
# Wikimedia требует контакт в User-Agent; без него API Википедии быстро отвечает 429.
USER_AGENT = "nexus-catalog-import/1.0 (Nexus dev data import{}) python-aiohttp".format(
    f"; {c}" if (c := os.environ.get("WIKIMEDIA_CONTACT")) else "")
ENTITY = "http://www.wikidata.org/entity/"
MAX_ACTORS = 15
MAX_TITLE = 300          # validate.rs
MAX_LONG_TEXT = 20_000   # validate.rs
POSTER_WIDTH = 780
PHOTO_WIDTH = 400

# P31/P136 → slug тегов-жанров. slug совпадают с сидами (sci-fi, detective, animation, ...).
GENRES = {
    "Q130232": ["drama"], "Q157443": ["comedy"], "Q188473": ["action"], "Q959790": ["crime"],
    "Q1054574": ["romance"], "Q1919632": ["romance"], "Q2484376": ["thriller"],
    "Q200092": ["horror"], "Q853630": ["horror"], "Q93204": ["documentary"],
    "Q319221": ["adventure"], "Q859369": ["comedy", "drama"], "Q842256": ["musical"],
    "Q471839": ["sci-fi"], "Q860626": ["romance", "comedy"], "Q118612349": ["romance", "comedy"],
    "Q157394": ["fantasy"], "Q645928": ["biography"], "Q369747": ["war"], "Q172980": ["western"],
    "Q1200678": ["detective"], "Q17013749": ["history"], "Q2143665": ["family"],
    "Q1361932": ["family"], "Q224700": ["comedy", "horror"], "Q19367312": ["crime", "thriller"],
    "Q1033891": ["action"], "Q1339864": ["sport"], "Q202866": ["animation"],
    "Q29168811": ["animation"], "Q20650540": ["animation"], "Q1107": ["animation"],
}
# Названия новых тегов; уже существующие (сиды, админка) не меняются.
GENRE_NAMES = {
    "drama": "Драма", "comedy": "Комедия", "action": "Боевик", "crime": "Криминал",
    "romance": "Мелодрама", "thriller": "Триллер", "horror": "Ужасы",
    "documentary": "Документальный", "adventure": "Приключения", "musical": "Мюзикл",
    "sci-fi": "Научная фантастика", "fantasy": "Фэнтези", "biography": "Биография",
    "war": "Военный", "western": "Вестерн", "detective": "Детектив", "history": "История",
    "family": "Семейный", "sport": "Спорт", "animation": "Анимация",
}
FILM_TYPES = "wd:Q11424 wd:Q24869 wd:Q202866 wd:Q29168811"
UNITS = {"Q7727": 1.0, "Q25235": 60.0, "Q11574": 1 / 60}  # минута, час, секунда → минуты
PRECISION_DAY, PRECISION_YEAR = 11, 9

log = logging.getLogger("wikidata-import")


# ---------------------------------------------------------------- download (QLever)

PREFIXES = """PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX p: <http://www.wikidata.org/prop/>
PREFIX ps: <http://www.wikidata.org/prop/statement/>
PREFIX psv: <http://www.wikidata.org/prop/statement/value/>
PREFIX pq: <http://www.wikidata.org/prop/qualifier/>
PREFIX wikibase: <http://wikiba.se/ontology#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX schema: <http://schema.org/>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
"""


def films_filter(min_links: int) -> str:
    # QLever хранит sitelinks как xsd:int и сравнивает его только после явного приведения.
    return (f"VALUES ?t {{ {FILM_TYPES} }} ?f wdt:P31 ?t . ?f wikibase:sitelinks ?l . "
            f"FILTER(xsd:integer(STR(?l)) >= {min_links})")


def label(var: str, subject: str, lang: str) -> str:
    return f'OPTIONAL {{ {subject} rdfs:label ?{var} FILTER(LANG(?{var}) = "{lang}") }}'


def queries(min_links: int) -> dict[str, str]:
    films = films_filter(min_links)
    return {
        "basics": f"""SELECT ?f ?ru ?en ?orig ?date ?prec ?dur ?unit ?tmdb ?ruwiki ?enwiki WHERE {{
  {films}
  {label("ru", "?f", "ru")} {label("en", "?f", "en")}
  OPTIONAL {{ ?f wdt:P1476 ?orig }}
  OPTIONAL {{ ?f p:P577 ?ds . ?ds psv:P577 ?dv . ?dv wikibase:timeValue ?date .
             ?dv wikibase:timePrecision ?prec }}
  OPTIONAL {{ ?f p:P2047 ?qs . ?qs psv:P2047 ?qv . ?qv wikibase:quantityAmount ?dur .
             ?qv wikibase:quantityUnit ?unit }}
  OPTIONAL {{ ?f wdt:P4947 ?tmdb }}
  OPTIONAL {{ ?ruwiki schema:about ?f . ?ruwiki schema:isPartOf <https://ru.wikipedia.org/> }}
  OPTIONAL {{ ?enwiki schema:about ?f . ?enwiki schema:isPartOf <https://en.wikipedia.org/> }}
}}""",
        "credits": f"""SELECT ?f ?role ?p ?ord ?char ?charName WHERE {{
  {films}
  {{ ?f p:P57 ?st . ?st ps:P57 ?p . BIND("director" AS ?role) }} UNION
  {{ ?f p:P161 ?st . ?st ps:P161 ?p . BIND("actor" AS ?role)
     OPTIONAL {{ ?st pq:P1545 ?ord }} OPTIONAL {{ ?st pq:P4633 ?charName }}
     OPTIONAL {{ ?st pq:P453 ?char }} }} UNION
  {{ ?f p:P725 ?st . ?st ps:P725 ?p . BIND("actor" AS ?role)
     OPTIONAL {{ ?st pq:P1545 ?ord }} OPTIONAL {{ ?st pq:P4633 ?charName }}
     OPTIONAL {{ ?st pq:P453 ?char }} }}
}}""",
        "people": f"""SELECT DISTINCT ?p ?ru ?en ?mul ?links ?desc ?birth ?bprec ?img ?tmdb WHERE {{
  {films}
  {{ ?f wdt:P57 ?p }} UNION {{ ?f wdt:P161 ?p }} UNION {{ ?f wdt:P725 ?p }}
  OPTIONAL {{ ?p wikibase:sitelinks ?links }}
  {label("ru", "?p", "ru")} {label("en", "?p", "en")} {label("mul", "?p", "mul")}
  OPTIONAL {{ ?p schema:description ?desc FILTER(LANG(?desc) = "ru") }}
  OPTIONAL {{ ?p p:P569 ?bs . ?bs psv:P569 ?bv . ?bv wikibase:timeValue ?birth .
             ?bv wikibase:timePrecision ?bprec }}
  OPTIONAL {{ ?p wdt:P18 ?img }}
  OPTIONAL {{ ?p wdt:P4985 ?tmdb }}
}}""",
        "characters": f"""SELECT DISTINCT ?c ?ru ?en WHERE {{
  {films}
  {{ ?f p:P161 ?st }} UNION {{ ?f p:P725 ?st }} ?st pq:P453 ?c .
  {label("ru", "?c", "ru")} {label("en", "?c", "en")}
}}""",
        "facets": f"""SELECT ?f ?kind ?x ?ru ?en ?mul ?iso WHERE {{
  {films}
  {{ ?f wdt:P136 ?x BIND("genre" AS ?kind) }} UNION {{ ?f wdt:P31 ?x BIND("type" AS ?kind) }} UNION
  {{ ?f wdt:P272 ?x BIND("studio" AS ?kind) {label("ru", "?x", "ru")} {label("en", "?x", "en")}
     {label("mul", "?x", "mul")} }}
  UNION {{ ?f wdt:P495 ?x BIND("country" AS ?kind) OPTIONAL {{ ?x wdt:P297 ?iso }} }}
}}""",
    }


async def sparql_to_csv(session: aiohttp.ClientSession, endpoint: str, query: str, path: Path,
                        retries: int = 4):
    """Поток CSV в файл; .part → rename, так что в кэше либо полный ответ, либо ничего."""
    tmp = path.with_suffix(".part")
    for attempt in range(retries + 1):
        started = time.monotonic()
        try:
            async with session.post(endpoint, data={"query": PREFIXES + query},
                                    headers={"Accept": "text/csv"}) as resp:
                if resp.status != 200:
                    body = (await resp.text())[:500]
                    if resp.status < 500 and resp.status != 429:
                        raise RuntimeError(f"{path.name}: HTTP {resp.status}: {body}")
                    raise aiohttp.ClientResponseError(resp.request_info, (), status=resp.status,
                                                      message=body)
                rows = -1
                with tmp.open("wb") as f:
                    async for line in resp.content:
                        f.write(line)
                        rows += 1
            tmp.rename(path)
            log.info("download: %s — %d rows, %.1f MB, %.0fs", path.name, rows,
                     path.stat().st_size / 1e6, time.monotonic() - started)
            return
        except (aiohttp.ClientError, asyncio.TimeoutError) as e:
            delay = 10 * 2 ** attempt
            log.warning("download: %s failed (%r), retry in %ds", path.name, e, delay)
            await asyncio.sleep(delay)
    raise RuntimeError(f"{path.name}: gave up after {retries} retries")


def read_csv(path: Path):
    with path.open(newline="", encoding="utf-8") as f:
        yield from csv.DictReader(f)


def qid(iri: str) -> str:
    return iri[len(ENTITY):] if iri.startswith(ENTITY) else ""


async def download(args, session: aiohttp.ClientSession) -> tuple[list[str], int]:
    """Топ --limit QID по sitelinks и порог sitelinks, под который скачаны остальные файлы."""
    cache = Path(args.cache_dir)
    cache.mkdir(parents=True, exist_ok=True)
    films_csv = cache / f"films_l{args.min_links}.csv"
    if args.refresh or not films_csv.exists():
        await sparql_to_csv(session, args.endpoint,
                            f"SELECT ?f ?l WHERE {{ {films_filter(args.min_links)} }}", films_csv)
    links: dict[str, int] = {}
    for row in read_csv(films_csv):
        if q := qid(row["f"]):
            links[q] = max(links.get(q, 0), int(row["l"]))
    # Порядок детерминированный: известность, затем более старый (меньший) QID.
    top = sorted(links, key=lambda q: (-links[q], int(q[1:])))[:args.limit]
    threshold = links[top[-1]] if top else args.min_links
    log.info("films: %d with ≥%d sitelinks, selected %d (≥%d sitelinks)",
             len(links), args.min_links, len(top), threshold)
    for name, query in queries(threshold).items():
        path = cache / f"{name}_l{threshold}.csv"
        if args.refresh or not path.exists():
            await sparql_to_csv(session, args.endpoint, query, path)
    return top, threshold


# ---------------------------------------------------------------- wiki (MediaWiki API)

def wiki_title(url: str) -> str:
    return unquote(url.split("/wiki/", 1)[1]).replace("_", " ")


async def mediawiki(session, limiter: RateLimiter, lang: str, params: dict, retries: int = 8) -> dict:
    reason = ""
    for attempt in range(retries + 1):
        await limiter.wait()
        delay = min(60, 2 ** attempt)
        try:
            async with session.get(f"https://{lang}.wikipedia.org/w/api.php", params={
                    "action": "query", "format": "json", "formatversion": "2", "redirects": "1",
                    "maxlag": "5", **params}) as resp:
                if resp.status == 200:
                    data = await resp.json()
                    if data.get("error", {}).get("code") != "maxlag":
                        return data
                    reason = "maxlag"
                elif resp.status < 500 and resp.status != 429:
                    raise RuntimeError(f"{lang}.wikipedia: HTTP {resp.status}")
                else:
                    reason = f"HTTP {resp.status}"
                    delay = max(delay, float(resp.headers.get("Retry-After") or 0))
        except (aiohttp.ClientError, asyncio.TimeoutError) as e:
            reason = repr(e)
        log.warning("%s.wikipedia: %s, retry in %.0fs", lang, reason, delay)
        await asyncio.sleep(delay)
    raise RuntimeError(f"{lang}.wikipedia: gave up after {retries} retries ({reason})")


def resolve_pages(data: dict, titles: list[str]) -> dict[str, dict]:
    """Запрошенный заголовок → страница с учётом нормализации и редиректов."""
    alias = {t: t for t in titles}
    for key in ("normalized", "redirects"):
        for m in data.get("query", {}).get(key, []):
            for orig, target in alias.items():
                if target == m["from"]:
                    alias[orig] = m["to"]
    pages = {p["title"]: p for p in data.get("query", {}).get("pages", []) if not p.get("missing")}
    return {orig: pages[t] for orig, t in alias.items() if t in pages}


def wiki_cache(cache_dir: str) -> dict[str, dict]:
    done: dict[str, dict] = {}
    cache = Path(cache_dir) / "wiki.jsonl"
    if cache.exists():
        for line in cache.open(encoding="utf-8"):
            try:
                row = json.loads(line)
            except json.JSONDecodeError:  # строка, оборванная при остановке
                continue
            done.setdefault(row["qid"], {}).update(row["data"])
    return done


async def wiki(args, session, top: list[str], threshold: int) -> dict[str, dict]:
    """qid → {"extract", "poster_ru", "poster_en"}. Кэш wiki.jsonl дописывается после каждого
    запроса, так что остановленный этап продолжается с места остановки."""
    cache = Path(args.cache_dir) / "wiki.jsonl"
    done = wiki_cache(args.cache_dir)
    selected = set(top)
    ru, en = {}, {}
    for row in read_csv(Path(args.cache_dir) / f"basics_l{threshold}.csv"):
        q = qid(row["f"])
        if q in selected:
            if row["ruwiki"]:
                ru.setdefault(q, wiki_title(row["ruwiki"]))
            if row["enwiki"]:
                en.setdefault(q, wiki_title(row["enwiki"]))
    out = cache.open("a", encoding="utf-8")
    sem = asyncio.Semaphore(args.wiki_concurrency)
    limiter = RateLimiter(args.wiki_rate)
    progress = {"n": 0}

    async def batch(lang, items, params, extract):
        async with sem:
            titles = [t for _, t in items]
            data = await mediawiki(session, limiter, lang, {**params, "titles": "|".join(titles)})
        pages = resolve_pages(data, titles)
        for q, t in items:
            page = pages.get(t, {})
            row = {"done_" + lang: True}
            if extract and (text := clean(page.get("extract"))):
                row["extract"] = text[:MAX_LONG_TEXT]
            if src := page.get("thumbnail", {}).get("source"):
                row["poster_" + lang] = src.split("?", 1)[0]  # без ?utm_…
            done.setdefault(q, {}).update(row)
            out.write(json.dumps({"qid": q, "data": row}, ensure_ascii=False) + "\n")
        progress["n"] += 1
        if progress["n"] % 100 == 0:
            out.flush()
            log.info("wiki: %d requests", progress["n"])

    # pilicense=any: постеры в статьях обычно несвободные (добросовестное использование) —
    # для прода нужно решить вопрос прав, см. документацию.
    todo_ru = [(q, t) for q, t in ru.items() if not done.get(q, {}).get("done_ru")]
    log.info("wiki: ru — %d films to fetch (cached %d)", len(todo_ru), len(ru) - len(todo_ru))
    await asyncio.gather(*(batch("ru", todo_ru[i:i + 20], {
        "prop": "extracts|pageimages", "exintro": "1", "explaintext": "1", "exlimit": "20",
        "piprop": "thumbnail", "pithumbsize": str(POSTER_WIDTH), "pilicense": "any",
        "pilimit": "20"}, True) for i in range(0, len(todo_ru), 20)))
    todo_en = [(q, t) for q, t in en.items()
               if not done.get(q, {}).get("poster_ru") and not done.get(q, {}).get("done_en")]
    log.info("wiki: en posters — %d films to fetch", len(todo_en))
    await asyncio.gather(*(batch("en", todo_en[i:i + 50], {
        "prop": "pageimages", "piprop": "thumbnail", "pithumbsize": str(POSTER_WIDTH),
        "pilicense": "any", "pilimit": "50"}, False) for i in range(0, len(todo_en), 50)))
    out.close()
    return done


# ---------------------------------------------------------------- transform

@dataclass
class Person:
    qid: str
    full_name: str
    slug_base: str
    birth_date: date | None
    photo_url: str | None
    bio: str | None
    links: int


@dataclass
class Credit:
    person: Person
    role: str
    character_name: str | None
    position: int


@dataclass
class Film:
    qid: str
    slug_base: str
    year: int | None
    title: str
    original_title: str | None
    description: str | None
    release_date: date | None
    cover_url: str | None
    metadata: dict
    tags: list[str]
    tmdb_id: int | None
    credits: list[Credit] = field(default_factory=list)


def wd_date(value: str, precision: str) -> tuple[int | None, date | None]:
    """'2006-03-16T00:00:00Z' → (год, дата при точности до дня)."""
    m = re.match(r"^(\d{4})-(\d{2})-(\d{2})T", value or "")
    if not m or not precision or int(precision) < PRECISION_YEAR:
        return None, None
    year = int(m[1])
    if int(precision) < PRECISION_DAY:
        return year, None
    try:
        return year, date(year, int(m[2]), int(m[3]))
    except ValueError:
        return year, None


def commons_url(iri: str, width: int) -> str | None:
    if not iri:
        return None
    url = iri.replace("http://", "https://", 1)
    return f"{url}?width={width}" if " " not in url else None


def bio_from(desc: str | None) -> str | None:
    """«американский актёр» → «Американский актёр.» — как bio в сидах."""
    if not (desc := clean(desc)):
        return None
    desc = desc[:1].upper() + desc[1:]
    return desc if desc[-1] in ".!?" else desc + "."


def slug_source(original: str | None, en: str | None, ru: str) -> str:
    # Латинский оригинал, иначе английское название (как в сидах: stalker-1979), иначе транслит.
    for text in (original, en):
        if text and is_latin(text):
            return text
    return ru


class Dataset:
    """Всё скачанное в памяти: ~1 млн строк титров и ~0,5 млн людей — сотни мегабайт."""

    def __init__(self, cache: Path, threshold: int, top: list[str], wiki_data: dict):
        self.selected = set(top)
        self.order = {q: i for i, q in enumerate(top)}
        self.wiki = wiki_data
        f = lambda name: cache / f"{name}_l{threshold}.csv"

        self.basics: dict[str, dict] = {}
        for row in read_csv(f("basics")):
            q = qid(row["f"])
            if q not in self.selected:
                continue
            b = self.basics.setdefault(q, {"ru": row["ru"], "en": row["en"], "orig": [],
                                           "dates": [], "runtime": [], "tmdb": None})
            b["ru"] = b["ru"] or row["ru"]
            b["en"] = b["en"] or row["en"]
            if row["orig"] and row["orig"] not in b["orig"]:
                b["orig"].append(row["orig"])
            if row["date"]:
                b["dates"].append(wd_date(row["date"], row["prec"]))
            unit = UNITS.get(qid(row["unit"]))
            if row["dur"] and unit:
                try:
                    b["runtime"].append(round(float(row["dur"]) * unit))
                except ValueError:
                    pass
            if row["tmdb"].isdigit():
                b["tmdb"] = int(row["tmdb"])

        self.people: dict[str, dict] = {}
        for row in read_csv(f("people")):
            q = qid(row["p"])
            if not q:
                continue
            p = self.people.setdefault(q, {"ru": "", "en": "", "links": 0, "desc": "",
                                           "birth": None, "bprec": -1, "img": "", "tmdb": None})
            if p["tmdb"] is None and row.get("tmdb", "").isdigit():
                p["tmdb"] = int(row["tmdb"])  # P4985 — склейка людей с TMDB в merge.py
            for k in ("ru", "en", "desc", "img"):
                p[k] = p[k] or row[k]
            # С 2024 года имена людей часто только в «mul» (общая подпись на латинице), без «en».
            p["en"] = p["en"] or row["mul"]
            if row["links"].isdigit():
                p["links"] = max(p["links"], int(row["links"]))
            prec = int(row["bprec"]) if row["bprec"].isdigit() else -1
            if row["birth"] and prec > p["bprec"]:
                p["birth"], p["bprec"] = row["birth"], prec

        self.characters = {}
        for row in read_csv(f("characters")):
            if (q := qid(row["c"])) and (name := clean(row["ru"]) or clean(row["en"])):
                self.characters[q] = name

        self.credits: dict[str, list[tuple]] = {}
        for row in read_csv(f("credits")):
            q, p = qid(row["f"]), qid(row["p"])
            if q in self.selected and p:
                char = self.characters.get(qid(row["char"])) or clean(row["charName"])
                ord_ = int(row["ord"]) if row["ord"].isdigit() else None
                self.credits.setdefault(q, []).append((row["role"], p, ord_, char))

        self.facets: dict[str, dict] = {}
        for row in read_csv(f("facets")):
            q = qid(row["f"])
            if q not in self.selected:
                continue
            fc = self.facets.setdefault(q, {"tags": [], "studios": {}, "countries": []})
            x = qid(row["x"])
            if row["kind"] in ("genre", "type"):
                fc["tags"].extend(GENRES.get(x, []))
            elif row["kind"] == "studio" and (name := clean(row["ru"]) or clean(row["en"])
                                                or clean(row["mul"])):
                fc["studios"][x] = name
            elif row["kind"] == "country":
                iso = (row["iso"] or "").strip().upper()
                if re.fullmatch(r"[A-Z]{2}", iso):
                    fc["countries"].append(iso)
        self._persons: dict[str, Person | None] = {}

    def person(self, q: str) -> Person | None:
        if q in self._persons:
            return self._persons[q]
        p = self.people.get(q)
        person = None
        if p and (name := clean(p["ru"]) or clean(p["en"])):
            name = name[:MAX_TITLE]
            source = p["en"] if p["en"] and is_latin(p["en"]) else name
            _, birth = wd_date(p["birth"], str(p["bprec"])) if p["birth"] else (None, None)
            person = Person(q, name, slugify(source) or slugify(name) or q.lower(), birth,
                            commons_url(p["img"], PHOTO_WIDTH), bio_from(p["desc"]), p["links"])
        self._persons[q] = person
        return person

    def film(self, q: str) -> Film | None:
        b = self.basics.get(q)
        if not b:
            return None
        title = clean(b["ru"]) or clean(b["en"]) or (clean(b["orig"][0]) if b["orig"] else None)
        if not title:
            return None
        original = clean(b["orig"][0]) if b["orig"] else None
        if not original and b["en"] and b["en"] != title:
            original = clean(b["en"])
        years = [y for y, _ in b["dates"] if y]
        days = [d for _, d in b["dates"] if d]
        year = min(years) if years else None
        base = slugify(slug_source(original, b["en"], title)) or slugify(title) or q.lower()
        w = self.wiki.get(q, {})
        fc = self.facets.get(q, {"tags": [], "studios": {}, "countries": []})

        metadata = {}
        runtime = [r for r in b["runtime"] if 1 <= r <= MAX_RUNTIME]
        if runtime:
            metadata["runtime_min"] = min(runtime)
        if countries := unique(fc["countries"]):
            metadata["countries"] = countries
        studios = unique(s[:MAX_STUDIO].strip() for s in sorted(fc["studios"].values()))
        if studios := [s for s in studios if s][:MAX_STUDIOS]:
            metadata["studios"] = studios

        film = Film(
            qid=q, slug_base=base, year=year, title=title[:MAX_TITLE],
            original_title=original[:MAX_TITLE] if original else None,
            description=w.get("extract"), release_date=min(days) if days else None,
            cover_url=(w.get("poster_ru") or w.get("poster_en") or "").split("?", 1)[0] or None,
            metadata=metadata,
            tags=unique(fc["tags"]), tmdb_id=b["tmdb"])

        seen, position = set(), 0
        credits = self.credits.get(q, [])
        directors = [c for c in credits if c[0] == "director"]
        actors = [c for c in credits if c[0] == "actor"]
        # Порядок в титрах (P1545) есть не у всех: без него — самые известные актёры первыми.
        actors.sort(key=lambda c: (c[2] is None, c[2] or 0,
                                   -(self.people.get(c[1]) or {}).get("links", 0), c[1]))
        n_actors = 0
        for role, p, _, char in directors + actors:
            if role == "actor" and n_actors == MAX_ACTORS:
                break
            person = self.person(p)
            key = (p, role, char if role == "actor" else None)
            if not person or key in seen:
                continue
            seen.add(key)
            film.credits.append(Credit(person, role, key[2], position))
            position += 1
            n_actors += role == "actor"
        return film


# ---------------------------------------------------------------- load

STATE_SCHEMA = """
CREATE SCHEMA IF NOT EXISTS wikidata_import;
CREATE TABLE IF NOT EXISTS wikidata_import.movies (
    qid         text        PRIMARY KEY,
    entity_id   uuid        NOT NULL REFERENCES public.entities (id) ON DELETE CASCADE,
    created     boolean     NOT NULL,  -- false: строка уже была (сиды, админка), её не меняем
    tmdb_id     integer,
    imported_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS movies_entity_id_idx ON wikidata_import.movies (entity_id);
CREATE TABLE IF NOT EXISTS wikidata_import.people (
    qid       text PRIMARY KEY,
    person_id uuid NOT NULL REFERENCES public.people (id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS people_person_id_idx ON wikidata_import.people (person_id);
"""


@dataclass
class Stats:
    created: int = 0
    adopted: int = 0
    conflicts: int = 0
    people: int = 0
    credits: int = 0
    tags: int = 0


async def ensure_genres(conn) -> dict[str, object]:
    slugs = list(GENRE_NAMES)
    await conn.execute(
        """INSERT INTO tags (slug, name, kind)
           SELECT s, n, 'genre' FROM unnest($1::text[], $2::text[]) AS u(s, n)
           ON CONFLICT (slug) DO NOTHING""", slugs, [GENRE_NAMES[s] for s in slugs])
    rows = await conn.fetch("SELECT id, slug FROM tags WHERE slug = ANY($1::text[])", slugs)
    return {r["slug"]: r["id"] for r in rows}


def norm(text: str | None) -> str:
    return re.sub(r"\s+", " ", (text or "").strip().lower())


async def load_batch(conn, films: list[Film], tags: dict, stats: Stats):
    async with conn.transaction():
        # 1. entities. Готовая непривязанная сущность-фильм с тем же slug (с годом или без,
        #    если год совпадает) или с тем же названием и годом — это тот же фильм: привязываем.
        yearless = lambda f: f.slug_base
        with_year = lambda f: f"{f.slug_base}-{f.year}" if f.year else f.slug_base
        fallback = lambda f: f"{with_year(f)}-{f.qid.lower()}"
        slugs = unique(s for f in films for s in (with_year(f), yearless(f), fallback(f)))
        rows = await conn.fetch(
            """SELECT e.id, e.slug, e.kind::text = 'movie' AS movie,
                      extract(year FROM e.release_date)::int AS year,
                      EXISTS (SELECT 1 FROM wikidata_import.movies w WHERE w.entity_id = e.id) AS linked
               FROM entities e WHERE e.slug = ANY($1::text[])""", slugs)
        by_slug = {r["slug"]: r for r in rows}
        titles = unique(norm(t) for f in films if f.year for t in (f.title, f.original_title) if t)
        rows = await conn.fetch(
            """SELECT e.id, lower(e.title) AS title, lower(e.original_title) AS original,
                      extract(year FROM e.release_date)::int AS year
               FROM entities e
               WHERE e.kind = 'movie' AND e.release_date IS NOT NULL
                 AND (lower(e.title) = ANY($1::text[]) OR lower(e.original_title) = ANY($1::text[]))
                 AND NOT EXISTS (SELECT 1 FROM wikidata_import.movies w WHERE w.entity_id = e.id)""",
            titles)
        by_title: dict[tuple, object] = {}
        for r in rows:
            for t in (r["title"], r["original"]):
                if t:
                    by_title.setdefault((norm(t), r["year"]), r["id"])

        taken_slugs, taken_ids, plan = set(), set(), []
        for f in films:
            decision = None
            # Совпадение по названию + году (сиды с «нестандартным» slug: lotr-fellowship-2001).
            for t in (f.original_title, f.title):
                eid = by_title.get((norm(t), f.year)) if t and f.year else None
                if eid and eid not in taken_ids:
                    decision = ("adopt", eid)
                    break
            if not decision:
                for slug, adopt_only in ((with_year(f), False), (yearless(f), True), (fallback(f), False)):
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
                log.warning("%s: no free slug for %s", f.qid, with_year(f))
                continue
            if decision[0] == "create":
                taken_slugs.add(decision[1])
            else:
                taken_ids.add(decision[1])
            plan.append((f, decision))

        new = [(f, d[1]) for f, d in plan if d[0] == "create"]
        inserted = await conn.fetch(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, metadata)
               SELECT 'movie', s, t, o, d, r, c, m::jsonb
               FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::date[], $6::text[],
                           $7::text[]) AS u(s, t, o, d, r, c, m)
               ON CONFLICT (slug) DO NOTHING
               RETURNING id, slug""",
            [s for _, s in new], [f.title for f, _ in new], [f.original_title for f, _ in new],
            [f.description for f, _ in new], [f.release_date for f, _ in new],
            [f.cover_url for f, _ in new],
            [json.dumps(f.metadata, ensure_ascii=False) for f, _ in new])
        new_ids = {r["slug"]: r["id"] for r in inserted}

        links, created = [], []
        for f, (kind, value) in plan:
            if kind == "create" and value in new_ids:
                links.append((f.qid, new_ids[value], True, f.tmdb_id))
                created.append((f, new_ids[value]))
            elif kind == "adopt":
                links.append((f.qid, value, False, f.tmdb_id))
            else:
                stats.conflicts += 1  # slug заняли параллельно — следующий запуск повторит
        if links:
            await conn.execute(
                """INSERT INTO wikidata_import.movies (qid, entity_id, created, tmdb_id)
                   SELECT * FROM unnest($1::text[], $2::uuid[], $3::bool[], $4::int[])
                   ON CONFLICT DO NOTHING""", *map(list, zip(*links)))
        stats.created += len(created)
        stats.adopted += sum(1 for l in links if not l[2])
        if not created:  # у привязанных готовых карточек титры и жанры ведёт админка
            return

        # 2. people
        persons = {c.person.qid: c.person for f, _ in created for c in f.credits}
        rows = await conn.fetch(
            "SELECT qid, person_id FROM wikidata_import.people WHERE qid = ANY($1::text[])",
            list(persons))
        person_ids = {r["qid"]: r["person_id"] for r in rows}
        # Известные — первыми: им достаётся slug без суффикса.
        fresh = sorted((p for q, p in persons.items() if q not in person_ids),
                       key=lambda p: (-p.links, int(p.qid[1:])))
        if fresh:
            pfallback = lambda p: f"{p.slug_base}-{p.qid.lower()}"
            rows = await conn.fetch(
                """SELECT p.id, p.slug,
                          EXISTS (SELECT 1 FROM wikidata_import.people w WHERE w.person_id = p.id) AS linked
                   FROM people p WHERE p.slug = ANY($1::text[])""",
                unique(s for p in fresh for s in (p.slug_base, pfallback(p))))
            existing = {r["slug"]: r for r in rows}
            taken, adopted_ids, plan_p = set(), set(), []
            for p in fresh:
                for slug in (p.slug_base, pfallback(p)):
                    if slug in taken:
                        continue
                    r = existing.get(slug)
                    if r is None:
                        taken.add(slug)
                        plan_p.append((p, "create", slug))
                        break
                    if not r["linked"] and r["id"] not in adopted_ids:
                        adopted_ids.add(r["id"])
                        plan_p.append((p, "adopt", r["id"]))
                        break
            to_create = [(p, s) for p, k, s in plan_p if k == "create"]
            inserted = await conn.fetch(
                """INSERT INTO people (slug, full_name, birth_date, photo_url, bio)
                   SELECT * FROM unnest($1::text[], $2::text[], $3::date[], $4::text[], $5::text[])
                   ON CONFLICT (slug) DO NOTHING
                   RETURNING id, slug""",
                [s for _, s in to_create], [p.full_name for p, _ in to_create],
                [p.birth_date for p, _ in to_create], [p.photo_url for p, _ in to_create],
                [p.bio for p, _ in to_create])
            new_people = {r["slug"]: r["id"] for r in inserted}
            stats.people += len(new_people)
            new_links = [(p.qid, new_people[v] if k == "create" else v) for p, k, v in plan_p
                         if k == "adopt" or v in new_people]
            if new_links:
                await conn.execute(
                    """INSERT INTO wikidata_import.people (qid, person_id)
                       SELECT * FROM unnest($1::text[], $2::uuid[]) ON CONFLICT DO NOTHING""",
                    *map(list, zip(*new_links)))
            person_ids.update(new_links)

        # 3. entity_credits
        credits = [(eid, person_ids[c.person.qid], c.role, c.character_name, c.position)
                   for f, eid in created for c in f.credits if c.person.qid in person_ids]
        if credits:
            result = await conn.execute(
                """INSERT INTO entity_credits (entity_id, person_id, role, character_name, position)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::text[], $5::int[])
                   ON CONFLICT DO NOTHING""", *map(list, zip(*credits)))
            stats.credits += int(result.split()[-1])

        # 4. entity_tags
        pairs = [(eid, tags[t]) for f, eid in created for t in f.tags if t in tags]
        if pairs:
            result = await conn.execute(
                """INSERT INTO entity_tags (entity_id, tag_id)
                   SELECT * FROM unnest($1::uuid[], $2::uuid[]) ON CONFLICT DO NOTHING""",
                *map(list, zip(*pairs)))
            stats.tags += int(result.split()[-1])


async def load(args, dsn: str, data: Dataset, top: list[str]):
    conn = await asyncpg.connect(dsn)
    try:
        if await conn.fetchval("SELECT to_regclass('catalog_import.movies') IS NOT NULL"):
            sys.exit("catalog_import exists: films are loaded by merge.py (TMDB + Wikidata); "
                     "run this script with --stage download/wiki only")
        await conn.execute(STATE_SCHEMA)
        tags = await ensure_genres(conn)
        done = {r["qid"] for r in await conn.fetch("SELECT qid FROM wikidata_import.movies")}
        todo = [q for q in top if q not in done]
        log.info("load: %d films to load (already loaded %d)", len(todo), len(top) - len(todo))
        stats, started, skipped = Stats(), time.monotonic(), 0
        for i in range(0, len(todo), args.batch_size):
            films = []
            for q in todo[i:i + args.batch_size]:
                if film := data.film(q):
                    films.append(film)
                else:
                    skipped += 1
            await load_batch(conn, films, tags, stats)
            n = min(i + args.batch_size, len(todo))
            if (i // args.batch_size) % 20 == 0 or n == len(todo):
                speed = n / (time.monotonic() - started)
                log.info("load: %d/%d (%.0f films/s) | entities +%d, adopted %d, people +%d, "
                         "credits +%d, tags +%d, no title %d, slug conflicts %d", n, len(todo),
                         speed, stats.created, stats.adopted, stats.people, stats.credits,
                         stats.tags, skipped, stats.conflicts)
        await fill_from_wiki(conn, data)
    finally:
        await conn.close()
    log.info("next: scripts/reindex-search.sh — rebuild Meilisearch")


async def fill_from_wiki(conn, data: Dataset):
    """Описания и постеры, докачанные после загрузки фильма (этап wiki идёт медленнее): только
    в пустые поля и только у созданных импортом — правки админки не перезаписываются."""
    rows = []
    for q, w in data.wiki.items():
        cover = (w.get("poster_ru") or w.get("poster_en") or "").split("?", 1)[0] or None
        if q in data.selected and (w.get("extract") or cover):
            rows.append((q, w.get("extract"), cover))
    if not rows:
        return
    result = await conn.execute(
        """UPDATE entities e
           SET description = COALESCE(e.description, u.d), cover_url = COALESCE(e.cover_url, u.c)
           FROM unnest($1::text[], $2::text[], $3::text[]) AS u(q, d, c)
           JOIN wikidata_import.movies w ON w.qid = u.q AND w.created
           WHERE e.id = w.entity_id
             AND ((e.description IS NULL AND u.d IS NOT NULL) OR (e.cover_url IS NULL AND u.c IS NOT NULL))""",
        *map(list, zip(*rows)))
    log.info("wiki: filled description/poster of %s films", result.split()[-1])


# ---------------------------------------------------------------- main

def to_json(film: Film) -> str:
    return json.dumps(film, default=lambda o: o.__dict__ if hasattr(o, "__dict__") else str(o),
                      ensure_ascii=False, indent=2)


async def run(args):
    dsn = os.environ.get("DATABASE_URL")
    if args.stage in ("all", "load") and not args.dry_run and not dsn:
        sys.exit("DATABASE_URL is required")
    timeout = aiohttp.ClientTimeout(total=None, sock_connect=30, sock_read=600)
    async with aiohttp.ClientSession(timeout=timeout, headers={"User-Agent": USER_AGENT}) as session:
        top, threshold = await download(args, session)
        if args.stage == "download":
            return
        wiki_data = (wiki_cache(args.cache_dir) if args.no_wiki
                     else await wiki(args, session, top, threshold))
    if args.stage == "wiki":
        return
    data = Dataset(Path(args.cache_dir), threshold, top, wiki_data)
    log.info("dataset: %d films, %d people, %d films with credits", len(data.basics),
             len(data.people), len(data.credits))
    if args.dry_run:
        for q in top[:args.dry_run]:
            print(to_json(data.film(q)))
        return
    await load(args, dsn, data, top)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    p.add_argument("--limit", type=int, default=100_000, help="most known films to import")
    p.add_argument("--min-links", type=int, default=2, help="ignore films with fewer sitelinks")
    p.add_argument("--stage", choices=["all", "download", "wiki", "load"], default="all",
                   help="stop after this stage (load reuses the cache)")
    p.add_argument("--endpoint", default=QLEVER, help="SPARQL endpoint with full Wikidata")
    p.add_argument("--cache-dir", default=os.path.expanduser("~/.cache/nexus-catalog-import/wikidata"),
                   help="outside the repo: cargo-watch restarts the app on every write in it")
    p.add_argument("--refresh", action="store_true", help="re-download SPARQL results")
    p.add_argument("--no-wiki", action="store_true", help="don't query Wikipedia, use only what wiki.jsonl already has")
    p.add_argument("--wiki-concurrency", type=int, default=1)
    p.add_argument("--wiki-rate", type=float, default=1.0, help="Wikipedia requests per second")
    p.add_argument("--batch-size", type=int, default=500, help="films per DB transaction")
    p.add_argument("--dry-run", type=int, metavar="N", default=0,
                   help="print N transformed films, write nothing")
    p.add_argument("-v", "--verbose", action="store_true")
    args = p.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(asctime)s %(levelname)s %(message)s", datefmt="%H:%M:%S")
    asyncio.run(run(args))


if __name__ == "__main__":
    main()
