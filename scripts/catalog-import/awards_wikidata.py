#!/usr/bin/env python3
"""Награды и номинации фильмов из Wikidata в `entities.metadata.awards`.

Зачем: `awards` заводились только руками через админку, поэтому в каталоге они были у одной
«Дюны». TMDB награды не отдаёт; Wikidata отдаёт и награды (P166), и номинации (P1411), с годом
церемонии и с семейством премии («Оскар», BAFTA, «Золотой глобус») — по нему на сайте работает
вкладка «Награды».

Каждая запись: `{award, category, title, year, won}`:
  * `award` — премия: «Оскар», BAFTA, «Золотой глобус» (P361 награды, иначе P31; служебные
    классы вроде «категория кинопремии» отбрасываются). По ней идёт выбор премии во вкладке;
  * `category` — номинация внутри премии: «за лучший фильм» — остаток названия без имени премии;
  * `title` — полное название одной строкой, как показывать в карточке;
  * `year` — квалификатор P585 утверждения, иначе дата церемонии (P805 → P585);
  * `won` — `true` у P166, `false` у P1411.

Этапы:
  1. download — SPARQL пачками по id TMDB из нашей же базы (`VALUES`), по --batch-films штук.
     Запрос всегда маленький: работает на официальном WDQS с его таймаутом 60 с и не зависит от
     QLever, который регулярно лежит (502). Каждая пачка — свой CSV в --cache-dir, поэтому
     прогон после обрыва продолжается с места остановки (--refresh перекачивает всё).
  2. load — пачками по --batch-size: `metadata.awards` переписывается из Wikidata, но записи,
     которых там нет, сохраняются: их завёл редактор руками.

Ограничения бэкенда (`catalog::metadata`): не больше 50 записей на фильм, строки до 200
символов, год 1880–2100 — здесь они соблюдаются, иначе админка потом не сохранит карточку.

Зависимости — `requirements.txt` рядом (aiohttp, asyncpg); venv: `scripts/catalog-import/.venv`
(в `.gitignore`), создаётся `python3 -m venv .venv && .venv/bin/pip install -r requirements.txt`.

Запуск (проверка на одном фильме — секунды):
  DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
  .venv/bin/python awards_wikidata.py --slug oppenheimer-2023 --dry-run --verbose

Весь каталог (популярные первыми; прогон можно прерывать и продолжать):
  DATABASE_URL=... .venv/bin/python awards_wikidata.py [--limit 20000]

После загрузки: `POST /api/v1/catalog/admin/awards/sync` — чтобы в таблице `awards` появились
новые премии для вкладки. В индекс поиска награды не идут: карточка читает их из PostgreSQL.
"""

import argparse
import asyncio
import csv
import json
import logging
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

import aiohttp
import asyncpg

WDQS = "https://query.wikidata.org/sparql"
USER_AGENT = "nexus-catalog-import/1.0 (Nexus dev data import{}) python-aiohttp".format(
    f"; {c}" if (c := os.environ.get("WIKIMEDIA_CONTACT")) else "")

PREFIXES = """PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX p: <http://www.wikidata.org/prop/>
PREFIX ps: <http://www.wikidata.org/prop/statement/>
PREFIX pq: <http://www.wikidata.org/prop/qualifier/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
"""

# Ограничения `catalog::metadata` — больше бэкенд не примет.
MAX_AWARDS = 50
MAX_TITLE = 200
MIN_YEAR, MAX_YEAR = 1880, 2100

# Служебные классы Wikidata: это тип записи, а не премия. В `award` они попасть не должны.
GENERIC_FAMILIES = {
    "категория кинопремии", "film award category", "степень награды", "class of award",
    "кинопремия", "film award", "награда", "award", "премия", "категория награды",
    "award category", "телевизионная премия", "television award", "премия в области искусства",
    # P31 иногда ведёт на отношение, а не на премию: «подчинение» у заявок на «Оскар».
    "подчинение", "subordination", "номинация", "nomination", "submission", "заявка",
    "кандидатура", "candidacy", "церемония награждения", "award ceremony",
}

