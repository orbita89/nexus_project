#!/usr/bin/env python3
"""Раздел «Награды» из `entities.metadata.awards`: премии, церемонии, категории, номинации.

Зачем: раздел читает таблицы `awards` / `award_editions` / `award_categories` /
`award_nominations`, а наполнены у нас пока только плоские строчки `metadata.awards`
(их завёл `awards_wikidata.py`: Wikidata P166 и P1411, по id TMDB). Этот скрипт
переносит их в реляционную модель, чтобы раздел не открывался пустым:

  * премия — по `metadata.awards[].award` (slug транслитом, у крупных — вручную, см. CURATED);
  * `kinds` премии — типы работ, у которых она встречается;
  * церемония — год записи (`year`); записи без года пропускаются: год — адрес церемонии;
  * категория — `metadata.awards[].category` (без неё — «Общая категория»);
  * номинация — работа (`entity_id`) и флаг `won`.

Чего в источнике нет: номинации людей (`person_id`), `season_year`, номер и дата церемонии,
описания и логотипы премий — это заполняется в админке руками поверх залитого.

Прогон идемпотентный: церемонии, которые скрипт собирается наполнить, он переписывает целиком
(как `PUT /admin/awards/{slug}/{year}/nominations`). Премии и категории, заведённые руками,
сохраняются: правятся только `kinds` и пустые поля.

Зависимости — `requirements.txt` рядом (asyncpg); venv: `scripts/catalog-import/.venv`.

Запуск:
  DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
  .venv/bin/python awards_build.py --dry-run

  # только крупные премии (CURATED) и последние пять лет — стартовое наполнение
  DATABASE_URL=... .venv/bin/python awards_build.py --curated-only --since-year 2020

После загрузки — переиндексация поиска (`POST /api/v1/catalog/admin/search/reindex` или
`make reindex`): фасеты `award_slugs` / `award_wins` / `award_years` считаются при индексации.
Карточки работ читают награды прямо из PostgreSQL, им хватает загрузки.
"""

import argparse
import asyncio
import json
import logging
import os
import re
import sys
from collections import defaultdict

import asyncpg

log = logging.getLogger("awards-build")

# Крупные премии: slug, название и типы работ фиксируются, а не выводятся из данных —
# по одной на тип работ, чтобы вкладки раздела не были пустыми.
CURATED = {
    "Оскар": {
        "slug": "oscar",
        "title": "Оскар",
        "full_title": "Премия Американской академии кинематографических искусств и наук",
        "kinds": ["movie"],
        "country": "США",
        "founded": 1929,
        "position": 100,
    },
    "Прайм-тайм премия «Эмми»": {
        "slug": "emmy",
        "title": "Эмми",
        "full_title": "Прайм-тайм премия «Эмми»",
        "kinds": ["series"],
        "country": "США",
        "founded": 1949,
        "position": 90,
    },
    "Хьюго": {
        "slug": "hugo",
        "title": "Хьюго",
        "full_title": "Премия «Хьюго» за лучшую фантастику",
        "kinds": ["book"],
        "country": "США",
        "founded": 1953,
        "position": 80,
    },
    "The Game Awards": {
        "slug": "the-game-awards",
        "title": "The Game Awards",
        "kinds": ["game"],
        "country": "США",
        "founded": 2014,
        "position": 70,
    },
    "Золотой глобус": {
        "slug": "golden-globe",
        "title": "Золотой глобус",
        "kinds": ["movie", "series"],
        "country": "США",
        "founded": 1944,
        "position": 60,
    },
    "BAFTA": {
        "slug": "bafta",
        "title": "BAFTA",
        "full_title": "Премия Британской академии кино и телевизионных искусств",
        "kinds": ["movie", "series"],
        "country": "Великобритания",
        "founded": 1949,
        "position": 50,
    },
}

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh",
    "з": "z", "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o",
    "п": "p", "р": "r", "с": "s", "т": "t", "у": "u", "ф": "f", "х": "h", "ц": "ts",
    "ч": "ch", "ш": "sh", "щ": "sch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu",
    "я": "ya",
}

