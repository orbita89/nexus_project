#!/usr/bin/env python3
"""Самые популярные игры из Wikidata в каталог Nexus (раздел «Игры»).

Зачем Wikidata, а не Steam: магазин Steam и его CDN отсюда закрыты корпоративным фильтром
(`steam_games.py` упирается в 403), а у Wikidata есть и сами игры, и разработчики, и жанры,
и русские названия. Обложку даёт англ. Википедия: в карточке статьи об игре стоит её бокс-арт
(`upload.wikimedia.org`), запасной вариант — изображение P18 или логотип P154 из Commons.

«Популярные на данный момент» — это не sitelinks (так Minecraft и Pac-Man равны навсегда),
а просмотры статей о них в англ. Википедии за два последних полных месяца
(Wikimedia Pageviews). Отбор в два шага: SPARQL берёт пул кандидатов по убыванию sitelinks (широкая сеть
известных игр), просмотры из этого пула выбирают --limit самых читаемых сейчас.

Игра — строка `entities` (kind = 'game'), своих таблиц нет:

    title           ← русское название (нет — английское)
    original_title  ← английское, если отличается от русского
    description     ← вступление статьи ру-Википедии, иначе англ. (до 20 000 символов)
    release_date    ← самая ранняя дата выхода P577 (точность до года — 1 января)
    cover_url       ← бокс-арт из карточки англ. статьи через `imageinfo` (копия шириной 600),
                      иначе P18/P154 из Commons; у нас картинки не хранятся
    cover_width,    ← размеры той самой копии: карточка получает точное `aspect-ratio`
    cover_height      до загрузки файла
    metadata        ← {"platforms", "developer", "publisher"} (схема `GameMetadata`)
    external_ids    ← {"wikidata": "Q17452"} и {"steam": appid} из P1733, если он есть
                      (индексы `entities_external_wikidata_uq`, `entities_external_steam_uq`)
    теги            ← жанры P136 по известным id (`GENRES`); нет тега — создаётся

Пропускаются: без названия, без обложки, уже записанный qid (повторный прогон безопасен).
Трейлеры не пишутся — их добавит `make media` (Rutube).

Запуск:
    python3 -m venv scripts/games-import/.venv
    scripts/games-import/.venv/bin/pip install -r scripts/games-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/games-import/.venv/bin/python scripts/games-import/wikidata_games.py \\
        [--limit 100] [--pool 500] [--min-sitelinks 8] [--delay 1.5] [--dry-run] [-v]
Затем — точечная индексация записанных игр (скрипт печатает готовую команду):
    scripts/index-entities.sh elden-ring minecraft ...
"""

import argparse
import json
import logging
import os
import re
import sys
import time
import unicodedata
from datetime import date, timedelta
from urllib.parse import quote

import psycopg2
import requests

WDQS = "https://query.wikidata.org/sparql"
WIKIDATA_API = "https://www.wikidata.org/w/api.php"
ENWIKI_API = "https://en.wikipedia.org/w/api.php"
RUWIKI_API = "https://ru.wikipedia.org/w/api.php"
COMMONS_API = "https://commons.wikimedia.org/w/api.php"
COMMONS_FILEPATH = "https://commons.wikimedia.org/wiki/Special:FilePath/"
PAGEVIEWS = ("https://wikimedia.org/api/rest_v1/metrics/pageviews/per-article"
             "/{project}/all-access/user/{title}/monthly/{start}/{end}")
COVER_WIDTH = 600
USER_AGENT = "nexus-catalog-import/1.0 (Nexus dev data import{}) python-requests".format(
    f"; {c}" if (c := os.environ.get("WIKIMEDIA_CONTACT")) else "")

# Ограничения `catalog::metadata` и `catalog::validate` — больше бэкенд не примет.
MAX_TEXT = 200
MAX_TITLE = 300
MAX_DESCRIPTION = 20_000
MAX_SLUG_BASE = 80
MAX_PLATFORMS = 12
BATCH = 50
RETRIES = 5
# Пауза между запросами к API Викимедиа: без неё `wbgetentities` отвечает 429 уже на второй
# пачке. Переопределяется ключом --delay. У Pageviews лимит мягче — своя пауза.
DELAY = 1.5
VIEWS_DELAY = 1.0

