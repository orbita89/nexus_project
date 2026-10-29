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

# Курируемые премии: slug, название, страна, год учреждения и типы работ задаются здесь, а не
# выводятся из данных. Ключ — канонический, названия из `metadata.awards[].award` сводятся к
# нему через ALIASES: «Золотой лев» и «награды Венецианского кинофестиваля» — одна премия.
# Всё, чего здесь нет, заводится автоматически (slug транслитом, kinds — по типам работ).
CURATED = {
    # --- главные, по одной на тип работ: вкладки раздела не должны быть пустыми
    "oscar": {"title": "Оскар", "kinds": ["movie"], "country": "США", "founded": 1929,
              "position": 100,
              "full_title": "Премия Американской академии кинематографических искусств и наук"},
    "emmy": {"title": "Эмми", "kinds": ["series"], "country": "США", "founded": 1949,
             "position": 95, "full_title": "Прайм-тайм премия «Эмми»"},
    "hugo": {"title": "Хьюго", "kinds": ["book"], "country": "США", "founded": 1953,
             "position": 90, "full_title": "Премия «Хьюго» за лучшую фантастику"},
    "the-game-awards": {"title": "The Game Awards", "kinds": ["game"], "country": "США",
                        "founded": 2014, "position": 85},
    # --- киноакадемии
    "golden-globe": {"title": "Золотой глобус", "kinds": ["movie", "series"], "country": "США",
                     "founded": 1944, "position": 80},
    "bafta": {"title": "BAFTA", "kinds": ["movie", "series"], "country": "Великобритания",
              "founded": 1949, "position": 78,
              "full_title": "Премия Британской академии кино и телевизионных искусств"},
    "cesar": {"title": "Сезар", "kinds": ["movie"], "country": "Франция", "founded": 1976,
              "position": 70},
    "goya": {"title": "Гойя", "kinds": ["movie"], "country": "Испания", "founded": 1987,
             "position": 68},
    "david-di-donatello": {"title": "Давид ди Донателло", "kinds": ["movie"], "country": "Италия",
                           "founded": 1956, "position": 66},
    "european-film-awards": {"title": "Премия Европейской киноакадемии", "kinds": ["movie"],
                             "country": "Европа", "founded": 1988, "position": 64},
    "japan-academy": {"title": "Премия Японской киноакадемии", "kinds": ["movie"],
                      "country": "Япония", "founded": 1978, "position": 60},
    "aacta": {"title": "AACTA", "kinds": ["movie"], "country": "Австралия", "founded": 1958,
              "position": 58, "full_title": "Премия Австралийской киноакадемии"},
    "nika": {"title": "Ника", "kinds": ["movie"], "country": "Россия", "founded": 1987,
             "position": 56},
    "golden-eagle": {"title": "Золотой орёл", "kinds": ["movie"], "country": "Россия",
                     "founded": 2002, "position": 54},
    # --- фестивали
    "cannes": {"title": "Каннский кинофестиваль", "kinds": ["movie"], "country": "Франция",
               "founded": 1946, "position": 76},
    "venice": {"title": "Венецианский кинофестиваль", "kinds": ["movie"], "country": "Италия",
               "founded": 1932, "position": 74},
    "berlinale": {"title": "Берлинский кинофестиваль", "kinds": ["movie"], "country": "Германия",
                  "founded": 1951, "position": 72},
    "sundance": {"title": "Сандэнс", "kinds": ["movie"], "country": "США", "founded": 1978,
                 "position": 50},
    "locarno": {"title": "Кинофестиваль в Локарно", "kinds": ["movie"], "country": "Швейцария",
                "founded": 1946, "position": 48},
    # --- гильдии: именно они предсказывают «Оскар»
    "sag": {"title": "Премия Гильдии киноактёров США", "kinds": ["movie", "series"],
            "country": "США", "founded": 1995, "position": 46},
    "dga": {"title": "Премия Гильдии режиссёров США", "kinds": ["movie", "series"],
            "country": "США", "founded": 1948, "position": 45},
    "wga": {"title": "Премия Гильдии сценаристов США", "kinds": ["movie", "series"],
            "country": "США", "founded": 1949, "position": 44},
    "pga": {"title": "Премия Гильдии продюсеров США", "kinds": ["movie", "series"],
            "country": "США", "founded": 1990, "position": 43},
    # --- критика
    "critics-choice": {"title": "Critics’ Choice Awards", "kinds": ["movie", "series"],
                       "country": "США", "founded": 1995, "position": 42},
    "independent-spirit": {"title": "Независимый дух", "kinds": ["movie"], "country": "США",
                           "founded": 1984, "position": 41},
    "nbr": {"title": "Премия Национального совета кинокритиков США", "kinds": ["movie"],
            "country": "США", "founded": 1909, "position": 40},
    "nsfc": {"title": "Премия Национального общества кинокритиков США", "kinds": ["movie"],
             "country": "США", "founded": 1966, "position": 39},
    "nyfcc": {"title": "Премия кинокритиков Нью-Йорка", "kinds": ["movie"], "country": "США",
              "founded": 1935, "position": 38},
    "lafca": {"title": "Премия Ассоциации кинокритиков Лос-Анджелеса", "kinds": ["movie"],
              "country": "США", "founded": 1975, "position": 37},
    # --- жанровые и зрительские
    "saturn": {"title": "Сатурн", "kinds": ["movie", "series"], "country": "США",
               "founded": 1973, "position": 36},
    "annie": {"title": "Энни", "kinds": ["movie", "series"], "country": "США", "founded": 1972,
              "position": 35, "full_title": "Премия за достижения в анимации"},
    "nebula": {"title": "Небьюла", "kinds": ["book"], "country": "США", "founded": 1966,
               "position": 34},
    "mtv-movie-awards": {"title": "MTV Movie & TV Awards", "kinds": ["movie", "series"],
                         "country": "США", "founded": 1992, "position": 30},
    "kids-choice": {"title": "Kids’ Choice Awards", "kinds": ["movie", "series"],
                    "country": "США", "founded": 1988, "position": 29},
    "satellite": {"title": "Спутник", "kinds": ["movie", "series"], "country": "США",
                  "founded": 1996, "position": 28},
    # --- антипремия
    "razzie": {"title": "Золотая малина", "kinds": ["movie"], "country": "США", "founded": 1981,
               "position": 20},
}

