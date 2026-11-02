#!/usr/bin/env python3
"""Самые известные картины из Wikidata в каталог Nexus (раздел «Картины»).

Зачем Wikidata: у неё есть и сами полотна, и художники, и музеи, и изображение в Wikimedia
Commons — всё под свободной лицензией и с русскими названиями. «Топ-100» — по числу языковых
разделов Википедии (`wikibase:sitelinks`): у «Моны Лизы» их больше сотни, у случайного холста —
один-два. Это самый честный доступный прокси известности.

Картина — строка `entities` (kind = 'painting'), своих таблиц нет:

    title           ← русское название (нет — английское)
    original_title  ← английское, если отличается от русского
    description     ← вступление статьи ру-Википедии (до 20 000 символов), иначе описание Wikidata
    release_date    ← год создания P571 (точность до года — 1 января этого года)
    cover_url       ← изображение P18 через Commons `Special:FilePath/<файл>?width=1000`
                      (как логотипы премий, awards_logos.py: стабильный адрес, сам отдаёт копию
                      нужной ширины; у нас картинки не хранятся)
    cover_width,    ← размеры той самой копии (`imageinfo` Commons с `iiurlwidth`): карточка
    cover_height      получает точное `aspect-ratio` до загрузки файла, и стена картин не
                      перестраивается на лету
    metadata        ← {"technique", "museum", "city", "height_cm", "width_cm"}
                      (схема `PaintingMetadata` в `catalog::metadata`)
    external_ids    ← {"wikidata": "Q12418"}; индекс `entities_external_wikidata_uq`
    художник        ← P170 → `people` + `entity_credits` (role = 'artist'), как режиссёр у фильма
    теги            ← жанр P136 (kind = genre) и направление P135 (kind = tag); нет тега —
                      создаётся

Данные берутся в три шага, чтобы ни один запрос не был тяжёлым:
  1. SPARQL (WDQS) — список id картин по убыванию sitelinks;
  2. `wbgetentities` пачками по 50 — заявления и названия самих картин;
  3. `wbgetentities` пачками по 50 — названия того, на что они ссылаются (художник, музей,
     материал, жанр, направление), и `extracts` ру-Википедии для описаний.

Пропускаются: без изображения, без названия, уже записанный qid (повторный прогон безопасен),
не прошедшие ограничения `catalog::metadata` (строки до 200 символов, размеры 0,1–10 000 см).

Запуск:
    python3 -m venv scripts/catalog-import/.venv
    scripts/catalog-import/.venv/bin/pip install -r scripts/catalog-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/catalog-import/.venv/bin/python scripts/catalog-import/paintings_wikidata.py \\
        [--limit 100] [--min-sitelinks 15] [--pool N] [--dry-run] [-v]
Дозаполнить размеры обложек у картин, записанных до того, как они стали сохраняться:
    ... paintings_wikidata.py --fill-sizes
Затем — точечная индексация записанных картин (скрипт печатает готовую команду):
    scripts/index-entities.sh mona-lisa guernica ...
Полная перестройка (scripts/reindex-search.sh) для этого не нужна: она строит оба индекса
целиком и идёт десятки минут.
"""

import argparse
import json
import logging
import os
import re
import sys
import time
import unicodedata
from urllib.parse import quote, unquote, urlparse

import psycopg2
import requests

WDQS = "https://query.wikidata.org/sparql"
WIKIDATA_API = "https://www.wikidata.org/w/api.php"
RUWIKI_API = "https://ru.wikipedia.org/w/api.php"
COMMONS_FILEPATH = "https://commons.wikimedia.org/wiki/Special:FilePath/"
COMMONS_API = "https://commons.wikimedia.org/w/api.php"
COVER_WIDTH = 1000
USER_AGENT = "nexus-catalog-import/1.0 (Nexus dev data import{}) python-requests".format(
    f"; {c}" if (c := os.environ.get("WIKIMEDIA_CONTACT")) else "")

# Ограничения `catalog::metadata` и `catalog::validate` — больше бэкенд не примет.
MAX_TEXT = 200
MAX_TITLE = 300
MAX_DESCRIPTION = 20_000
MIN_SIZE_CM, MAX_SIZE_CM = 0.1, 10_000.0
MAX_SLUG_BASE = 80
BATCH = 50
RETRIES = 5
# Пауза между запросами к API Викимедиа: без неё `wbgetentities` отвечает 429 уже на второй
# пачке. Переопределяется ключом --delay.
DELAY = 1.5