Q_VIDEO_GAME = "Q7889"
P_PUBLICATION = "P577"
P_DEVELOPER = "P178"
P_PUBLISHER = "P123"
P_GENRE = "P136"
P_PLATFORM = "P400"
P_STEAM = "P1733"
P_IMAGE = "P18"
P_LOGO = "P154"

# Жанр Wikidata → наш тег (slug, название, kind). Незнакомый жанр пропускается: в P136 попадают
# и «компьютерная игра с ЛГБТ-персонажем», и прочее, чему в списке жанров каталога не место.
GENRES = {
    "Q343568": ("action-adventure", "Приключенческий экшен", "genre"),
    "Q828322": ("platformer", "Платформер", "genre"),
    "Q185029": ("fps", "Шутер от первого лица", "genre"),
    "Q380266": ("tps", "Шутер от третьего лица", "genre"),
    "Q744038": ("rpg", "Ролевая игра", "genre"),
    "Q1422746": ("action-rpg", "Action RPG", "genre"),
    "Q270948": ("action", "Боевик", "genre"),
    "Q860750": ("racing", "Гонки", "genre"),
    "Q23916": ("adventure", "Приключения", "genre"),
    "Q54767": ("puzzle", "Головоломка", "genre"),
    "Q208189": ("rts", "Стратегия в реальном времени", "genre"),
    "Q2176159": ("turn-based-strategy", "Пошаговая стратегия", "genre"),
    "Q472055": ("strategy", "Стратегия", "genre"),
    "Q333967": ("survival-horror", "Survival horror", "genre"),
    "Q846224": ("fighting", "Файтинг", "genre"),
    "Q1044478": ("shmup", "Shoot 'em up", "genre"),
    "Q1610017": ("simulator", "Симулятор", "genre"),
    "Q1478420": ("sport", "Спорт", "genre"),
    "Q689445": ("visual-novel", "Визуальная новелла", "genre"),
    "Q1163960": ("hack-and-slash", "Hack and slash", "genre"),
    "Q858523": ("stealth", "Стелс", "genre"),
    "Q862490": ("mmo", "MMO", "tag"),
    "Q27670585": ("sci-fi", "Фантастика", "tag"),
    "Q115101598": ("post-apocalyptic", "Постапокалипсис", "tag"),
}

# Платформа Wikidata → значение `metadata.platforms`. Незнакомая — из английского названия
# (`slugify`), чтобы новая консоль не пропадала молча.
PLATFORMS = {
    "Q1406": "pc", "Q14116": "macos", "Q388": "linux", "Q170434": "dos",
    "Q94": "android", "Q48493": "xbox-360", "Q48263": "xbox", "Q19610114": "xbox-one",
    "Q98973368": "xbox-series", "Q13361286": "switch", "Q10677": "ps1", "Q10680": "ps2",
    "Q10683": "ps3", "Q5014725": "ps4", "Q63184502": "ps5", "Q170325": "psp",
    "Q188642": "ps-vita", "Q8079": "wii", "Q56942": "wii-u", "Q182172": "gamecube",
    "Q170323": "nintendo-ds", "Q132020": "game-boy-advance", "Q186437": "game-boy",
    "Q172742": "nes", "Q192851": "arcade",
}

# Одна и та же платформа приходит под разными названиями (у игры — «PlayStation Vita»,
# у другой — «PS Vita»): slug от названия сводится к общему значению фильтра.
PLATFORM_ALIASES = {
    "personal-computer": "pc", "mac-operating-system": "macos", "classic-mac-os": "macos",
    "ms-dos": "dos", "playstation-vita": "ps-vita", "web-browser": "browser",
    "chromebook": "chromeos", "java-virtual-machine": "java", "java-platform": "java",
    "super-nintendo-entertainment-system": "snes", "new-nintendo-3ds": "nintendo-3ds",
    "nintendo-switch-lite": "switch", "nintendo-switch-2": "switch-2",
    "commodore-amiga": "amiga", "sega-genesis-mega-drive": "sega-mega-drive",
    "shield-portable": "nvidia-shield", "shield-tablet": "nvidia-shield",
    "nvidia-shield-tv": "nvidia-shield",
}

