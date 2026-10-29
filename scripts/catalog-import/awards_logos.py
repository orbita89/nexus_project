#!/usr/bin/env python3
"""Логотипы премий (`awards.logo_url`) из Wikimedia Commons.

На плитках раздела и в шапке премии вместо логотипа рисуется заглушка: `logo_url` пуст почти у
всех. Здесь он заполняется картинкой премии из Wikidata — элемент премии ищется так же, как в
`awards_ceremonies.py` (по фильмам каталога, QID руками не вписывается).

Что берём и почему с отбором. Свойства идут по убыванию пригодности: `P154` (логотип),
`P2910` (иконка), `P18` (изображение). `P18` у премии — это чаще всего репортажное фото, а не
награда: у «Оскара» там красная дорожка 2009 года, у «Сезара» — портрет актрисы. Поэтому
`P18` принимается, только если имя файла похоже на саму премию (её название, `logo`, `award`,
`statuette`, `trophy`, `prize`). Всё остальное пропускается: лучше честная заглушка, чем чужое
фото в роли логотипа. `P154` и `P2910` берутся как есть — это и есть логотип.

Ссылка ведёт на Commons (`Special:FilePath/<файл>?width=512`) — файл отдаётся по ней напрямую,
копий у себя не держим. На Commons лежат свободные файлы; логотипы под несвободной лицензией
(их часть) туда не попадают, поэтому у некоторых премий логотипа не будет совсем.

Зависимости — `requirements.txt` рядом (aiohttp, asyncpg); venv: `scripts/catalog-import/.venv`.

Запуск:
  DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
  .venv/bin/python awards_logos.py --dry-run

  # только курируемые премии, перезаписать уже проставленные
  DATABASE_URL=... .venv/bin/python awards_logos.py --curated --force

Страницы раздела — статика фронтенда, и скрипт пишет в базу напрямую: после прогона нужна
пересборка статики, иначе логотипы появятся только при следующей правке в админке.
"""

import argparse
import asyncio
import csv
import logging
import os
import re
import sys
from pathlib import Path
from urllib.parse import unquote

import aiohttp
import asyncpg

from awards_ceremonies import PREFIXES, ask, clean, discover, name_key, qid

log = logging.getLogger("awards-logos")

# Сколько премий в одном запросе.
AWARDS_PER_QUERY = 20
# Ширина картинки в ссылке на Commons: плитка премии и шапка.
WIDTH = 512
# Слова, по которым файл из `P18` считается изображением самой награды, а не репортажем.
# Намеренно узкий список: «award» и «prize» стоят в названиях фоторепортажей («Red carpet
# Academy Awards 2009.jpg», «… at the 2016 Film Independent Spirit Awards … .jpg»).
AWARD_WORDS = ("logo", "icon", "emblem", "statuette", "trophy", "symbol", "wordmark")


def images_query(qids: list[str]) -> str:
    """Картинки самой премии и её родителя.

    Найденный элемент — иногда отдельная награда, а не премия целиком («Золотая пальмовая
    ветвь» вместо Каннского фестиваля, «Серебряный медведь» вместо Берлинале). Логотип в таких
    случаях стоит у родителя (`P361` / `P31`), поэтому берём и его.
    """
    values = " ".join(f"wd:{q}" for q in qids)
    return PREFIXES + f"""SELECT ?a ?logo ?icon ?image ?plogo ?picon ?pimage WHERE {{
  VALUES ?a {{ {values} }}
  OPTIONAL {{ ?a wdt:P154 ?logo }}
  OPTIONAL {{ ?a wdt:P2910 ?icon }}
  OPTIONAL {{ ?a wdt:P18 ?image }}
  OPTIONAL {{
    {{ ?a wdt:P361 ?parent }} UNION {{ ?a wdt:P31 ?parent }}
    OPTIONAL {{ ?parent wdt:P154 ?plogo }}
    OPTIONAL {{ ?parent wdt:P2910 ?picon }}
    OPTIONAL {{ ?parent wdt:P18 ?pimage }}
  }}
}}"""


def file_name(url: str) -> str:
    """`…/Special:FilePath/Emmy%20icon.svg` → `Emmy icon.svg`."""
    return unquote((url or "").rsplit("/", 1)[-1])


