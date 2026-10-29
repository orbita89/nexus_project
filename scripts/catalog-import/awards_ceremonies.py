#!/usr/bin/env python3
"""Церемонии премий целиком из Wikidata: категория → все номинанты года, работы и люди.

Зачем отдельный скрипт. `awards_wikidata.py` идёт от фильма (`?f wdt:P4947 ?tmdb`, затем его
P166/P1411), поэтому забирает только награды, где **субъект утверждения — сам фильм**. За
сценарий, режиссуру, актёрские работы, музыку и монтаж в Wikidata номинируют **человека**, и
утверждение висит на нём, с квалификатором P1686 («за работу») → фильм. Такие номинации до нас
не доходили: на странице церемонии категория показывала одного победителя вместо пяти
номинантов, а `person` в номинациях всегда был пустым.

Здесь порядок обратный — от премии:
  1. categories — категории премии (`?cat wdt:P361 wd:<премия>`), с русским и английским
     названием;
  2. download — по пачке категорий: все утверждения P166 (награда получена) и P1411
     (номинация) с годом (квалификатор P585, иначе дата церемонии P805 → P585), субъектом
     (работа или человек) и работой из P1686. Каждая пачка — свой CSV в --cache-dir, прогон
     после обрыва продолжается с места остановки (--refresh перекачивает всё);
  3. load — церемония переписывается целиком, как `PUT /admin/awards/{slug}/{year}/nominations`:
     год, которого в выгрузке нет, не трогается.

Сопоставление с каталогом: работа — по id TMDB (`entities.external_ids->>'tmdb'`), человек — по
имени (`people.full_name` / `original_name`, сравнение без регистра и «ё»). Кого не нашли,
записываем именем в `award_nominations.name`: категория из пяти номинантов должна выглядеть
полной, даже если двух фильмов у нас нет.

Зависимости — `requirements.txt` рядом (aiohttp, asyncpg); venv: `scripts/catalog-import/.venv`.

Сначала — что нашлось в Wikidata по каждой премии (базу не трогает):
  DATABASE_URL=... .venv/bin/python awards_ceremonies.py --check

Проверка на одной премии за пару лет (минуты):
  DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
  .venv/bin/python awards_ceremonies.py --award oscar --since-year 2020 --dry-run --verbose

Все курируемые премии (`position > 0` в таблице `awards`; долго, прогон можно прерывать):
  DATABASE_URL=... .venv/bin/python awards_ceremonies.py

После загрузки: переиндексация поиска (фасеты `award_wins` / `award_nominations` считаются при
индексации). Карточки и страницы церемоний читают PostgreSQL напрямую.
"""

import argparse
import asyncio
import csv
import logging
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

import aiohttp
import asyncpg

log = logging.getLogger("awards-ceremonies")

WDQS = "https://query.wikidata.org/sparql"
USER_AGENT = "nexus-catalog-import/1.0 (Nexus dev data import{}) python-aiohttp".format(
    f"; {c}" if (c := os.environ.get("WIKIMEDIA_CONTACT")) else "")

# Премии берутся из таблицы `awards` (их завёл awards_build.py), названия-синонимы — из его же
# ALIASES: по ним ищутся фильмы, у которых эта премия уже записана в metadata.
from awards_build import ALIASES  # noqa: E402  (лежит рядом)

MIN_YEAR, MAX_YEAR = 1880, 2100
MAX_TITLE = 300
# Сколько категорий в одном запросе: запрос должен укладываться в таймаут WDQS (60 с).
CATEGORIES_PER_QUERY = 8
# Колонки CSV выгрузки.
FIELDS = ["cat", "catRu", "catEn", "year", "subj", "subjRu", "subjEn", "human",
          "subjTmdb", "workRu", "workEn", "workTmdb"]

PREFIXES = """PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX p: <http://www.wikidata.org/prop/>
PREFIX ps: <http://www.wikidata.org/prop/statement/>
PREFIX pq: <http://www.wikidata.org/prop/qualifier/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
"""