# Свойства Wikidata, которые читаем.
P_IMAGE = "P18"
P_INCEPTION = "P571"
P_CREATOR = "P170"
P_COLLECTION = "P195"
P_LOCATION = "P276"
P_MATERIAL = "P186"
P_GENRE = "P136"
P_MOVEMENT = "P135"
P_HEIGHT = "P2048"
P_WIDTH = "P2049"

# Единицы длины Wikidata → множитель до сантиметров.
UNITS = {"Q174728": 1.0, "Q174789": 0.1, "Q11573": 100.0, "Q218593": 2.54}

log = logging.getLogger("paintings")

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh", "з": "z",
    "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "у": "u", "ф": "f", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh",
    "щ": "shch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
}


def slugify(text: str) -> str:
    """Как `slugify` каталога (и остальных импортов); может быть пустой."""
    text = "".join(TRANSLIT.get(ch, ch) for ch in text.lower())
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].strip("-")


def clean(value: str | None, limit: int) -> str | None:
    value = re.sub(r"\s+", " ", (value or "")).strip()
    return value[:limit] if value else None


# ---------------------------------------------------------------- Wikidata


def http(session: requests.Session, method: str, url: str, delay: float = 0.0, **kwargs) -> dict:
    """Запрос с повторами: у Wikimedia бывают 429 и 503. `delay` — пауза перед запросом."""
    if delay:
        time.sleep(delay)
    for attempt in range(RETRIES + 1):
        try:
            response = session.request(method, url, timeout=120, **kwargs)
            if response.status_code in (429, 500, 502, 503, 504):
                raise RuntimeError(f"HTTP {response.status_code}")
            response.raise_for_status()
            return response.json()
        except (requests.RequestException, ValueError, RuntimeError) as error:
            if attempt == RETRIES:
                raise RuntimeError(f"{url}: {error}") from error
            pause = 10 * (attempt + 1)
            log.warning("%s: %s — повтор через %s с", url, error, pause)
            time.sleep(pause)
    raise RuntimeError("unreachable")


def top_paintings(session: requests.Session, limit: int, min_sitelinks: int) -> list[str]:
    """Id картин по убыванию числа языковых разделов. Только с изображением: без него карточке
    нечего показывать, а искать картинку негде (у фильмов этим занимается `make media`)."""
    sparql = f"""SELECT ?item ?sitelinks WHERE {{
  ?item wdt:P31 wd:Q3305213 ; wdt:P18 ?image ; wikibase:sitelinks ?sitelinks .
  FILTER(?sitelinks >= {min_sitelinks})
}}
ORDER BY DESC(?sitelinks)
LIMIT {limit}"""
    data = http(session, "GET", WDQS, params={"query": sparql, "format": "json"})
    ids = []
    for row in data["results"]["bindings"]:
        qid = row["item"]["value"].rsplit("/", 1)[-1]
        if qid.startswith("Q"):
            ids.append(qid)
    return ids


def entities(session: requests.Session, ids: list[str], props: str, delay: float = DELAY) -> dict:
    """`wbgetentities` пачками по 50 (предел API), с паузой между пачками."""
    found = {}
    for start in range(0, len(ids), BATCH):
        chunk = ids[start:start + BATCH]
        data = http(session, "GET", WIKIDATA_API, delay=delay, params={
            "action": "wbgetentities", "ids": "|".join(chunk), "props": props,
            "languages": "ru|en", "sitefilter": "ruwiki", "format": "json",
        })
        found.update(data.get("entities") or {})
    return found


def label(entity: dict, lang: str) -> str | None:
    return ((entity.get("labels") or {}).get(lang) or {}).get("value")


def claims(entity: dict, prop: str) -> list[dict]:
    return [c for c in (entity.get("claims") or {}).get(prop, [])
            if (c.get("mainsnak") or {}).get("snaktype") == "value"]


def value_of(claim: dict):
    return claim["mainsnak"]["datavalue"]["value"]


