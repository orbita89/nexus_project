#!/usr/bin/env python3
"""Топ игр Steam в каталог Nexus (без своих таблиц и без ключей).

Список — SteamSpy (`api.php?request=all&page=N`, 1000 игр на страницу, не чаще раза в минуту),
по убыванию владельцев (нижняя граница `owners`), затем положительных отзывов. Данные игры —
Steam Store `api/appdetails?appids=…&l=russian` (русские тексты, если они есть у игры).

Игра — строка `entities` (kind = 'game'):

    title           ← `name` из appdetails (русское, если игра локализовала название)
    original_title  ← `name` SteamSpy (английское), если отличается
    description     ← `about_the_game` без HTML, иначе `short_description` (до 20 000 символов)
    release_date    ← `release_date.date` («19 мая. 2015 г.»); «скоро» — NULL
    cover_url       ← вертикальная обложка `library_600x900_2x.jpg`, иначе `library_600x900.jpg`,
                      иначе горизонтальная `header.jpg` — прямая ссылка на CDN Steam
                      (`shared.akamai.steamstatic.com`, как у фильмов `image.tmdb.org`; у нас
                      картинки не хранятся). Каждая проверяется GET (JPEG/PNG больше 1 КБ).
                      Ни одной — игра пропускается
    metadata        ← {"platforms": pc/macos/linux, "developer", "publisher"} (схема `GameMetadata`)
    external_ids    ← {"steam": appid}; индекс `entities_external_steam_uq`
    теги            ← жанры Steam по id (`GENRES`); нет тега — создаётся (`kind` как в `GENRES`)

Пропускаются: не `type = game`, магазин не отдал игру (`success: false`, в том числе скрытые для
региона), контент только для взрослых (`content_descriptors` 3 или 4), уже записанный appid.
Трейлеры не пишутся (у Steam mp4, у нас — YouTube/Rutube): `make media` добавит Rutube.

Запуск:
    python3 -m venv scripts/games-import/.venv
    scripts/games-import/.venv/bin/pip install -r scripts/games-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/games-import/.venv/bin/python scripts/games-import/steam_games.py \\
        [--limit 1000] [--delay 1.5] [--dry-run]
Затем scripts/reindex-search.sh (поиск сайта) и `make media` (трейлеры Rutube).
Перезапуск безопасен: записанные appid пропускаются (и не запрашиваются у магазина).
"""

import argparse
import hashlib
import html
import json
import logging
import os
import re
import sys
import time
import unicodedata
from datetime import date

import psycopg2
import requests

STEAMSPY_URL = "https://steamspy.com/api.php"
APPDETAILS_URL = "https://store.steampowered.com/api/appdetails"
STEAM_CDN = "https://shared.akamai.steamstatic.com/"
COVER_FILES = ["library_600x900_2x.jpg", "library_600x900.jpg", "header.jpg"]
MIN_COVER_BYTES = 1000
STEAMSPY_DELAY = 61  # SteamSpy: `all` — раз в минуту
MAX_TITLE = 300
MAX_TEXT = 20_000
MAX_NAME = 200
MAX_SLUG_BASE = 80
RETRIES = 5
ADULT_DESCRIPTORS = {3, 4}
PLATFORMS = {"windows": "pc", "mac": "macos", "linux": "linux"}

# id жанра Steam → наш тег (slug, название, kind).
GENRES = {
    "1": ("action", "Боевик", "genre"),
    "25": ("adventure", "Приключения", "genre"),
    "2": ("strategy", "Стратегия", "genre"),
    "3": ("rpg", "Ролевая игра", "genre"),
    "28": ("simulator", "Симулятор", "genre"),
    "9": ("racing", "Гонки", "genre"),
    "18": ("sport", "Спорт", "genre"),
    "4": ("casual", "Казуальная", "genre"),
    "23": ("indie", "Инди", "tag"),
    "29": ("mmo", "MMO", "tag"),
    "37": ("free-to-play", "Бесплатная", "tag"),
    "70": ("early-access", "Ранний доступ", "tag"),
}

MONTHS = {
    "янв": 1, "фев": 2, "мар": 3, "апр": 4, "мая": 5, "май": 5, "июн": 6, "июл": 7,
    "авг": 8, "сен": 9, "окт": 10, "ноя": 11, "дек": 12,
}

log = logging.getLogger("steam_games")

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh", "з": "z",
    "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "у": "u", "ф": "f", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh",
    "щ": "shch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
}


def slugify(text: str) -> str:
    """Как `slugify` каталога (и books-import); может быть пустой."""
    text = "".join(TRANSLIT.get(ch, ch) for ch in text.lower())
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    text = re.sub(r"['’.]", "", text)
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].rstrip("-")


