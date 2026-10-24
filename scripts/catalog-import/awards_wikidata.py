#!/usr/bin/env python3
"""Награды и номинации фильмов из Wikidata в `entities.metadata.awards`.

Зачем отдельный скрипт: `awards` заводились только руками через админку, поэтому в каталоге они
есть у одной «Дюны». Источника у наград до сих пор не было — TMDB их не отдаёт; Wikidata отдаёт
и награды (P166), и номинации (P1411), причём с годом церемонии.

Этапы:
  1. download — два SPARQL-запроса к QLever (тот же публичный движок, что у `import_wikidata.py`:
     официальный query.wikidata.org режет такие запросы по таймауту). Фильмы отбираются по
     наличию id TMDB (P4947) — по нему строки и сопоставляются с каталогом. Ответы ложатся
     в CSV в --cache-dir, повторный запуск их переиспользует (--refresh перекачивает).
  2. load — пачками по --batch-size: `metadata.awards` переписывается целиком из Wikidata, но
     записи, которых там нет, сохраняются (их завёл редактор руками — затирать их нельзя).

Год берётся из квалификатора P585 (точка во времени) самого утверждения, иначе — из даты
церемонии (P805 → P585). Награды без года тоже берём: год в карточке необязателен.

Названия — русские (rdfs:label ru), без них — английские. Премия и категория приходят одной
строкой: «Оскар» за лучшую операторскую работу.

Ограничения бэкенда (`catalog::metadata`): не больше 50 записей на фильм, заголовок до 200
символов, год 1880–2100 — здесь они соблюдаются, иначе админка потом не сохранит карточку.

Запуск:
  DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
  python3 awards_wikidata.py --limit 200000 [--dry-run] [--refresh] [--slug oppenheimer-2023]

После загрузки: scripts/reindex-search.sh не нужен — награды в индекс поиска не идут, карточка
читает их из PostgreSQL.
"""

import argparse
import asyncio
import csv
import json
import logging
import os
import sys
from collections import defaultdict
from pathlib import Path

import aiohttp
import asyncpg

QLEVER = "https://qlever.cs.uni-freiburg.de/api/wikidata"
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

log = logging.getLogger("awards")


def query(prop: str) -> str:
    """P166 — награда получена, P1411 — номинация. Год: квалификатор утверждения или церемония."""
    return f"""SELECT ?tmdb ?awardRu ?awardEn ?catRu ?catEn ?year WHERE {{
  ?f wdt:P4947 ?tmdb .
  ?f p:{prop} ?st .
  ?st ps:{prop} ?award .
  OPTIONAL {{ ?award rdfs:label ?awardRu FILTER(LANG(?awardRu) = "ru") }}
  OPTIONAL {{ ?award rdfs:label ?awardEn FILTER(LANG(?awardEn) = "en") }}
  OPTIONAL {{
    ?st pq:P1686 ?work .
    OPTIONAL {{ ?work rdfs:label ?catRu FILTER(LANG(?catRu) = "ru") }}
    OPTIONAL {{ ?work rdfs:label ?catEn FILTER(LANG(?catEn) = "en") }}
  }}
  OPTIONAL {{ ?st pq:P585 ?when }}
  OPTIONAL {{ ?st pq:P805 ?ceremony . ?ceremony wdt:P585 ?ceremonyWhen }}
  BIND(YEAR(COALESCE(?when, ?ceremonyWhen)) AS ?year)
}}"""


async def download(session: aiohttp.ClientSession, endpoint: str, path: Path, sparql: str,
                   refresh: bool, retries: int = 4):
    """Поток CSV в файл; .part → rename, так что в кэше либо полный ответ, либо ничего."""
    if path.exists() and not refresh:
        log.info("%s уже скачан", path.name)
        return
    tmp = path.with_suffix(".part")
    for attempt in range(retries + 1):
        try:
            async with session.post(endpoint, data={"query": PREFIXES + sparql},
                                    headers={"Accept": "text/csv"}) as resp:
                if resp.status != 200:
                    body = (await resp.text())[:500]
                    raise RuntimeError(f"{path.name}: HTTP {resp.status}: {body}")
                with tmp.open("wb") as f:
                    async for chunk in resp.content.iter_chunked(1 << 16):
                        f.write(chunk)
            tmp.replace(path)
            log.info("%s скачан (%.1f МБ)", path.name, path.stat().st_size / 1024 / 1024)
            return
        except (aiohttp.ClientError, asyncio.TimeoutError, RuntimeError) as e:
            tmp.unlink(missing_ok=True)
            if attempt == retries:
                raise
            delay = 5 * (attempt + 1)
            log.warning("%s: %s — повтор через %s с", path.name, e, delay)
            await asyncio.sleep(delay)