log = logging.getLogger("games")

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
    text = re.sub(r"['’.]", "", text)
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].strip("-")


def clean(value: str | None, limit: int) -> str | None:
    value = re.sub(r"[ \t]+", " ", (value or "")).strip()
    return value[:limit].strip() if value else None


# ---------------------------------------------------------------- HTTP


def http(session: requests.Session, url: str, delay: float = 0.0, allow_404: bool = False,
         **kwargs) -> dict | None:
    """Запрос с повторами: у Wikimedia бывают 429 и 503. `delay` — пауза перед запросом."""
    if delay:
        time.sleep(delay)
    for attempt in range(RETRIES + 1):
        try:
            response = session.get(url, timeout=120, **kwargs)
            if allow_404 and response.status_code == 404:
                return None
            if response.status_code in (429, 500, 502, 503, 504):
                raise RuntimeError(f"HTTP {response.status_code}")
            response.raise_for_status()
            return response.json()
        except (requests.RequestException, ValueError, RuntimeError) as error:
            if attempt == RETRIES:
                raise RuntimeError(f"{url}: {error}") from error
            pause = 10 * (attempt + 1)
            log.debug("%s: %s — повтор через %s с", url, error, pause)
            time.sleep(pause)
    raise RuntimeError("unreachable")


# ---------------------------------------------------------------- отбор


def candidates(session: requests.Session, pool: int, min_sitelinks: int,
               delay: float) -> list[dict]:
    """Пул игр по убыванию sitelinks. Заголовки статей — отдельным `wbgetentities`, а не
    вторым блоком SPARQL: с соединением по `schema:about` запрос на сотни игр не укладывается
    в минуту WDQS и получает 504. Статья в англ. Википедии обязательна: из неё и обложка,
    и просмотры, по которым потом считается сегодняшняя популярность."""
    sparql = f"""SELECT ?item ?sitelinks WHERE {{
  ?item wdt:P31 wd:{Q_VIDEO_GAME} ; wikibase:sitelinks ?sitelinks .
  FILTER(?sitelinks >= {min_sitelinks})
}}
ORDER BY DESC(?sitelinks)
LIMIT {pool}"""
    data = http(session, WDQS, params={"query": sparql, "format": "json"})
    order = {}
    for row in data["results"]["bindings"]:
        qid = row["item"]["value"].rsplit("/", 1)[-1]
        if qid.startswith("Q"):
            order[qid] = int(row["sitelinks"]["value"])
    titles = wikidata(session, list(order), "sitelinks", delay,
                      sitefilter="enwiki|ruwiki")
    games = []
    for qid, sitelinks in order.items():
        links = (titles.get(qid) or {}).get("sitelinks") or {}
        if "enwiki" not in links:
            continue
        games.append({
            "qid": qid,
            "sitelinks": sitelinks,
            "en": links["enwiki"]["title"],
            "ru": (links.get("ruwiki") or {}).get("title"),
        })
    return games


def months(back: int = 2) -> tuple[str, str]:
    """Два последних полных месяца: границы для Pageviews (`YYYYMMDD00`)."""
    first_of_this = date.today().replace(day=1)
    end = first_of_this - timedelta(days=1)
    start = end.replace(day=1)
    for _ in range(back - 1):
        start = (start - timedelta(days=1)).replace(day=1)
    return start.strftime("%Y%m%d00"), end.strftime("%Y%m%d00")


def views(session: requests.Session, project: str, title: str, start: str, end: str,
          delay: float) -> int:
    """Просмотры статьи за период; нет статистики (новая статья, редирект) — 0."""
    url = PAGEVIEWS.format(project=project, title=quote(title.replace(" ", "_"), safe=""),
                           start=start, end=end)
    try:
        data = http(session, url, delay=delay, allow_404=True)
    except RuntimeError as error:
        log.warning("pageviews %s/%s: %s", project, title, error)
        return 0
    return sum(int(item.get("views") or 0) for item in (data or {}).get("items", []))