log = logging.getLogger("awards")


def query(prop: str, tmdb_ids: list[str]) -> str:
    """P166 — награда получена, P1411 — номинация. Пачка фильмов — через VALUES."""
    values = " ".join(f'"{i}"' for i in tmdb_ids)
    return f"""SELECT ?tmdb ?awardRu ?awardEn ?famRu ?famEn ?rel ?year WHERE {{
  VALUES ?tmdb {{ {values} }}
  ?f wdt:P4947 ?tmdb .
  ?f p:{prop} ?st .
  ?st ps:{prop} ?award .
  OPTIONAL {{ ?award rdfs:label ?awardRu FILTER(LANG(?awardRu) = "ru") }}
  OPTIONAL {{ ?award rdfs:label ?awardEn FILTER(LANG(?awardEn) = "en") }}
  OPTIONAL {{
    {{ ?award wdt:P361 ?fam BIND("P361" AS ?rel) }}
    UNION
    {{ ?award wdt:P31 ?fam BIND("P31" AS ?rel) }}
    OPTIONAL {{ ?fam rdfs:label ?famRu FILTER(LANG(?famRu) = "ru") }}
    OPTIONAL {{ ?fam rdfs:label ?famEn FILTER(LANG(?famEn) = "en") }}
  }}
  OPTIONAL {{ ?st pq:P585 ?when }}
  OPTIONAL {{ ?st pq:P805 ?ceremony . ?ceremony wdt:P585 ?ceremonyWhen }}
  BIND(YEAR(COALESCE(?when, ?ceremonyWhen)) AS ?year)
}}"""


async def fetch(session: aiohttp.ClientSession, endpoint: str, sparql: str, path: Path,
                retries: int = 3) -> bool:
    """Поток CSV в файл; .part → rename, так что в кэше либо полный ответ, либо ничего."""
    tmp = path.with_suffix(".part")
    for attempt in range(retries + 1):
        try:
            async with session.post(endpoint, data={"query": PREFIXES + sparql},
                                    headers={"Accept": "text/csv"}) as resp:
                if resp.status != 200:
                    body = (await resp.text())[:200].replace("\n", " ")
                    raise RuntimeError(f"HTTP {resp.status}: {body}")
                with tmp.open("wb") as f:
                    async for chunk in resp.content.iter_chunked(1 << 16):
                        f.write(chunk)
            tmp.replace(path)
            return True
        except (aiohttp.ClientError, asyncio.TimeoutError, RuntimeError) as e:
            tmp.unlink(missing_ok=True)
            if attempt == retries:
                log.error("%s: %s — пачка пропущена", path.name, e)
                return False
            delay = 5 * (attempt + 1)
            log.warning("%s: %s — повтор через %s с", path.name, e, delay)
            await asyncio.sleep(delay)
    return False


def strip_award_prefix(name: str) -> str:
    """«премия BAFTA» → «BAFTA»: слово «премия» в начале — шум в списке премий.

    Срезаем только перед одним словом: у «премия Европейской киноакадемии» остаток стоит в
    родительном падеже («Европейской киноакадемии»), и без «премии» он читается как обрывок.
    """
    rest = re.sub(r"^(премия|награда)\s+", "", name, flags=re.IGNORECASE).strip()
    return rest if rest and len(rest.split()) == 1 else name.strip()


def is_category(name: str) -> bool:
    """«за лучшую мужскую роль» — это категория, а не премия: у части наград P31 ведёт на неё,
    а не на «Оскар». Такие названия в список премий пускать нельзя."""
    lowered = name.lower()
    return (lowered in GENERIC_FAMILIES
            or lowered.startswith(("за ", "award for", "prize for", "премия за")))


def family_of(rows: list[dict]) -> str:
    """Семейство премии: P361 («часть чего-либо») точнее P31 — у «Оскара» за лучший фильм P31
    это «премия за лучший фильм», то есть категория, а P361 — сам «Оскар». Поэтому сначала
    P361, и только если его нет — P31; служебные классы отбрасываются, русское название
    предпочтительнее английского."""
    for rel in ("P361", "P31"):
        for key in ("famRu", "famEn"):
            for row in rows:
                if (row.get("rel") or "") != rel:
                    continue
                name = strip_award_prefix((row.get(key) or "").strip())
                if name and not is_category(name):
                    return name[:MAX_TITLE]
    return ""