def award_query(tmdb_ids: list[str]) -> str:
    """Элемент премии в Wikidata — по нашим же фильмам: их награда → её «часть чего-либо».

    Так не нужно вписывать QID руками (легко ошибиться: Q812939 — это немецкий закон о
    госслужбе, а не BAFTA). Берём самый частый родитель среди категорий этих фильмов.
    """
    values = " ".join(f'"{i}"' for i in tmdb_ids)
    return PREFIXES + f"""SELECT ?fam ?famRu ?famEn (COUNT(*) AS ?n) WHERE {{
  VALUES ?tmdb {{ {values} }}
  ?f wdt:P4947 ?tmdb .
  {{ ?f p:P166 ?st . ?st ps:P166 ?cat }} UNION {{ ?f p:P1411 ?st . ?st ps:P1411 ?cat }}
  {{ ?cat wdt:P361 ?fam }} UNION {{ ?cat wdt:P31 ?fam }}
  OPTIONAL {{ ?fam rdfs:label ?famRu FILTER(LANG(?famRu) = "ru") }}
  OPTIONAL {{ ?fam rdfs:label ?famEn FILTER(LANG(?famEn) = "en") }}
}} GROUP BY ?fam ?famRu ?famEn ORDER BY DESC(?n) LIMIT 5"""


def categories_query(award_qid: str) -> str:
    """Категории премии: и «часть» (P361), и «экземпляр» (P31).

    У «Оскара» почти все категории — P31 («премия «Оскар» за лучший адаптированный сценарий»
    это экземпляр «Оскара»), а P361 стоит у полутора десятков; у Канн наоборот. Берём оба,
    иначе на церемонии не хватает половины категорий.
    """
    return PREFIXES + f"""SELECT DISTINCT ?cat ?catRu ?catEn WHERE {{
  {{ ?cat wdt:P361 wd:{award_qid} }} UNION {{ ?cat wdt:P31 wd:{award_qid} }}
  OPTIONAL {{ ?cat rdfs:label ?catRu FILTER(LANG(?catRu) = "ru") }}
  OPTIONAL {{ ?cat rdfs:label ?catEn FILTER(LANG(?catEn) = "en") }}
}}"""


def nominations_query(prop: str, cats: list[str]) -> str:
    """P166 — награда получена, P1411 — номинация. Субъект — работа или человек.

    Год: квалификатор P585, иначе дата церемонии (P805 → P585). Работа человека — P1686.
    """
    values = " ".join(f"wd:{qid}" for qid in cats)
    return PREFIXES + f"""SELECT ?cat ?catRu ?catEn ?year ?subj ?subjRu ?subjEn ?human
       ?subjTmdb ?workRu ?workEn ?workTmdb WHERE {{
  VALUES ?cat {{ {values} }}
  ?subj p:{prop} ?st .
  ?st ps:{prop} ?cat .
  OPTIONAL {{ ?st pq:P585 ?when }}
  OPTIONAL {{ ?st pq:P805 ?ceremony . ?ceremony wdt:P585 ?ceremonyWhen }}
  BIND(YEAR(COALESCE(?when, ?ceremonyWhen)) AS ?year)
  OPTIONAL {{ ?st pq:P1686 ?work .
             OPTIONAL {{ ?work wdt:P4947 ?workTmdb }}
             OPTIONAL {{ ?work rdfs:label ?workRu FILTER(LANG(?workRu) = "ru") }}
             OPTIONAL {{ ?work rdfs:label ?workEn FILTER(LANG(?workEn) = "en") }} }}
  OPTIONAL {{ ?subj wdt:P4947 ?subjTmdb }}
  OPTIONAL {{ ?subj rdfs:label ?subjRu FILTER(LANG(?subjRu) = "ru") }}
  OPTIONAL {{ ?subj rdfs:label ?subjEn FILTER(LANG(?subjEn) = "en") }}
  OPTIONAL {{ ?cat rdfs:label ?catRu FILTER(LANG(?catRu) = "ru") }}
  OPTIONAL {{ ?cat rdfs:label ?catEn FILTER(LANG(?catEn) = "en") }}
  BIND(EXISTS {{ ?subj wdt:P31 wd:Q5 }} AS ?human)
}}"""