def popular(session: requests.Session, games: list[dict], limit: int, delay: float) -> list[dict]:
    """Пул → самые читаемые сейчас: просмотры англ. статьи за два полных месяца. Только англ.:
    ru-просмотры почти повторяют порядок, а запросов вдвое больше — с общего корпоративного
    адреса Pageviews и так отвечает 429."""
    start, end = months()
    log.info("просмотры за %s–%s для %d кандидатов", start[:6], end[:6], len(games))
    for number, game in enumerate(games, 1):
        game["views"] = views(session, "en.wikipedia", game["en"], start, end, delay)
        if number % 50 == 0:
            log.info("просмотры: %d/%d", number, len(games))
    games.sort(key=lambda g: (g["views"], g["sitelinks"]), reverse=True)
    return games[:limit]


# ---------------------------------------------------------------- Wikidata


def wikidata(session: requests.Session, ids: list[str], props: str, delay: float,
             sitefilter: str | None = None) -> dict:
    """`wbgetentities` пачками по 50 (предел API), с паузой между пачками. Язык `mul` — общие
    названия вроде «PlayStation 5»: у таких элементов отдельной английской метки уже нет."""
    found = {}
    for start in range(0, len(ids), BATCH):
        chunk = ids[start:start + BATCH]
        params = {
            "action": "wbgetentities", "ids": "|".join(chunk), "props": props,
            "languages": "ru|en|mul", "format": "json",
        }
        if sitefilter:
            params["sitefilter"] = sitefilter
        data = http(session, WIKIDATA_API, delay=delay, params=params)
        found.update(data.get("entities") or {})
    return found


def label(entity: dict, *langs: str) -> str | None:
    labels = entity.get("labels") or {}
    return next((labels[lang]["value"] for lang in langs if lang in labels), None)


def claims(entity: dict, prop: str) -> list[dict]:
    return [c for c in (entity.get("claims") or {}).get(prop, [])
            if (c.get("mainsnak") or {}).get("snaktype") == "value"]


def value_of(claim: dict):
    return claim["mainsnak"]["datavalue"]["value"]


def refs(entity: dict, prop: str) -> list[str]:
    """Id сущностей, на которые ссылается свойство (разработчик, платформа, жанр…)."""
    out = []
    for claim in claims(entity, prop):
        value = value_of(claim)
        if isinstance(value, dict) and value.get("entity-type") == "item" \
                and value["id"] not in out:
            out.append(value["id"])
    return out


def released(entity: dict) -> str | None:
    """Самая ранняя дата выхода P577: у игры их столько, сколько платформ и регионов.
    Точность 9 (год) — 1 января, 10 (месяц) — первое число."""
    dates = []
    for claim in claims(entity, P_PUBLICATION):
        value = value_of(claim)
        if not isinstance(value, dict):
            continue
        match = re.match(r"^\+(\d{4})-(\d{2})-(\d{2})T", value.get("time", ""))
        if not match:
            continue
        year, month, day = (int(part) for part in match.groups())
        precision = value.get("precision", 11)
        if not 1950 <= year <= date.today().year + 5:
            continue
        if precision <= 9:
            month, day = 1, 1
        elif precision == 10:
            day = 1
        dates.append(f"{year:04d}-{max(month, 1):02d}-{max(day, 1):02d}")
    return min(dates) if dates else None


def steam_appid(entity: dict) -> int | None:
    for claim in claims(entity, P_STEAM):
        value = value_of(claim)
        if isinstance(value, str) and value.isdigit():
            return int(value)
    return None


# ---------------------------------------------------------------- обложки