def refs(entity: dict, prop: str) -> list[str]:
    """Id сущностей, на которые ссылается свойство (художник, музей, жанр…)."""
    out = []
    for claim in claims(entity, prop):
        value = value_of(claim)
        if isinstance(value, dict) and value.get("entity-type") == "item":
            out.append(value["id"])
    return out


def ref_current(entity: dict, prop: str) -> list[str]:
    """Как `refs`, но сперва заявление с рангом `preferred`, затем самые поздние: у картин с
    историей P195 перечисляет и прежние собрания («Музей в Люксембургском саду» у «Свободы»)."""
    items = [(c.get("rank"), value_of(c)) for c in claims(entity, prop)]
    items = [(rank, v["id"]) for rank, v in items
             if isinstance(v, dict) and v.get("entity-type") == "item"]
    preferred = [qid for rank, qid in items if rank == "preferred"]
    return preferred + [qid for rank, qid in reversed(items) if rank != "preferred"]


def year_of(entity: dict) -> int | None:
    """Год создания P571: в Wikidata это время вида `+1503-01-01T00:00:00Z`, иногда до н. э."""
    for claim in claims(entity, P_INCEPTION):
        value = value_of(claim)
        if not isinstance(value, dict):
            continue
        match = re.match(r"^\+(\d{4})-", value.get("time", ""))
        if match:
            year = int(match.group(1))
            if 1 <= year <= 2100:
                return year
    return None


def size_cm(entity: dict, prop: str) -> float | None:
    """Высота или ширина в сантиметрах; незнакомая единица — пропускаем."""
    for claim in claims(entity, prop):
        value = value_of(claim)
        if not isinstance(value, dict):
            continue
        unit = (value.get("unit") or "").rsplit("/", 1)[-1]
        factor = UNITS.get(unit)
        if factor is None:
            continue
        try:
            size = float(value["amount"]) * factor
        except (KeyError, ValueError):
            continue
        if MIN_SIZE_CM <= size <= MAX_SIZE_CM:
            return round(size, 1)
    return None


def cover_file(entity: dict) -> str | None:
    """Имя файла изображения P18 в Commons."""
    for claim in claims(entity, P_IMAGE):
        name = value_of(claim)
        if isinstance(name, str) and name.strip():
            return name.strip().replace(" ", "_")
    return None


def cover_of(name: str) -> str:
    """Имя файла Commons → адрес копии нужной ширины."""
    return f"{COMMONS_FILEPATH}{quote(name)}?width={COVER_WIDTH}"


def cover_sizes(session: requests.Session, names: list[str],
                delay: float = DELAY) -> dict[str, tuple[int, int]]:
    """Размеры тех самых копий шириной COVER_WIDTH, пачками по 50: `imageinfo` с `iiurlwidth`
    возвращает `thumbwidth`/`thumbheight` — ровно то, что получит браузер. У файла уже этой
    ширины или уже её (панорамы) миниатюры нет, тогда берём размеры оригинала."""
    sizes = {}
    for start in range(0, len(names), BATCH):
        chunk = names[start:start + BATCH]
        data = http(session, "GET", COMMONS_API, delay=delay, params={
            # `thumbwidth`/`thumbheight` приходят только вместе с `iiprop=url`; с одним `size`
            # ответ — размеры оригинала, и в базу попадали бы они, а не скачиваемая копия.
            "action": "query", "prop": "imageinfo", "iiprop": "size|url",
            "iiurlwidth": COVER_WIDTH, "redirects": 1,
            "titles": "|".join(f"File:{name}" for name in chunk), "format": "json",
        })
        for page in ((data.get("query") or {}).get("pages") or {}).values():
            info = (page.get("imageinfo") or [{}])[0]
            width = info.get("thumbwidth") or info.get("width")
            height = info.get("thumbheight") or info.get("height")
            title = (page.get("title") or "").removeprefix("File:").replace(" ", "_")
            if title and isinstance(width, int) and isinstance(height, int) \
                    and width > 0 and height > 0:
                sizes[title] = (width, height)
    return sizes


def ruwiki_titles(entities_by_id: dict) -> dict[str, str]:
    """qid → заголовок статьи ру-Википедии (для описания)."""
    titles = {}
    for qid, entity in entities_by_id.items():
        site = (entity.get("sitelinks") or {}).get("ruwiki")
        if site and site.get("title"):
            titles[qid] = site["title"]
    return titles