async def ask(session: aiohttp.ClientSession, sparql: str, retries: int = 3) -> list[dict]:
    """Запрос к WDQS → строки CSV-ответа. Пустой список — запрос не прошёл."""
    for attempt in range(retries):
        try:
            async with session.post(
                WDQS,
                data={"query": sparql},
                headers={"Accept": "text/csv", "User-Agent": USER_AGENT},
                timeout=aiohttp.ClientTimeout(total=180),
            ) as response:
                body = await response.text()
                if response.status != 200:
                    raise RuntimeError(f"{response.status}: {body[:200]}")
                return list(csv.DictReader(body.splitlines()))
        except Exception as error:  # сеть, таймаут, 429 — пробуем ещё раз
            delay = 5 * (attempt + 1)
            log.warning("запрос не прошёл (%s), повтор через %s с", error, delay)
            await asyncio.sleep(delay)
    return []


def match_score(row: dict, names: list[str]) -> int:
    """Насколько название кандидата похоже на наши названия премии: 2 — одно внутри другого,
    1 — общее значимое слово, 0 — ничего общего."""
    labels = [name_key(clean(row.get(key))) for key in ("famRu", "famEn")]
    labels = [label for label in labels if label]
    ours = [name_key(name) for name in names]
    for label in labels:
        for name in ours:
            if label in name or name in label:
                return 2
    words = {word for label in labels for word in label.split() if len(word) > 4}
    if words & {word for name in ours for word in name.split() if len(word) > 4}:
        return 1
    return 0


def qid(uri: str) -> str:
    return (uri or "").rsplit("/", 1)[-1]


def clean(value: str | None, limit: int = MAX_TITLE) -> str:
    value = re.sub(r"\s+", " ", (value or "")).strip()
    return value[:limit]


def category_title(row: dict, award_title: str) -> str:
    """«Премия „Оскар“ за лучший адаптированный сценарий» → «за лучший адаптированный сценарий».

    Так же, как в `metadata.awards[].category`: иначе та же категория заведётся второй строкой.
    """
    title = clean(row.get("catRu")) or clean(row.get("catEn"))
    if not title:
        return ""
    stripped = re.sub(r"[«»\"„“]", "", title)
    needle = re.sub(r"[«»\"„“]", "", award_title)
    index = stripped.lower().find(needle.lower()) if needle else -1
    if index >= 0:
        tail = stripped[index + len(needle):].strip(" -–—:,")
        if tail:
            return tail[:MAX_TITLE]
    return title


def year_of(raw: str | None) -> int | None:
    try:
        year = int((raw or "").strip())
    except ValueError:
        return None
    return year if MIN_YEAR <= year <= MAX_YEAR else None


def name_key(name: str) -> str:
    return re.sub(r"\s+", " ", name.lower().replace("ё", "е")).strip()