def infobox_images(session: requests.Session, titles: list[str], delay: float) -> dict[str, str]:
    """Заголовок англ. статьи → имя файла из карточки: у статей об играх там бокс-арт. Берём
    вступительную секцию вики-текста (`rvsection=0`) — в ней и стоит шаблон Infobox."""
    found = {}
    for start in range(0, len(titles), 20):
        chunk = titles[start:start + 20]
        data = http(session, ENWIKI_API, delay=delay, params={
            "action": "query", "prop": "revisions", "rvprop": "content", "rvslots": "main",
            "rvsection": "0", "redirects": 1, "titles": "|".join(chunk), "format": "json",
        })
        query = data.get("query") or {}
        # Редиректы: ответ придёт под целевым заголовком, вернуть надо под исходным.
        aliases = {r["to"]: r["from"] for r in query.get("redirects") or []}
        for page in (query.get("pages") or {}).values():
            revisions = page.get("revisions") or []
            if not revisions:
                continue
            text = ((revisions[0].get("slots") or {}).get("main") or {}).get("*") or ""
            match = re.search(
                r"\|\s*(?:image|cover|boxart)\s*=\s*(?:\[\[)?(?:File:|Image:)?\s*"
                r"([^|\]\n<]+?\.(?:jpg|jpeg|png|gif|webp))", text, re.I)
            if not match:
                continue
            name = match.group(1).strip().replace("_", " ")
            title = page.get("title")
            for key in (title, aliases.get(title)):
                if key:
                    found[key] = name
    return found


def file_covers(session: requests.Session, api: str, names: list[str],
                delay: float) -> dict[str, dict]:
    """Имя файла → адрес копии шириной COVER_WIDTH и её размеры (`imageinfo` с `iiurlwidth`:
    `thumbwidth`/`thumbheight` — ровно то, что получит браузер). Файл уже уже этой ширины —
    масштабирования нет, и размеры берутся у оригинала."""
    covers = {}
    for start in range(0, len(names), BATCH):
        chunk = names[start:start + BATCH]
        data = http(session, api, delay=delay, params={
            "action": "query", "prop": "imageinfo", "iiprop": "size|url|mime",
            "iiurlwidth": COVER_WIDTH, "redirects": 1,
            "titles": "|".join(f"File:{name}" for name in chunk), "format": "json",
        })
        for page in ((data.get("query") or {}).get("pages") or {}).values():
            info = (page.get("imageinfo") or [{}])[0]
            url = info.get("thumburl") or info.get("url")
            title = (page.get("title") or "").removeprefix("File:")
            if not url or not title or not (info.get("mime") or "").startswith("image/"):
                continue
            scaled = (info.get("width") or 0) > COVER_WIDTH
            width = info.get("thumbwidth") if scaled else info.get("width")
            height = info.get("thumbheight") if scaled else info.get("height")
            covers[title] = {
                # utm-метки `imageinfo` в адресе не нужны — файл отдаётся и без них.
                "url": url.split("?")[0],
                "width": width if isinstance(width, int) and width > 0 else None,
                "height": height if isinstance(height, int) and height > 0 else None,
            }
    return covers


def commons_file(entity: dict) -> str | None:
    """Запасная обложка из Wikidata: изображение P18, иначе логотип P154."""
    for prop in (P_IMAGE, P_LOGO):
        for claim in claims(entity, prop):
            name = value_of(claim)
            if isinstance(name, str) and name.strip():
                return name.strip().replace("_", " ")
    return None


def extracts(session: requests.Session, api: str, titles: list[str], delay: float) -> dict[str, str]:
    """Вступления статей пачками по 20: это описание карточки."""
    found = {}
    for start in range(0, len(titles), 20):
        chunk = titles[start:start + 20]
        data = http(session, api, delay=delay, params={
            "action": "query", "prop": "extracts", "exintro": 1, "explaintext": 1,
            "redirects": 1, "titles": "|".join(chunk), "format": "json",
        })
        query = data.get("query") or {}
        aliases = {r["to"]: r["from"] for r in query.get("redirects") or []}
        for page in (query.get("pages") or {}).values():
            title, extract = page.get("title"), page.get("extract")
            if title and extract:
                for key in (title, aliases.get(title)):
                    if key:
                        found[key] = extract
    return found


# ---------------------------------------------------------------- Wikidata → строка