# Те же пределы, что у `catalog::validate`.
MAX_SLUG = 100
MAX_TITLE = 300


def slugify(value: str) -> str:
    """`Лучший фильм` → `luchshiy-film`: латиница в нижнем регистре, цифры, одиночные дефисы."""
    out = "".join(TRANSLIT.get(ch, ch) for ch in value.lower())
    out = re.sub(r"[^a-z0-9]+", "-", out).strip("-")
    out = re.sub(r"-{2,}", "-", out)
    return out[:MAX_SLUG].strip("-")


def unique_slug(base: str, taken: set[str], fallback: str) -> str:
    slug = base or fallback
    if slug not in taken:
        taken.add(slug)
        return slug
    for n in range(2, 1000):
        candidate = f"{slug[: MAX_SLUG - 4]}-{n}"
        if candidate not in taken:
            taken.add(candidate)
            return candidate
    raise RuntimeError(f"no free slug for {base}")


def clean(value, limit: int = MAX_TITLE) -> str | None:
    if not isinstance(value, str):
        return None
    value = value.strip()
    return value[:limit] if value else None


async def collect(conn, args) -> dict:
    """Записи `metadata.awards` из каталога: {премия: {год: [номинация]}} и типы работ премии."""
    rows = await conn.fetch(
        """SELECT e.id, e.kind, e.metadata -> 'awards' AS awards
             FROM entities e
            WHERE jsonb_typeof(e.metadata -> 'awards') = 'array'
              AND jsonb_array_length(e.metadata -> 'awards') > 0"""
    )
    awards: dict[str, dict] = defaultdict(
        lambda: {"kinds": set(), "years": defaultdict(list), "count": 0}
    )
    skipped = 0
    for row in rows:
        items = json.loads(row["awards"]) if isinstance(row["awards"], str) else row["awards"]
        for item in items:
            if not isinstance(item, dict):
                skipped += 1
                continue
            name = clean(item.get("award"))
            year = item.get("year")
            if not name or not isinstance(year, int) or not 1880 <= year <= 2100:
                skipped += 1
                continue
            if args.since_year and year < args.since_year:
                continue
            if args.curated_only and name not in CURATED:
                continue
            if args.award and name != args.award:
                continue
            award = awards[name]
            award["kinds"].add(row["kind"])
            award["count"] += 1
            award["years"][year].append(
                {
                    "entity_id": row["id"],
                    "category": clean(item.get("category")) or "Общая категория",
                    "won": bool(item.get("won")),
                }
            )
    log.info("каталог: %d работ с наградами, %d премий, %d записей пропущено",
             len(rows), len(awards), skipped)
    return awards


async def upsert_award(conn, name: str, data: dict, taken: set[str]) -> tuple:
    """Премия: курируемая — по CURATED, остальные — транслитом названия. Возвращает (id, slug)."""
    curated = CURATED.get(name, {})
    kinds = sorted(curated.get("kinds") or data["kinds"])
    slug = curated.get("slug")
    title = curated.get("title") or name

    row = None
    if slug:
        row = await conn.fetchrow("SELECT id, slug FROM awards WHERE slug = $1", slug)
    if row is None:
        row = await conn.fetchrow("SELECT id, slug FROM awards WHERE title = $1", title)
    if row is not None:
        # Премию могли завести руками: трогаем только типы работ и пустые поля.
        await conn.execute(
            """UPDATE awards SET kinds = $2,
                   full_title = COALESCE(full_title, $3), country = COALESCE(country, $4),
                   founded = COALESCE(founded, $5)
                 WHERE id = $1""",
            row["id"], kinds, curated.get("full_title"), curated.get("country"),
            curated.get("founded"),
        )
        taken.add(row["slug"])
        return row["id"], row["slug"]

    slug = slug or unique_slug(slugify(name), taken, "award")
    taken.add(slug)
    award_id = await conn.fetchval(
        """INSERT INTO awards (slug, title, full_title, kinds, country, founded, position)
           VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id""",
        slug, title[:MAX_TITLE], curated.get("full_title"), kinds,
        curated.get("country"), curated.get("founded"), curated.get("position", 0),
    )
    return award_id, slug