def category_of(title: str, family: str) -> str:
    """«премия «Оскар» за лучший фильм» + «Оскар» → «за лучший фильм»."""
    if not family:
        return ""
    cleaned = re.sub(r"[«»\"]", "", title)
    needle = re.sub(r"[«»\"]", "", family)
    index = cleaned.lower().find(needle.lower())
    if index < 0:
        return ""
    return cleaned[index + len(needle):].strip(" -–—:,")[:MAX_TITLE]


def year_of(raw: str | None):
    raw = (raw or "").strip()
    if not raw:
        return None
    try:
        year = int(raw)
    except ValueError:
        return None
    return year if MIN_YEAR <= year <= MAX_YEAR else None


def collect(cache: Path) -> dict[str, list[dict]]:
    """CSV всех пачек → {tmdb_id: [{award, category, title, year, won}, ...]}."""
    # Одна награда приходит несколькими строками: у неё несколько P361/P31. Сначала группируем.
    grouped: dict[tuple, list[dict]] = defaultdict(list)
    for path in sorted(cache.glob("*.csv")):
        won = "received" in path.name
        with path.open(newline="", encoding="utf-8") as f:
            for row in csv.DictReader(f):
                tmdb = (row.get("tmdb") or "").strip()
                title = (row.get("awardRu") or row.get("awardEn") or "").strip()[:MAX_TITLE]
                if not tmdb or not title:
                    continue
                grouped[(tmdb, title, year_of(row.get("year")), won)].append(row)

    by_film: dict[str, dict[tuple, dict]] = defaultdict(dict)
    for (tmdb, title, year, won), rows in grouped.items():
        family = family_of(rows)
        key = (title.lower(), year)
        # Победа важнее номинации: одна и та же строка не должна остаться номинацией.
        if key not in by_film[tmdb] or won:
            by_film[tmdb][key] = {
                "award": family or title,
                "category": category_of(title, family),
                "title": title,
                "year": year,
                "won": won,
            }

    result = {}
    for tmdb, awards in by_film.items():
        result[tmdb] = sorted(
            awards.values(),
            key=lambda a: (not a["won"], -(a["year"] or 0), a["title"]))[:MAX_AWARDS]
    return result


def merge(existing: list, fresh: list[dict]) -> list[dict]:
    """Ручные записи редактора сохраняются: их в Wikidata нет, затирать нельзя."""
    keys = {(a["title"].lower(), a.get("year")) for a in fresh}
    manual = [a for a in existing or []
              if isinstance(a, dict) and a.get("title")
              and (a["title"].lower(), a.get("year")) not in keys]
    return (manual + fresh)[:MAX_AWARDS]


async def download(session, args, tmdb_ids: list[str], cache: Path) -> int:
    """Пачки по --batch-films: два запроса на пачку (награды и номинации)."""
    batches = [tmdb_ids[i:i + args.batch_films]
               for i in range(0, len(tmdb_ids), args.batch_films)]
    done = 0
    for n, batch in enumerate(batches, start=1):
        for prop, kind in (("P166", "received"), ("P1411", "nominated")):
            path = cache / f"batch_{n:05d}_{kind}.csv"
            if path.exists() and not args.refresh:
                continue
            done += int(await fetch(session, args.endpoint, query(prop, batch), path))
            # Вежливость к публичному эндпоинту: он один на всех.
            await asyncio.sleep(args.delay)
        if n % 10 == 0 or n == len(batches):
            log.info("пачка %d / %d", n, len(batches))
    return done