def row_of(game: dict, entity: dict, names: dict[str, str], platform_names: dict[str, str],
           cover: dict | None, description: str | None) -> dict | None:
    """Строка каталога из записи Wikidata; `None` — нечего показывать."""
    title_ru = label(entity, "ru") or game["ru"]
    title_en = label(entity, "en", "mul") or game["en"]
    title = clean(title_ru or title_en, MAX_TITLE)
    if not title or not cover:
        return None
    original = clean(title_en, MAX_TITLE)
    # Незнакомая платформа — slug от английского названия: с русского («браузер» → `brauzer`)
    # получается нечитаемое значение фильтра. У игр вроде Minecraft платформ три десятка,
    # включая ответвления и эмуляторы, — в карточку идут первые MAX_PLATFORMS.
    platforms = []
    for qid in refs(entity, P_PLATFORM):
        slug = PLATFORMS.get(qid) or slugify(platform_names.get(qid) or "")[:50]
        slug = PLATFORM_ALIASES.get(slug, slug)
        if slug and slug not in platforms:
            platforms.append(slug)
    platforms = platforms[:MAX_PLATFORMS]
    metadata = {
        "platforms": platforms or None,
        "developer": clean(next((names[q] for q in refs(entity, P_DEVELOPER) if q in names),
                                None), MAX_TEXT),
        "publisher": clean(next((names[q] for q in refs(entity, P_PUBLISHER) if q in names),
                                None), MAX_TEXT),
    }
    tags = []
    for qid in refs(entity, P_GENRE):
        tag = GENRES.get(qid)
        if tag and tag not in tags:
            tags.append(tag)
    release = released(entity)
    return {
        "wikidata": game["qid"],
        "steam": steam_appid(entity),
        "title": title,
        "original_title": original if original and original != title else None,
        "description": clean(description, MAX_DESCRIPTION),
        "release_date": release,
        "cover_url": cover["url"],
        "cover_width": cover["width"],
        "cover_height": cover["height"],
        "metadata": {k: v for k, v in metadata.items() if v is not None},
        "tags": tags,
        "year": int(release[:4]) if release else None,
    }


# ---------------------------------------------------------------- DB


def known_qids(conn) -> set[str]:
    with conn.cursor() as cur:
        cur.execute("SELECT external_ids ->> 'wikidata' FROM entities "
                    "WHERE external_ids ? 'wikidata'")
        return {row[0] for row in cur.fetchall()}


def free_slug(cur, candidates_: list[str]) -> str | None:
    cur.execute("SELECT slug FROM entities WHERE slug = ANY(%s)", (candidates_,))
    taken = {row[0] for row in cur.fetchall()}
    return next((c for c in candidates_ if c not in taken), None)


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


