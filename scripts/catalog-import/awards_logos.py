#!/usr/bin/env python3
"""Логотипы премий (`awards.logo_url`) из Wikimedia Commons.

На плитках раздела и в шапке премии вместо логотипа рисуется заглушка: `logo_url` пуст почти у
всех. Здесь он заполняется картинкой премии из Wikidata — элемент премии ищется так же, как в
`awards_ceremonies.py` (по фильмам каталога, QID руками не вписывается).

Если ни `P154`, ни `P2910`, ни подходящего `P18` нет, смотрим категорию премии на Commons
(`P373`) и берём из неё файл, в названии которого есть «logo», «icon» и т. п.: у «Оскара» так
находится `Oscar mini icon.png`.

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
from urllib.parse import quote, unquote

import aiohttp
import asyncpg

from awards_ceremonies import PREFIXES, USER_AGENT, ask, clean, discover, name_key, qid

log = logging.getLogger("awards-logos")

# Сколько премий в одном запросе.
AWARDS_PER_QUERY = 20
# Ширина картинки в ссылке на Commons: плитка премии и шапка.
WIDTH = 512
COMMONS_API = "https://commons.wikimedia.org/w/api.php"
# Пауза между запросами к Commons: на частые он отвечает 429, и щедрая пауза здесь дешевле
# повторного прогона — премий всего сотня.
COMMONS_PAUSE = 3.0
# Слова, по которым файл из `P18` считается изображением самой награды, а не репортажем.
# Намеренно узкий список: «award» и «prize» стоят в названиях фоторепортажей («Red carpet
# Academy Awards 2009.jpg», «… at the 2016 Film Independent Spirit Awards … .jpg»).
AWARD_WORDS = ("logo", "icon", "emblem", "statuette", "trophy", "symbol", "wordmark")


def category_query(qids: list[str]) -> str:
    """Категория премии на Commons (`P373`): в ней ищется логотип, если свойств с картинкой нет."""
    values = " ".join(f"wd:{q}" for q in qids)
    return PREFIXES + f"""SELECT ?a ?cat WHERE {{
  VALUES ?a {{ {values} }}
  ?a wdt:P373 ?cat
}}"""


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
    """Адрес `Special:FilePath`: из Wikidata приходит по http, из категории — только хвостом."""
    url = re.sub(r"^http://", "https://", raw or "")
    if url.startswith("Special:FilePath/"):
        return f"https://commons.wikimedia.org/wiki/{url}"
    return url


async def category_logo(session, category: str, cache: Path) -> str:
    """Файл-логотип в категории Commons: `Oscar mini icon.png` у «Academy Awards». '' — нет.

    Commons щедро отвечает 429 (и просит подождать в `Retry-After`), поэтому список файлов
    категории кладётся в кэш: прерванный прогон продолжается, а не начинается заново.
    """
    path = cache / f"commons-{re.sub(r'[^a-zA-Z0-9]+', '-', category).strip('-')[:80]}.csv"
    files: list[str] = []
    if path.exists():
        with path.open(newline="", encoding="utf-8") as f:
            files = [row["file"] for row in csv.DictReader(f)]
    else:
        params = {"action": "query", "list": "categorymembers",
                  "cmtitle": f"Category:{category}", "cmtype": "file", "cmlimit": "200",
                  "format": "json"}
        data = None
        for attempt in range(4):
            async with session.get(COMMONS_API, params=params) as response:
                if response.status == 200 and response.content_type == "application/json":
                    data = await response.json()
                    break
                wait = response.headers.get("Retry-After")
            await asyncio.sleep(float(wait) if wait and wait.isdigit()
                                else COMMONS_PAUSE * 2 * (attempt + 1))
        if data is None:
            log.warning("Commons не ответил по категории «%s» — пропуск до следующего прогона",
                        category)
            return ""
        files = [member["title"][len("File:"):]
                 for member in data.get("query", {}).get("categorymembers", [])]
        with path.open("w", newline="", encoding="utf-8") as f:
            writer = csv.DictWriter(f, ["file"])
            writer.writeheader()
            writer.writerows({"file": name} for name in files)
    # Порядок слов — по убыванию пригодности: логотип лучше фотографии статуэтки.
    for word in AWARD_WORDS:
        for name in sorted(files):
            if word in name.lower():
                return name
    return ""


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
        # Без user-agent Wikimedia отвечает отказом — для Commons это обязательное поле.
        async with aiohttp.ClientSession(headers={"User-Agent": USER_AGENT}) as session:
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

            # Премии без картинки в свойствах: ищем логотип в их категории на Commons.
            rest = {item: data for item, data in items.items() if data[0] not in found}
            if rest:
                categories = {qid(row.get("a", "")): row.get("cat", "")
                              for row in await ask(session, category_query(list(rest)))}
                for item, (slug, _) in rest.items():
                    category = categories.get(item)
                    if not category:
                        continue
                    name = await category_logo(session, category, cache)
                    await asyncio.sleep(COMMONS_PAUSE)
                    if name:
                        found[slug] = (commons_url(f"Special:FilePath/{quote(name)}"),
                                       "категория Commons")

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