async def discover(session, conn, award: str, cache: Path, args) -> tuple[str, str]:
    """Элемент премии в Wikidata: ищется по фильмам каталога и кэшируется. ('', '') — не нашли."""
    path = cache / f"{award}-item.csv"
    if path.exists() and not args.refresh:
        with path.open(newline="", encoding="utf-8") as f:
            row = next(csv.DictReader(f), None)
        if row:
            return row["qid"], row["title"]
    if args.no_download:
        return "", ""
    names = [name for name, key in ALIASES.items() if key == award]
    if not names:
        names = [row["title"] for row in await conn.fetch(
            "SELECT title FROM awards WHERE slug = $1", award)]
    tmdb = [row["tmdb"] for row in await conn.fetch(
        """SELECT e.external_ids->>'tmdb' AS tmdb FROM entities e
            WHERE e.external_ids ? 'tmdb'
              AND EXISTS (SELECT 1 FROM jsonb_array_elements(e.metadata -> 'awards') a
                           WHERE a->>'award' = ANY($1))
            LIMIT 60""", names)]
    if not tmdb:
        log.warning("%s: в каталоге нет фильмов с этой премией — нечем её найти", award)
        return "", ""
    rows = await ask(session, award_query(tmdb))
    if not rows:
        return "", ""
    # Самый частый родитель — не всегда наш: у каннских фильмов хватает и других премий.
    # Поэтому сначала тот, чьё название совпадает с нашими (ALIASES), и только потом частота.
    best = max(rows, key=lambda row: (match_score(row, names), int(row.get("n", 0) or 0)))
    qid_found = qid(best.get("fam", ""))
    title = clean(best.get("famRu")) or clean(best.get("famEn"))
    with path.open("w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, ["qid", "title", "n"])
        writer.writeheader()
        writer.writerow({"qid": qid_found, "title": title, "n": best.get("n", "")})
    log.info("%s = %s (%s), утверждений %s", award, title, qid_found, best.get("n", "?"))
    return qid_found, title


async def categories(session, award: str, award_qid: str, cache: Path, args) -> dict[str, str]:
    """Категории премии: {QID: название}. Кэшируется, как и выгрузка номинаций."""
    path = cache / f"{award}-categories.csv"
    if args.refresh or not path.exists():
        if args.no_download:
            return {}
        award_title = args.award_title
        rows = await ask(session, categories_query(award_qid))
        if not rows:
            log.warning("%s: категорий не нашлось — проверьте QID", award)
            return {}
        with path.open("w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, ["cat", "catRu", "catEn", "awardTitle"])
            writer.writeheader()
            for row in rows:
                writer.writerow({
                    "cat": qid(row.get("cat", "")),
                    "catRu": row.get("catRu", ""),
                    "catEn": row.get("catEn", ""),
                    "awardTitle": award_title,
                })
    if not path.exists():
        return {}
    with path.open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    award_title = clean(rows[0].get("awardTitle")) if rows else ""
    found = {}
    for row in rows:
        title = category_title(row, award_title)
        if row["cat"] and title:
            found[row["cat"]] = title
    log.info("%s: категорий %d", award, len(found))
    return found


async def download(session, award: str, cats: dict[str, str], cache: Path, args) -> None:
    """Выгрузка номинаций премии: по пачке категорий на запрос, каждая — свой CSV."""
    chunks = [list(cats)[i:i + CATEGORIES_PER_QUERY]
              for i in range(0, len(cats), CATEGORIES_PER_QUERY)]
    for number, chunk in enumerate(chunks):
        for prop, suffix in (("P166", "received"), ("P1411", "nominated")):
            path = cache / f"{award}-{number:04d}-{suffix}.csv"
            if path.exists() and not args.refresh:
                continue
            if args.no_download:
                continue
            rows = await ask(session, nominations_query(prop, chunk))
            with path.open("w", newline="", encoding="utf-8") as f:
                writer = csv.DictWriter(f, FIELDS)
                writer.writeheader()
                for row in rows:
                    writer.writerow({
                        "cat": qid(row.get("cat", "")),
                        "catRu": row.get("catRu", ""),
                        "catEn": row.get("catEn", ""),
                        "year": row.get("year", ""),
                        "subj": qid(row.get("subj", "")),
                        "subjRu": row.get("subjRu", ""),
                        "subjEn": row.get("subjEn", ""),
                        "human": row.get("human", ""),
                        "subjTmdb": row.get("subjTmdb", ""),
                        "workRu": row.get("workRu", ""),
                        "workEn": row.get("workEn", ""),
                        "workTmdb": row.get("workTmdb", ""),
                    })
            log.debug("%s: %s строк", path.name, len(rows))
        log.info("%s: пачка %d/%d", award, number + 1, len(chunks))


def collect(award: str, cats: dict[str, str], cache: Path, args) -> dict:
    """CSV выгрузки → {год: {категория: [номинация]}}. Повторы одного утверждения схлопываются."""
    years: dict[int, dict[str, dict[tuple, dict]]] = defaultdict(lambda: defaultdict(dict))
    for path in sorted(cache.glob(f"{award}-[0-9]*.csv")):
        won = path.name.endswith("-received.csv")
        with path.open(newline="", encoding="utf-8") as f:
            for row in csv.DictReader(f):
                year = year_of(row.get("year"))
                title = cats.get(row.get("cat", ""))
                if year is None or not title:
                    continue
                if args.since_year and year < args.since_year:
                    continue
                human = (row.get("human") or "").lower() in ("true", "1")
                subject = clean(row.get("subjRu")) or clean(row.get("subjEn"))
                work_name = clean(row.get("workRu")) or clean(row.get("workEn"))
                nomination = {
                    "person": subject if human else "",
                    "entity_tmdb": clean(row.get("workTmdb") if human else row.get("subjTmdb"), 32),
                    "name": work_name if human else subject,
                    "won": won,
                }
                if not any((nomination["person"], nomination["entity_tmdb"], nomination["name"])):
                    continue
                key = (nomination["person"], nomination["entity_tmdb"], nomination["name"])
                # Победа важнее номинации: у победителя бывают оба утверждения.
                current = years[year][title].get(key)
                if current is None or (won and not current["won"]):
                    years[year][title][key] = nomination
    return {year: {title: dedup(list(items.values())) for title, items in cats_of.items()}
            for year, cats_of in years.items()}


def dedup(items: list[dict]) -> list[dict]:
    """Одна номинация категории — одна строка.

    В Wikidata одна и та же номинация записана несколько раз: отдельно у работы и отдельно у
    каждого причастного человека (с P1686 → работа). «Лучший фильм» так превращается в три
    строки на фильм — по продюсерам, а победа фильма дублирует победу сценариста. Схлопываем по
    работе и флагу `won`: имена людей идут списком в `name`, первый становится `person`.
    """
    merged: dict[tuple, dict] = {}
    for item in items:
        key = (item["entity_tmdb"] or item["name"], item["won"])
        current = merged.get(key)
        if current is None:
            merged[key] = dict(item, people=[item["person"]] if item["person"] else [])
            continue
        if item["person"] and item["person"] not in current["people"]:
            current["people"].append(item["person"])
        # Строка самой работы приносит её название, строка человека — нет.
        if not item["person"] and item["name"]:
            current["name"] = item["name"]
    result = []
    for item in merged.values():
        people = item.pop("people")
        if people:
            item["person"] = people[0]
            # Соавторы не теряются: на странице церемонии это строка под постером.
            item["name"] = ", ".join(people) if item["entity_tmdb"] else people[0]
        result.append(item)
    return result


async def resolve(conn, nominations: list[dict]) -> None:
    """Проставляет `entity_id` и `person_id`: работы — по id TMDB, люди — по имени."""
    tmdb = sorted({n["entity_tmdb"] for n in nominations if n["entity_tmdb"]})
    names = sorted({name_key(n["person"]) for n in nominations if n["person"]})
    entities = {row["tmdb"]: row["id"] for row in await conn.fetch(
        "SELECT external_ids->>'tmdb' AS tmdb, id FROM entities WHERE external_ids->>'tmdb' = ANY($1)",
        tmdb)} if tmdb else {}
    people = {}
    if names:
        for row in await conn.fetch(
            """SELECT id, replace(lower(full_name), 'ё', 'е') AS key FROM people
                WHERE replace(lower(full_name), 'ё', 'е') = ANY($1)
               UNION ALL
               SELECT id, replace(lower(original_name), 'ё', 'е') FROM people
                WHERE replace(lower(original_name), 'ё', 'е') = ANY($1)""",
            names,
        ):
            # Тёзки: берём первого — у номинации всё равно остаётся имя строкой.
            people.setdefault(row["key"], row["id"])
    for nomination in nominations:
        nomination["entity_id"] = entities.get(nomination["entity_tmdb"])
        nomination["person_id"] = people.get(name_key(nomination["person"])) \
            if nomination["person"] else None
        # Имя нужно, только когда ни работы, ни человека в каталоге нет.
        if nomination["person"] and not nomination["person_id"]:
            nomination["name"] = nomination["person"]
        elif nomination["entity_id"] or nomination["person_id"]:
            nomination["name"] = nomination["name"] or None


async def load(conn, award: str, data: dict, args) -> tuple[int, int]:
    """Переписывает церемонии премии. Возвращает (церемоний, номинаций)."""
    award_id = await conn.fetchval("SELECT id FROM awards WHERE slug = $1", award)
    if award_id is None:
        log.warning("%s: нет такой премии в каталоге — пропуск", award)
        return 0, 0
    categories_by_title = {row["title"]: row["id"] for row in await conn.fetch(
        "SELECT id, title FROM award_categories WHERE award_id = $1", award_id)}
    taken = {row["slug"] for row in await conn.fetch(
        "SELECT slug FROM award_categories WHERE award_id = $1", award_id)}

    editions = nominations = 0
    for year, cats_of in sorted(data.items()):
        flat = [n for items in cats_of.values() for n in items]
        await resolve(conn, flat)
        edition_id = await conn.fetchval(
            """INSERT INTO award_editions (award_id, year) VALUES ($1, $2)
               ON CONFLICT (award_id, year) DO UPDATE SET year = EXCLUDED.year RETURNING id""",
            award_id, year)
        # Церемония переписывается целиком — как PUT .../nominations.
        await conn.execute("DELETE FROM award_nominations WHERE edition_id = $1", edition_id)
        rows = []
        for position, (title, items) in enumerate(sorted(cats_of.items())):
            category_id = categories_by_title.get(title)
            if category_id is None:
                slug = slugify(title, taken, position)
                category_id = await conn.fetchval(
                    """INSERT INTO award_categories (award_id, slug, title, position)
                       VALUES ($1, $2, $3, $4) RETURNING id""",
                    award_id, slug, title, position)
                categories_by_title[title] = category_id
            for index, n in enumerate(sorted(items, key=lambda n: not n["won"])):
                rows.append((edition_id, category_id, n["entity_id"], n["person_id"],
                             n["name"] or None, n["won"], index))
        await conn.executemany(
            """INSERT INTO award_nominations (edition_id, category_id, entity_id, person_id,
                                              name, won, position)
               VALUES ($1, $2, $3, $4, $5, $6, $7)""", rows)
        editions += 1
        nominations += len(rows)
        log.debug("%s %d: категорий %d, номинаций %d", award, year, len(cats_of), len(rows))
    log.info("%s: церемоний %d, номинаций %d", award, editions, nominations)
    return editions, nominations


TRANSLIT = {"а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh",
            "з": "z", "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o",
            "п": "p", "р": "r", "с": "s", "т": "t", "у": "u", "ф": "f", "х": "h", "ц": "ts",
            "ч": "ch", "ш": "sh", "щ": "sch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu",
            "я": "ya"}


def slugify(title: str, taken: set[str], position: int) -> str:
    out = "".join(TRANSLIT.get(ch, ch) for ch in title.lower())
    out = re.sub(r"-{2,}", "-", re.sub(r"[^a-z0-9]+", "-", out)).strip("-")[:100].strip("-")
    slug = out or f"category-{position}"
    if slug in taken:
        for n in range(2, 1000):
            candidate = f"{slug[:96]}-{n}"
            if candidate not in taken:
                slug = candidate
                break
    taken.add(slug)
    return slug


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database-url", default=os.environ.get("DATABASE_URL"))
    parser.add_argument("--award", action="append",
                        help="только эта премия (slug из таблицы awards), можно много")
    parser.add_argument("--since-year", type=int, help="церемонии не раньше этого года")
    parser.add_argument("--cache-dir", default="cache/ceremonies")
    parser.add_argument("--refresh", action="store_true", help="перекачать выгрузку")
    parser.add_argument("--no-download", action="store_true",
                        help="только загрузка из уже скачанного кэша")
    parser.add_argument("--award-title", default="", help=argparse.SUPPRESS)
    parser.add_argument("--dry-run", action="store_true", help="не записывать, откатить")
    parser.add_argument("--check", action="store_true",
                        help="только найти премии и их категории, базу не трогать")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(levelname)s %(message)s")
    if not args.database_url:
        log.error("нужен DATABASE_URL")
        return 2
    cache = Path(args.cache_dir)
    cache.mkdir(parents=True, exist_ok=True)

    conn = await asyncpg.connect(args.database_url)
    awards = args.award or [row["slug"] for row in await conn.fetch(
        "SELECT slug FROM awards WHERE position > 0 ORDER BY position DESC")]
    total = [0, 0]
    try:
        tx = conn.transaction()
        await tx.start()
        async with aiohttp.ClientSession() as session:
            for slug in awards:
                award_qid, args.award_title = await discover(session, conn, slug, cache, args)
                if not award_qid:
                    log.warning("%s: премия в Wikidata не найдена — пропуск", slug)
                    continue
                cats = await categories(session, slug, award_qid, cache, args)
                if args.check:
                    log.info("%-22s %-10s %-34s категорий %d", slug, award_qid,
                             args.award_title, len(cats))
                    continue
                if not cats:
                    continue
                await download(session, slug, cats, cache, args)
                data = collect(slug, cats, cache, args)
                if not data:
                    log.warning("%s: в выгрузке нет церемоний — ничего не трогаем", slug)
                    continue
                editions, nominations = await load(conn, slug, data, args)
                total[0] += editions
                total[1] += nominations
        if args.dry_run:
            await tx.rollback()
            log.info("dry-run: откат")
        else:
            await tx.commit()
        log.info("итого: церемоний %d, номинаций %d", *total)
    finally:
        await conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