def save(conn, row: dict) -> str:
    """Игра и её теги — в одной транзакции: 'written' или 'duplicate'. Дубликат — занятый slug
    или уже записанный qid/appid (уникальные индексы по `external_ids`)."""
    with conn, conn.cursor() as cur:
        base = slugify(row["original_title"] or row["title"])
        candidates_ = [c for c in (base, f"{base}-{row['year']}" if base and row["year"] else None)
                       if c]
        candidates_.append(f"{base or 'game'}-{row['wikidata'].lower()}")
        slug = free_slug(cur, candidates_)
        if not slug:
            return "duplicate"
        external = {"wikidata": row["wikidata"]}
        if row["steam"]:
            external["steam"] = row["steam"]
        cur.execute(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, cover_width, cover_height, metadata, external_ids)
               VALUES ('game', %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
               ON CONFLICT DO NOTHING RETURNING id""",
            (
                slug, row["title"], row["original_title"], row["description"],
                row["release_date"], row["cover_url"], row["cover_width"], row["cover_height"],
                json.dumps(row["metadata"], ensure_ascii=False),
                json.dumps(external, ensure_ascii=False),
            ),
        )
        inserted = cur.fetchone()
        if not inserted:
            return "duplicate"
        # Slug нужен вызывающему: по нему игру потом досылают в поисковый индекс.
        row["slug"] = slug
        for tag_slug, name, kind in row["tags"]:
            cur.execute(
                "INSERT INTO entity_tags (entity_id, tag_id) VALUES (%s, %s) "
                "ON CONFLICT DO NOTHING",
                (inserted[0], tag_id(cur, tag_slug, name, kind)),
            )
    return "written"


# ---------------------------------------------------------------- main


def collect(session: requests.Session, chosen: list[dict], delay: float) -> list[dict]:
    """Выбранные игры → строки каталога: заявления Wikidata, названия ссылок, обложки, описания."""
    entities = wikidata(session, [g["qid"] for g in chosen], "labels|claims", delay)
    linked = sorted({qid for game in chosen
                     for prop in (P_DEVELOPER, P_PUBLISHER, P_PLATFORM)
                     for qid in refs(entities.get(game["qid"]) or {}, prop)})
    names_raw = wikidata(session, linked, "labels", delay) if linked else {}
    names = {qid: name for qid, entity in names_raw.items()
             if (name := label(entity, "ru", "en", "mul"))}
    platform_names = {qid: name for qid, entity in names_raw.items()
                      if (name := label(entity, "en", "mul", "ru"))}

    images = infobox_images(session, [g["en"] for g in chosen], delay)
    en_covers = file_covers(session, ENWIKI_API, sorted(set(images.values())), delay)
    fallback = {}
    for game in chosen:
        if game["en"] not in images or images[game["en"]] not in en_covers:
            name = commons_file(entities.get(game["qid"]) or {})
            if name:
                fallback[game["qid"]] = name
    commons = file_covers(session, COMMONS_API, sorted(set(fallback.values())), delay) \
        if fallback else {}

    ru_titles = [g["ru"] for g in chosen if g["ru"]]
    ru_text = extracts(session, RUWIKI_API, ru_titles, delay) if ru_titles else {}
    en_text = extracts(session, ENWIKI_API, [g["en"] for g in chosen if not ru_text.get(g["ru"])],
                       delay)

    rows = []
    for game in chosen:
        entity = entities.get(game["qid"])
        if not entity:
            continue
        cover = en_covers.get(images.get(game["en"], "")) or commons.get(
            fallback.get(game["qid"], ""))
        description = ru_text.get(game["ru"]) or en_text.get(game["en"])
        row = row_of(game, entity, names, platform_names, cover, description)
        if row:
            rows.append(row)
        else:
            log.info("пропуск %s (%s): нет обложки или названия", game["en"], game["qid"])
    return rows


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--limit", type=int, default=100, help="сколько игр записать")
    parser.add_argument("--pool", type=int, default=400,
                        help="кандидатов из Wikidata, из них просмотры выберут --limit")
    parser.add_argument("--min-sitelinks", type=int, default=8,
                        help="минимум языковых разделов у кандидата")
    parser.add_argument("--delay", type=float, default=DELAY,
                        help="пауза между запросами к API Викимедиа, с")
    parser.add_argument("--views-delay", type=float, default=VIEWS_DELAY,
                        help="пауза между запросами к Pageviews, с")
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

    conn = psycopg2.connect(dsn) if dsn else None
    known = known_qids(conn) if conn else set()
    session = requests.Session()
    session.headers["User-Agent"] = USER_AGENT
    stats = {"written": 0, "duplicate": 0, "skipped": 0}
    written = []
    try:
        pool = [g for g in candidates(session, args.pool, args.min_sitelinks, args.delay)
                if g["qid"] not in known]
        log.info("кандидатов: %d (новых, порог sitelinks ≥ %d)", len(pool), args.min_sitelinks)
        chosen = popular(session, pool, args.limit, args.views_delay)
        log.info("выбрано %d: от «%s» (%d просмотров) до «%s» (%d)", len(chosen),
                 chosen[0]["en"], chosen[0]["views"], chosen[-1]["en"], chosen[-1]["views"])
        rows = collect(session, chosen, args.delay)
        stats["skipped"] = len(chosen) - len(rows)
        for row in rows:
            result = "written" if args.dry_run else save(conn, row)
            stats[result] += 1
            if result == "written":
                written.append(row.get("slug") or row["wikidata"])
                log.debug("%s %s (%s)", result, row["title"], row["release_date"])
    except KeyboardInterrupt:
        log.warning("прервано")
    finally:
        if conn:
            conn.close()
    log.info("готово: %s", stats)
    if written and not args.dry_run:
        log.info("индексация: scripts/index-entities.sh %s", " ".join(written))
    return 0


if __name__ == "__main__":
    sys.exit(main())