def extracts(session: requests.Session, titles: dict[str, str],
             delay: float = DELAY) -> dict[str, str]:
    """Вступления статей ру-Википедии пачками по 20: это описание карточки."""
    by_title = {}
    names = list(titles.values())
    for start in range(0, len(names), 20):
        chunk = names[start:start + 20]
        data = http(session, "GET", RUWIKI_API, delay=delay, params={
            "action": "query", "prop": "extracts", "exintro": 1, "explaintext": 1,
            "redirects": 1, "titles": "|".join(chunk), "format": "json",
        })
        pages = (data.get("query") or {}).get("pages") or {}
        for page in pages.values():
            if page.get("title") and page.get("extract"):
                by_title[page["title"]] = page["extract"]
    return {qid: by_title[title] for qid, title in titles.items() if title in by_title}


def row_of(qid: str, entity: dict, names: dict[str, str], description: str | None,
           sizes: dict[str, tuple[int, int]]) -> dict | None:
    """Строка каталога из записи Wikidata; `None` — нечего показывать."""
    title_ru, title_en = label(entity, "ru"), label(entity, "en")
    title = clean(title_ru or title_en, MAX_TITLE)
    file_name = cover_file(entity)
    if not title or not file_name:
        return None
    cover = cover_of(file_name)
    width, height = sizes.get(file_name, (None, None))
    original = clean(title_en, MAX_TITLE)
    year = year_of(entity)
    museum = next((names[q] for q in ref_current(entity, P_COLLECTION) if q in names), None)
    city = next((names[q] for q in ref_current(entity, P_LOCATION) if q in names), None)
    materials = [names[q] for q in refs(entity, P_MATERIAL) if q in names]
    metadata = {
        "technique": clean(", ".join(materials), MAX_TEXT),
        "museum": clean(museum, MAX_TEXT),
        # P276 часто повторяет музей — тогда город не пишем, пусть поле останется пустым.
        "city": clean(city, MAX_TEXT) if city and city != museum else None,
        "height_cm": size_cm(entity, P_HEIGHT),
        "width_cm": size_cm(entity, P_WIDTH),
    }
    tags = []
    for prop, kind in ((P_GENRE, "genre"), (P_MOVEMENT, "tag")):
        for q in refs(entity, prop):
            name = clean(names.get(q), MAX_TEXT)
            slug = slugify(name or "")
            if name and slug and slug not in {t[0] for t in tags}:
                tags.append((slug, name[:100], kind))
    artists = []
    for q in refs(entity, P_CREATOR):
        name = clean(names.get(q), MAX_TEXT)
        if name:
            artists.append(name)
    return {
        "wikidata": qid,
        "title": title,
        "original_title": original if original and original != title else None,
        "description": clean(description, MAX_DESCRIPTION),
        "release_date": f"{year:04d}-01-01" if year else None,
        "cover_url": cover,
        "cover_width": width,
        "cover_height": height,
        "metadata": {k: v for k, v in metadata.items() if v is not None},
        "tags": tags,
        "artists": artists,
        "year": year,
    }


# ---------------------------------------------------------------- DB


def known_qids(conn) -> set[str]:
    with conn.cursor() as cur:
        cur.execute("SELECT external_ids ->> 'wikidata' FROM entities "
                    "WHERE external_ids ? 'wikidata'")
        return {row[0] for row in cur.fetchall()}


def free_slug(cur, candidates: list[str]) -> str | None:
    cur.execute("SELECT slug FROM entities WHERE slug = ANY(%s)", (candidates,))
    taken = {row[0] for row in cur.fetchall()}
    return next((c for c in candidates if c not in taken), None)


def tag_id(cur, slug: str, name: str, kind: str) -> str:
    cur.execute(
        "INSERT INTO tags (slug, name, kind) VALUES (%s, %s, %s) "
        "ON CONFLICT (slug) DO NOTHING RETURNING id",
        (slug, name, kind),
    )
    row = cur.fetchone()
    if row:
        log.info("tag created: %s (%s)", slug, kind)
        return row[0]
    cur.execute("SELECT id FROM tags WHERE slug = %s", (slug,))
    return cur.fetchone()[0]