def clean(text: str | None, limit: int) -> str | None:
    if not text:
        return None
    text = " ".join(text.split())[:limit].rstrip()
    return text or None


def strip_html(text: str | None) -> str | None:
    if not text:
        return None
    text = re.sub(r"<br\s*/?>|</p>|</h\d>|</li>", "\n", text, flags=re.I)
    text = html.unescape(re.sub(r"<[^>]+>", "", text))
    lines = [" ".join(line.split()) for line in text.splitlines()]
    text = "\n".join(line for line in lines if line)[:MAX_TEXT].rstrip()
    return text or None


def parse_date(value: str | None) -> date | None:
    """«19 мая. 2015 г.», «5 окт. 2023», «2015» → дата; «Скоро» и прочее — None."""
    if not value:
        return None
    m = re.search(r"(?:(\d{1,2})\s+([а-яё]+)\.?\s+)?(\d{4})", value.lower())
    if not m:
        return None
    day, month_word, year = m.group(1), m.group(2), int(m.group(3))
    month = MONTHS.get((month_word or "")[:3]) if month_word else None
    try:
        if day and month:
            return date(year, month, int(day))
        return date(year, 1, 1)
    except ValueError:
        return None


def owners_low(app: dict) -> int:
    """`owners` SteamSpy — «20,000,000 .. 50,000,000»: нижняя граница."""
    return int(re.sub(r"\D", "", (app.get("owners") or "0").split("..")[0]) or 0)


# ---------------------------------------------------------------- HTTP


def get_json(session: requests.Session, url: str, params: dict):
    for attempt in range(1, RETRIES + 1):
        try:
            resp = session.get(url, params=params, timeout=30)
        except requests.RequestException as e:
            log.warning("request failed (%s), retry %d/%d", e, attempt, RETRIES)
            time.sleep(2**attempt)
            continue
        if resp.status_code == 200:
            return resp.json()
        if resp.status_code in (429, 500, 502, 503, 504):
            # Store отвечает 429 при ~200 запросах за 5 минут.
            wait = int(resp.headers.get("Retry-After") or 2**attempt * 10)
            log.warning("HTTP %d, retry %d/%d in %ds", resp.status_code, attempt, RETRIES, wait)
            time.sleep(wait)
            continue
        raise RuntimeError(f"HTTP {resp.status_code}: {resp.text[:300]}")
    raise RuntimeError(f"giving up after {RETRIES} attempts")


def steamspy_page(session, page: int) -> list[dict]:
    data = get_json(session, STEAMSPY_URL, {"request": "all", "page": page}) or {}
    apps = list(data.values())
    apps.sort(key=lambda a: (owners_low(a), a.get("positive") or 0), reverse=True)
    return apps


def appdetails(session, appid: int) -> dict | None:
    data = get_json(session, APPDETAILS_URL, {"appids": appid, "l": "russian", "cc": "us"}) or {}
    entry = data.get(str(appid)) or {}
    return entry.get("data") if entry.get("success") else None


def cover_ok(session, url: str) -> bool:
    try:
        resp = session.get(url, timeout=30)
    except requests.RequestException:
        return False
    kind = resp.headers.get("Content-Type", "")
    return (
        resp.status_code == 200
        and kind.startswith(("image/jpeg", "image/png"))
        and len(resp.content) > MIN_COVER_BYTES
    )


def pick_cover(session, appid: int, header: str | None) -> str | None:
    candidates = [f"{STEAM_CDN}store_item_assets/steam/apps/{appid}/{f}" for f in COVER_FILES]
    # Новые игры кладут картинки по путям с хэшем — header_image из ответа его знает.
    if header and header.startswith(STEAM_CDN):
        candidates.append(header.split("?")[0])
    return next((url for url in candidates if cover_ok(session, url)), None)


# ---------------------------------------------------------------- Steam → строка


def to_row(app: dict, data: dict, cover: str) -> dict:
    title = clean(data.get("name"), MAX_TITLE) or clean(app.get("name"), MAX_TITLE)
    english = clean(app.get("name"), MAX_TITLE)
    platforms = [PLATFORMS[k] for k, v in (data.get("platforms") or {}).items()
                 if v and k in PLATFORMS]
    metadata = {
        "platforms": platforms,
        "developer": clean(next(iter(data.get("developers") or []), None), MAX_NAME),
        "publisher": clean(next(iter(data.get("publishers") or []), None), MAX_NAME),
    }
    tags = []
    for g in data.get("genres") or []:
        tag = GENRES.get(str(g.get("id")))
        if tag and tag not in tags:
            tags.append(tag)
    release = data.get("release_date") or {}
    return {
        "steam": int(data["steam_appid"]),
        "title": title,
        "original_title": english if english and english != title else None,
        "description": strip_html(data.get("about_the_game"))
        or strip_html(data.get("short_description")),
        "release_date": None if release.get("coming_soon") and not re.search(
            r"\d{4}", release.get("date") or "") else parse_date(release.get("date")),
        "cover_url": cover,
        "metadata": {k: v for k, v in metadata.items() if v},
        "tags": tags,
    }