def title_of(row: dict) -> str:
    """«Оскар» + категория одной строкой; без русского названия — английское."""
    award = (row.get("awardRu") or row.get("awardEn") or "").strip()
    category = (row.get("catRu") or row.get("catEn") or "").strip()
    if not award:
        return ""
    title = f"{award}: {category}" if category else award
    return title[:MAX_TITLE]


def year_of(row: dict):
    raw = (row.get("year") or "").strip()
    if not raw:
        return None
    try:
        year = int(raw)
    except ValueError:
        return None
    return year if MIN_YEAR <= year <= MAX_YEAR else None


def collect(paths: dict[str, Path]) -> dict[str, list[dict]]:
    """CSV → {tmdb_id: [{title, year, won}, ...]}, полученные сверху, внутри — новые по году."""
    by_film: dict[str, dict[tuple, dict]] = defaultdict(dict)
    for won, path in (("won", paths["received"]), ("nominated", paths["nominated"])):
        if not path.exists():
            continue
        with path.open(newline="", encoding="utf-8") as f:
            for row in csv.DictReader(f):
                tmdb = (row.get("tmdb") or "").strip()
                title = title_of(row)
                if not tmdb or not title:
                    continue
                year = year_of(row)
                key = (title.lower(), year)
                award = {"title": title, "year": year, "won": won == "won"}
                # Победа важнее номинации: одна и та же строка не должна остаться номинацией.
                if key not in by_film[tmdb] or award["won"]:
                    by_film[tmdb][key] = award
    result = {}
    for tmdb, awards in by_film.items():
        ordered = sorted(awards.values(),
                         key=lambda a: (not a["won"], -(a["year"] or 0), a["title"]))
        result[tmdb] = ordered[:MAX_AWARDS]
    return result


def merge(existing: list, fresh: list[dict]) -> list[dict]:
    """Ручные записи редактора сохраняются: их в Wikidata нет, затирать нельзя."""
    keys = {(a["title"].lower(), a.get("year")) for a in fresh}
    manual = [a for a in existing or []
              if isinstance(a, dict) and a.get("title")
              and (a["title"].lower(), a.get("year")) not in keys]
    return (manual + fresh)[:MAX_AWARDS]


async def load(conn: asyncpg.Connection, awards: dict[str, list[dict]], args) -> tuple[int, int]:
    """Пишет `metadata.awards` пачками. Возвращает (обновлено, пропущено без изменений)."""
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
        log.info("записано %d / %d", written, len(updates))
    return written, unchanged


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--endpoint", default=QLEVER)
    parser.add_argument("--cache-dir", default="cache/awards")
    parser.add_argument("--batch-size", type=int, default=500)
    parser.add_argument("--refresh", action="store_true", help="перекачать CSV, даже если есть")
    parser.add_argument("--dry-run", action="store_true", help="ничего не писать в БД")
    parser.add_argument("--slug", help="только этот фильм — для проверки")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        log.error("нужен DATABASE_URL")
        return 2

    cache = Path(args.cache_dir)
    cache.mkdir(parents=True, exist_ok=True)
    paths = {"received": cache / "awards_received.csv", "nominated": cache / "awards_nominated.csv"}

    timeout = aiohttp.ClientTimeout(total=1800)
    async with aiohttp.ClientSession(timeout=timeout, headers={"User-Agent": USER_AGENT}) as s:
        await download(s, args.endpoint, paths["received"], query("P166"), args.refresh)
        await download(s, args.endpoint, paths["nominated"], query("P1411"), args.refresh)

    awards = collect(paths)
    log.info("наград и номинаций: %d фильмов", len(awards))

    conn = await asyncpg.connect(database_url)
    try:
        written, unchanged = await load(conn, awards, args)
    finally:
        await conn.close()
    log.info("готово: обновлено %d, без изменений %d", written, unchanged)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