def person_id(cur, name: str) -> str | None:
    """Художник: уже известный по имени или новый. Тёзки здесь не разбираем — у картин их
    практически нет, а ручная правка в админке остаётся."""
    cur.execute("SELECT id FROM people WHERE full_name = %s ORDER BY created_at LIMIT 1", (name,))
    row = cur.fetchone()
    if row:
        return row[0]
    base = slugify(name) or "artist"
    slug = free_person_slug(cur, [base] + [f"{base}-{n}" for n in range(2, 12)])
    if not slug:
        return None
    cur.execute("INSERT INTO people (slug, full_name) VALUES (%s, %s) RETURNING id", (slug, name))
    return cur.fetchone()[0]


def free_person_slug(cur, candidates: list[str]) -> str | None:
    cur.execute("SELECT slug FROM people WHERE slug = ANY(%s)", (candidates,))
    taken = {row[0] for row in cur.fetchall()}
    return next((c for c in candidates if c not in taken), None)


def save(conn, row: dict) -> str:
    """Картина, её художники и теги — в одной транзакции: 'written' или 'duplicate'."""
    with conn, conn.cursor() as cur:
        base = slugify(row["original_title"] or row["title"])
        candidates = [c for c in (base, f"{base}-{row['year']}" if base and row["year"] else None)
                      if c]
        candidates.append(f"{base or 'painting'}-{row['wikidata'].lower()}")
        slug = free_slug(cur, candidates)
        if not slug:
            return "duplicate"
        cur.execute(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, cover_width, cover_height, metadata, external_ids)
               VALUES ('painting', %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
               ON CONFLICT DO NOTHING RETURNING id""",
            (
                slug, row["title"], row["original_title"], row["description"],
                row["release_date"], row["cover_url"], row["cover_width"], row["cover_height"],
                json.dumps(row["metadata"], ensure_ascii=False),
                json.dumps({"wikidata": row["wikidata"]}),
            ),
        )
        inserted = cur.fetchone()
        if not inserted:
            return "duplicate"
        entity_id = inserted[0]
        # Slug нужен вызывающему: по нему картину потом досылают в поисковый индекс.
        row["slug"] = slug
        for position, name in enumerate(row["artists"]):
            person = person_id(cur, name)
            if person:
                cur.execute(
                    "INSERT INTO entity_credits (entity_id, person_id, role, position) "
                    "VALUES (%s, %s, 'artist', %s) ON CONFLICT DO NOTHING",
                    (entity_id, person, position),
                )
        for tag_slug, name, kind in row["tags"]:
            cur.execute(
                "INSERT INTO entity_tags (entity_id, tag_id) VALUES (%s, %s) "
                "ON CONFLICT DO NOTHING",
                (entity_id, tag_id(cur, tag_slug, name, kind)),
            )
    return "written"


def fill_sizes(conn, session: requests.Session, delay: float, dry_run: bool,
               every: bool = False) -> int:
    """Дозаполнить `cover_width`/`cover_height` у картин, записанных до того, как размеры стали
    сохраняться (`every` — перечитать у всех, а не только у пустых). Имя файла Commons берётся
    из самого `cover_url`."""
    with conn.cursor() as cur:
        cur.execute("SELECT id, cover_url FROM entities WHERE kind = 'painting' "
                    "AND cover_url LIKE %s" + ("" if every else " AND cover_width IS NULL"),
                    (f"{COMMONS_FILEPATH}%",))
        rows = cur.fetchall()
    by_name: dict[str, list[str]] = {}
    for entity_id, url in rows:
        name = unquote(urlparse(url).path.rsplit("/", 1)[-1])
        by_name.setdefault(name, []).append(entity_id)
    log.info("дозаполнение размеров: %d картин, %d файлов", len(rows), len(by_name))
    sizes = cover_sizes(session, sorted(by_name), delay)
    filled = 0
    for name, ids in by_name.items():
        size = sizes.get(name)
        if not size:
            continue
        filled += len(ids)
        if dry_run:
            continue
        with conn, conn.cursor() as cur:
            cur.execute("UPDATE entities SET cover_width = %s, cover_height = %s "
                        "WHERE id = ANY(%s::uuid[])", (size[0], size[1], ids))
    return filled


# ---------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--limit", type=int, default=100, help="сколько картин записать")
    parser.add_argument("--min-sitelinks", type=int, default=15,
                        help="нижняя граница известности: языковых разделов Википедии")
    parser.add_argument("--pool", type=int, default=None,
                        help="сколько id брать у Wikidata (по умолчанию --limit * 3). Нужно для "
                             "добора: самые известные уже записаны, и окно id надо раздвинуть "
                             "дальше них, не поднимая число записываемых")
    parser.add_argument("--delay", type=float, default=DELAY,
                        help="пауза между запросами к API Викимедиа, с (429 — увеличить)")
    parser.add_argument("--fill-sizes", action="store_true",
                        help="только дозаполнить cover_width/cover_height у записанных картин")
    parser.add_argument("--all", action="store_true",
                        help="с --fill-sizes: перечитать размеры у всех, а не только у пустых")
    parser.add_argument("--dry-run", action="store_true", help="не писать в БД")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )
    dsn = os.environ.get("DATABASE_URL")
    if not dsn and not args.dry_run:
        log.error("DATABASE_URL is not set")
        return 2

    session = requests.Session()
    session.headers["User-Agent"] = USER_AGENT
    conn = psycopg2.connect(dsn) if dsn else None
    if args.fill_sizes:
        if not conn:
            log.error("--fill-sizes требует DATABASE_URL")
            return 2
        try:
            log.info("done: размеры у %d картин", fill_sizes(conn, session, args.delay,
                                                             args.dry_run, args.all))
        except RuntimeError as error:
            log.error("%s", error)
            return 1
        finally:
            conn.close()
        return 0
    known = known_qids(conn) if conn else set()
    stats = {"written": 0, "known": 0, "duplicate": 0, "skipped": 0}
    written_slugs: list[str] = []
    try:
        # С запасом: часть картин уже записана, часть отсеется без названия или изображения.
        ids = top_paintings(session, args.pool or args.limit * 3, args.min_sitelinks)
        log.info("Wikidata: %d картин по известности", len(ids))
        fresh = [qid for qid in ids if qid not in known]
        stats["known"] = len(ids) - len(fresh)
        fresh = fresh[:max(args.limit * 2, args.limit + 100)]
        found = entities(session, fresh, "labels|claims|sitelinks", args.delay)
        # Названия всего, на что ссылаются картины, — одной пачкой на весь прогон.
        linked = sorted({q for entity in found.values()
                         for prop in (P_CREATOR, P_COLLECTION, P_LOCATION, P_MATERIAL,
                                      P_GENRE, P_MOVEMENT)
                         for q in refs(entity, prop)})
        names = {qid: clean(label(e, "ru") or label(e, "en"), MAX_TEXT)
                 for qid, e in entities(session, linked, "labels", args.delay).items()}
        names = {qid: name for qid, name in names.items() if name}
        descriptions = extracts(session, ruwiki_titles(found), args.delay)
        # Размеры превью Commons — одной пачкой на весь прогон, как и названия.
        sizes = cover_sizes(session, sorted({name for entity in found.values()
                                             if (name := cover_file(entity))}), args.delay)
        for qid in fresh:
            if stats["written"] >= args.limit:
                break
            entity = found.get(qid)
            row = row_of(qid, entity, names, descriptions.get(qid), sizes) if entity else None
            if not row:
                stats["skipped"] += 1
                continue
            result = "written" if args.dry_run else save(conn, row)
            stats[result] += 1
            if result == "written" and row.get("slug"):
                written_slugs.append(row["slug"])
            log.debug("%s %s (%s, %s)", result, row["title"], row["year"],
                      row["metadata"].get("museum"))
    except KeyboardInterrupt:
        log.warning("interrupted")
    except RuntimeError as error:
        log.error("%s", error)
        return 1
    finally:
        if conn:
            conn.close()
    log.info("done: %s", stats)
    if written_slugs and not args.dry_run:
        # Точечная индексация: полная перестройка ради сотни картин не нужна.
        log.info("index them: scripts/index-entities.sh %s", " ".join(written_slugs))
    return 0


if __name__ == "__main__":
    sys.exit(main())