async def load(conn: asyncpg.Connection, awards: dict[str, list[dict]], args) -> tuple[int, int]:
    """Пишет `metadata.awards` пачками. Возвращает (обновлено, без изменений)."""
    where = "WHERE external_ids ? 'tmdb'"
    params: list = []
    if args.slug:
        where += " AND slug = $1"
        params.append(args.slug)
    rows = await conn.fetch(
        f"SELECT id, slug, external_ids->>'tmdb' AS tmdb, metadata FROM entities {where}", *params)

    updates: list[tuple] = []
    unchanged = 0
    for row in rows:
        fresh = awards.get(row["tmdb"] or "")
        if not fresh:
            continue
        metadata = json.loads(row["metadata"]) if isinstance(row["metadata"], str) \
            else dict(row["metadata"] or {})
        merged = merge(metadata.get("awards"), fresh)
        if metadata.get("awards") == merged:
            unchanged += 1
            continue
        metadata["awards"] = merged
        updates.append((row["id"], json.dumps(metadata, ensure_ascii=False)))
        if args.verbose:
            log.info("%s: %d наград", row["slug"], len(merged))

    if args.dry_run:
        log.info("dry-run: обновилось бы %d фильмов, без изменений %d", len(updates), unchanged)
        return len(updates), unchanged

    written = 0
    for start in range(0, len(updates), args.batch_size):
        batch = updates[start:start + args.batch_size]
        async with conn.transaction():
            await conn.execute(
                """UPDATE entities e SET metadata = u.m::jsonb, updated_at = now()
                   FROM unnest($1::uuid[], $2::text[]) AS u(id, m)
                   WHERE e.id = u.id""",
                [u[0] for u in batch], [u[1] for u in batch])
        written += len(batch)
    if written:
        log.info("записано %d фильмов", written)
    return written, unchanged


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--endpoint", default=WDQS)
    parser.add_argument("--cache-dir", default="cache/awards")
    parser.add_argument("--batch-films", type=int, default=400, help="фильмов в одном запросе")
    parser.add_argument("--batch-size", type=int, default=500, help="строк в одной транзакции")
    parser.add_argument("--delay", type=float, default=1.0, help="пауза между запросами, с")
    parser.add_argument("--refresh", action="store_true", help="перекачать CSV, даже если есть")
    parser.add_argument("--dry-run", action="store_true", help="ничего не писать в БД")
    parser.add_argument("--slug", help="только этот фильм — для проверки")
    parser.add_argument("--limit", type=int, help="сколько фильмов брать (популярные первыми)")
    parser.add_argument("--skip-download", action="store_true", help="только разбор кэша и запись")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        log.error("нужен DATABASE_URL")
        return 2

    conn = await asyncpg.connect(database_url)
    if args.slug:
        tmdb_id = await conn.fetchval(
            "SELECT external_ids->>'tmdb' FROM entities WHERE slug = $1", args.slug)
        if not tmdb_id:
            log.error("%s: нет такого фильма или у него нет id TMDB", args.slug)
            await conn.close()
            return 2
        tmdb_ids = [tmdb_id]
    else:
        # Популярные первыми: прогон можно прервать, и в каталоге уже будет самое заметное.
        limit = f"LIMIT {int(args.limit)}" if args.limit else ""
        tmdb_ids = [r["tmdb"] for r in await conn.fetch(
            f"""SELECT e.external_ids->>'tmdb' AS tmdb FROM entities e
                LEFT JOIN entity_stats s ON s.entity_id = e.id
                WHERE e.kind = 'movie' AND e.external_ids ? 'tmdb'
                ORDER BY s.tmdb_popularity DESC NULLS LAST, e.id {limit}""")]
    log.info("фильмов: %d", len(tmdb_ids))

    cache = Path(args.cache_dir) / (args.slug or "all")
    cache.mkdir(parents=True, exist_ok=True)
    if not args.skip_download:
        timeout = aiohttp.ClientTimeout(total=300)
        async with aiohttp.ClientSession(timeout=timeout,
                                         headers={"User-Agent": USER_AGENT}) as session:
            await download(session, args, tmdb_ids, cache)

    awards = collect(cache)
    log.info("награды нашлись у %d фильмов", len(awards))

    try:
        written, unchanged = await load(conn, awards, args)
    finally:
        await conn.close()
    log.info("готово: обновлено %d, без изменений %d", written, unchanged)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