def looks_like_award(name: str) -> bool:
    """Похоже ли имя файла на логотип премии, а не на репортаж с церемонии."""
    lowered = name_key(name)
    return any(word in lowered for word in AWARD_WORDS)


def pick(row: dict) -> tuple[str, str]:
    """Картинка премии и откуда она. Сначала сама премия, потом её родитель; внутри —
    логотип → иконка → изображение (последнее только если похоже на логотип). ('','') — нет."""
    for key, source in (("logo", "P154"), ("icon", "P2910"),
                        ("plogo", "P154 родителя"), ("picon", "P2910 родителя")):
        if row.get(key):
            return row[key], source
    for key, source in (("image", "P18"), ("pimage", "P18 родителя")):
        image = row.get(key) or ""
        if image and looks_like_award(file_name(image)):
            return image, source
    return "", ""


def commons_url(raw: str) -> str:
    """Ссылка Commons на файл нужной ширины (`Special:FilePath/<файл>?width=`), по https.

    Это официальный стабильный адрес файла: он сам перенаправляет на хранилище и отдаёт
    уменьшенную копию. Серверным клиентам Wikimedia на него отвечает 403 (политика про
    user-agent), браузеру — картинкой; поэтому проверять ссылку надо в браузере, а не curl'ом.
    Если хотлинк окажется ненадёжным, картинки стоит проксировать через свой gateway — там уже
    есть кэш обложек.
    """
    url = file_path_url(raw)
    return f"{url}?width={WIDTH}"


def file_path_url(raw: str) -> str:
    return re.sub(r"^http://", "https://", raw or "")


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database-url", default=os.environ.get("DATABASE_URL"))
    parser.add_argument("--award", action="append", help="только эта премия (slug), можно много")
    parser.add_argument("--curated", action="store_true",
                        help="только курируемые премии (position > 0)")
    parser.add_argument("--force", action="store_true", help="перезаписать уже проставленные")
    parser.add_argument("--cache-dir", default="cache/ceremonies",
                        help="кэш с найденными элементами премий (общий с awards_ceremonies.py)")
    parser.add_argument("--dry-run", action="store_true", help="не записывать, только показать")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    args.refresh = False
    args.no_download = False

    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(levelname)s %(message)s")
    if not args.database_url:
        log.error("нужен DATABASE_URL")
        return 2
    cache = Path(args.cache_dir)
    cache.mkdir(parents=True, exist_ok=True)

    conn = await asyncpg.connect(args.database_url)
    try:
        where = ["true"]
        params: list = []
        if args.award:
            params.append(args.award)
            where.append(f"slug = ANY(${len(params)})")
        if args.curated:
            where.append("position > 0")
        if not args.force:
            where.append("logo_url IS NULL")
        rows = await conn.fetch(
            f"SELECT slug, title FROM awards WHERE {' AND '.join(where)} "
            "ORDER BY position DESC, title", *params)
        log.info("премий без логотипа: %d", len(rows))

        found: dict[str, tuple[str, str]] = {}
        async with aiohttp.ClientSession() as session:
            items = {}
            for row in rows:
                award_qid, title = await discover(session, conn, row["slug"], cache, args)
                if award_qid:
                    items[award_qid] = (row["slug"], title or row["title"])
            qids = list(items)
            # Строк на премию может быть несколько (у родителя свои картинки): берём лучшую.
            for start in range(0, len(qids), AWARDS_PER_QUERY):
                chunk = qids[start:start + AWARDS_PER_QUERY]
                for result in await ask(session, images_query(chunk)):
                    item = qid(result.get("a", ""))
                    if item not in items:
                        continue
                    slug, _ = items[item]
                    url, source = pick(result)
                    if url and (slug not in found or "родителя" in found[slug][1]
                                and "родителя" not in source):
                        found[slug] = (commons_url(url), source)

        for slug, (url, source) in sorted(found.items()):
            log.info("%-24s %-14s %s", slug, source, file_name(url))
            if not args.dry_run:
                await conn.execute("UPDATE awards SET logo_url = $2 WHERE slug = $1", slug, url)
        log.info("%s логотипов: %d из %d премий",
                 "нашлось" if args.dry_run else "проставлено", len(found), len(rows))
        if args.dry_run:
            log.info("dry-run: ничего не записано")
    finally:
        await conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