def skip_reason(data: dict | None) -> str | None:
    if not data:
        return "unavailable"
    if data.get("type") != "game":
        return "not_game"
    ids = set((data.get("content_descriptors") or {}).get("ids") or [])
    if ids & ADULT_DESCRIPTORS:
        return "adult"
    return None


# ---------------------------------------------------------------- DB


def known_appids(conn) -> set[int]:
    with conn.cursor() as cur:
        cur.execute("SELECT (external_ids ->> 'steam')::int FROM entities "
                    "WHERE external_ids ? 'steam'")
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


def save(conn, row: dict) -> str:
    """Игра и её теги в одной транзакции: 'written' или 'duplicate'."""
    with conn, conn.cursor() as cur:
        base = slugify(row["original_title"] or row["title"])
        year = row["release_date"].year if row["release_date"] else None
        suffix = hashlib.md5(str(row["steam"]).encode()).hexdigest()[:6]
        candidates = [c for c in (base, f"{base}-{year}" if base and year else None) if c]
        candidates.append(f"{base or 'game'}-{suffix}")
        slug = free_slug(cur, candidates)
        if not slug:
            return "duplicate"
        cur.execute(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, metadata, external_ids)
               VALUES ('game', %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
               ON CONFLICT DO NOTHING RETURNING id""",
            (
                slug, row["title"], row["original_title"], row["description"],
                row["release_date"], row["cover_url"],
                json.dumps(row["metadata"], ensure_ascii=False),
                json.dumps({"steam": row["steam"]}),
            ),
        )
        inserted = cur.fetchone()
        if not inserted:
            return "duplicate"
        for tag_slug, name, kind in row["tags"]:
            cur.execute(
                "INSERT INTO entity_tags (entity_id, tag_id) VALUES (%s, %s) "
                "ON CONFLICT DO NOTHING",
                (inserted[0], tag_id(cur, tag_slug, name, kind)),
            )
    return "written"


# ---------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--limit", type=int, default=1000, help="сколько новых игр записать")
    parser.add_argument("--delay", type=float, default=1.5,
                        help="пауза между запросами к магазину, с (лимит ~200 за 5 минут)")
    parser.add_argument("--max-pages", type=int, default=5, help="страниц SteamSpy (по 1000)")
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
    known = known_appids(conn) if conn else set()
    session = requests.Session()
    session.headers["User-Agent"] = "nexus-games-import/1.0"
    stats = {"written": 0, "known": 0, "duplicate": 0, "unavailable": 0, "not_game": 0,
             "adult": 0, "no_cover": 0, "errors": 0}
    try:
        for page in range(args.max_pages):
            if stats["written"] >= args.limit:
                break
            if page:
                time.sleep(STEAMSPY_DELAY)
            apps = steamspy_page(session, page)
            log.info("SteamSpy page %d: %d apps", page, len(apps))
            if not apps:
                break
            for app in apps:
                if stats["written"] >= args.limit:
                    break
                appid = int(app["appid"])
                if appid in known:
                    stats["known"] += 1
                    continue
                try:
                    data = appdetails(session, appid)
                    time.sleep(args.delay)
                    reason = skip_reason(data)
                    if reason:
                        stats[reason] += 1
                        continue
                    cover = pick_cover(session, appid, data.get("header_image"))
                    if not cover:
                        stats["no_cover"] += 1
                        continue
                    row = to_row(app, data, cover)
                    result = "written" if args.dry_run else save(conn, row)
                except RuntimeError as e:
                    stats["errors"] += 1
                    log.error("app %d failed: %s", appid, e)
                    continue
                stats[result] += 1
                known.add(appid)
                log.debug("%s %s (%s)", result, row["title"], row["release_date"])
                if stats["written"] % 50 == 0 and result == "written":
                    log.info("progress: %s", stats)
    except KeyboardInterrupt:
        log.warning("interrupted")
    finally:
        if conn:
            conn.close()
    log.info("done: %s", stats)
    if stats["written"] and not args.dry_run:
        log.info("run scripts/reindex-search.sh, then `make media` for Rutube trailers")
    return 0


if __name__ == "__main__":
    sys.exit(main())