async def categories_of(conn, award_id: int, titles: set[str]) -> dict:
    """Категории премии по названиям: недостающие заводятся, slug — транслитом."""
    rows = await conn.fetch(
        "SELECT id, slug, title FROM award_categories WHERE award_id = $1", award_id
    )
    by_title = {row["title"]: row["id"] for row in rows}
    taken = {row["slug"] for row in rows}
    for position, title in enumerate(sorted(titles)):
        if title in by_title:
            continue
        slug = unique_slug(slugify(title), taken, f"category-{position}")
        by_title[title] = await conn.fetchval(
            """INSERT INTO award_categories (award_id, slug, title, position)
               VALUES ($1, $2, $3, $4) RETURNING id""",
            award_id, slug, title, position,
        )
    return by_title


async def load(conn, awards: dict, args) -> tuple[int, int, int]:
    """Заливает премии и церемонии. Возвращает (премий, церемоний, номинаций)."""
    taken = {row["slug"] for row in await conn.fetch("SELECT slug FROM awards")}
    total_editions = total_nominations = 0
    for name, data in sorted(awards.items(), key=lambda kv: -kv[1]["count"]):
        if data["count"] < args.min_records:
            continue
        award_id, slug = await upsert_award(conn, name, data, taken)
        titles = {n["category"] for year in data["years"].values() for n in year}
        by_title = await categories_of(conn, award_id, titles)

        for year, nominations in sorted(data["years"].items()):
            edition_id = await conn.fetchval(
                """INSERT INTO award_editions (award_id, year) VALUES ($1, $2)
                   ON CONFLICT (award_id, year) DO UPDATE SET year = EXCLUDED.year
                   RETURNING id""",
                award_id, year,
            )
            # Церемония переписывается целиком — как PUT .../nominations.
            await conn.execute("DELETE FROM award_nominations WHERE edition_id = $1", edition_id)
            rows = [
                (edition_id, by_title[n["category"]], n["entity_id"], n["won"], position)
                for position, n in enumerate(
                    sorted(nominations, key=lambda n: (not n["won"], n["category"]))
                )
            ]
            await conn.executemany(
                """INSERT INTO award_nominations (edition_id, category_id, entity_id, won,
                                                  position)
                   VALUES ($1, $2, $3, $4, $5)""",
                rows,
            )
            total_editions += 1
            total_nominations += len(rows)
        log.info("%s (%s): %d церемоний, %d записей", name, slug, len(data["years"]),
                 data["count"])
    return len(awards), total_editions, total_nominations


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database-url", default=os.environ.get("DATABASE_URL"))
    parser.add_argument("--award", help="только эта премия (название из metadata.awards)")
    parser.add_argument("--curated-only", action="store_true",
                        help="только премии из CURATED")
    parser.add_argument("--since-year", type=int, help="церемонии не раньше этого года")
    parser.add_argument("--min-records", type=int, default=1,
                        help="пропускать премии, у которых меньше стольких записей")
    parser.add_argument("--dry-run", action="store_true", help="не записывать, откатить")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(levelname)s %(message)s")
    if not args.database_url:
        log.error("нужен DATABASE_URL")
        return 2

    conn = await asyncpg.connect(args.database_url)
    try:
        tx = conn.transaction()
        await tx.start()
        awards = await collect(conn, args)
        counts = await load(conn, awards, args)
        if args.dry_run:
            await tx.rollback()
            log.info("dry-run: откат")
        else:
            await tx.commit()
        log.info("премий %d, церемоний %d, номинаций %d", *counts)
    finally:
        await conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