# Названия из `metadata.awards[].award` → ключ CURATED. Одна премия встречается под разными
# именами (само название, главный приз, «награды такого-то фестиваля»).
ALIASES = {
    "Оскар": "oscar",
    "Прайм-тайм премия «Эмми»": "emmy",
    "Эмми": "emmy",
    "Хьюго": "hugo",
    "The Game Awards": "the-game-awards",
    "Золотой глобус": "golden-globe",
    "BAFTA": "bafta",
    "Сезар": "cesar",
    "Гойя": "goya",
    "Давид ди Донателло": "david-di-donatello",
    "премия Европейской киноакадемии": "european-film-awards",
    "премия Японской киноакадемии": "japan-academy",
    "AACTA": "aacta",
    "Ника": "nika",
    "Золотой орёл": "golden-eagle",
    "главная награда Каннского кинофестиваля": "cannes",
    "Каннский кинофестиваль": "cannes",
    "Золотой лев": "venice",
    "награды Венецианского кинофестиваля": "venice",
    "Золотой медведь": "berlinale",
    "Серебряный медведь": "berlinale",
    "Sundance Festival Awards": "sundance",
    "Сандэнс": "sundance",
    "Награды Международного кинофестиваля в Локарно": "locarno",
    "премия Гильдии киноактёров США": "sag",
    "Премия Гильдии режиссёров Америки": "dga",
    "премия Гильдии сценаристов США": "wga",
    "премия Гильдии продюсеров США": "pga",
    "Producers Guild Film Awards": "pga",
    "Critics’ Choice Movie Awards": "critics-choice",
    "Независимый дух": "independent-spirit",
    "премия Национального совета кинокритиков США": "nbr",
    "премия Национального общества кинокритиков США": "nsfc",
    "премия кинокритиков Нью-Йорка": "nyfcc",
    "премия Ассоциации кинокритиков Лос-Анджелеса": "lafca",
    "Сатурн": "saturn",
    "Энни": "annie",
    "Небьюла": "nebula",
    "MTV Movie & TV Awards": "mtv-movie-awards",
    "Kids’ Choice Awards": "kids-choice",
    "Kids’ Choice Award за любимый анимационный фильм": "kids-choice",
    "Спутник": "satellite",
    "Золотая малина": "razzie",
}

# Не премии: выдвижение страны на «Оскар», отдельные категории, служебные группы Wikidata.
SKIP = {
    "International Submission to the Academy Awards",
    "приз зрительских симпатий",
    "группа наград",
    "премия «Оскар» за лучшую музыку к фильму",
    "LGBT film award",
    # Родовые названия без самой премии (в Wikidata это классы, а не награды).
    "кинофестиваль",
    "кинопремия",
    "премия",
    "премия для актёра или актрисы",
    "награда в области телевидения",
    "награда",
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
        lambda: {"kinds": set(), "years": defaultdict(list), "count": 0, "names": set()}
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
            if name in SKIP:
                skipped += 1
                continue
            key = ALIASES.get(name, name)
            if args.curated_only and key not in CURATED:
                continue
            if args.award and args.award not in (name, key):
                continue
            award = awards[key]
            award.setdefault("names", set()).add(name)
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


async def upsert_award(conn, key: str, data: dict, taken: set[str]) -> tuple:
    """Премия: курируемая — по CURATED, остальные — транслитом названия. Возвращает (id, slug)."""
    curated = CURATED.get(key, {})
    kinds = sorted(curated.get("kinds") or data["kinds"])
    slug = key if curated else None
    title = curated.get("title") or key

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

    slug = slug or unique_slug(slugify(key), taken, "award")
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
    """Заливает премии и церемонии. Возвращает (залитых премий, церемоний, номинаций):
    названия ниже `--min-records` пропускаются и в счёт не идут."""
    taken = {row["slug"] for row in await conn.fetch("SELECT slug FROM awards")}
    total_awards = total_editions = total_nominations = 0
    for key, data in sorted(awards.items(), key=lambda kv: -kv[1]["count"]):
        if data["count"] < args.min_records:
            continue
        total_awards += 1
        award_id, slug = await upsert_award(conn, key, data, taken)
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
        log.info("%s (%s): %d церемоний, %d записей%s", CURATED.get(key, {}).get("title", key),
                 slug, len(data["years"]), data["count"],
                 "" if len(data["names"]) < 2 else f" (названий: {len(data['names'])})")
    return total_awards, total_editions, total_nominations


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database-url", default=os.environ.get("DATABASE_URL"))
    parser.add_argument("--award", help="только эта премия (ключ CURATED или название)")
    parser.add_argument("--curated-only", action="store_true",
                        help="только премии из CURATED")
    parser.add_argument("--since-year", type=int, help="церемонии не раньше этого года")
    parser.add_argument("--min-records", type=int, default=10,
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
        log.info("найдено названий %d; залито премий %d, церемоний %d, номинаций %d",
                 len(awards), *counts)
    finally:
        await conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
