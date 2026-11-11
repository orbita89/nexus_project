#!/usr/bin/env python3
"""Топ сериалов TMDB в каталог Nexus (раздел «Сериалы»).

Отдельный маленький скрипт, а не ветка в конвейере фильмов (`fetch_tmdb.py` → `merge.py`): тот
рассчитан на 1,2 млн строк с кэшем, дампом id и слиянием с Wikidata, здесь же — сотня-другая
сериалов прямыми запросами.

Сериал — строка `entities` (kind = 'series'), своих таблиц нет:

    title           ← `name` (ru), нет русского — `original_name`
    original_title  ← `original_name`, если отличается от `title`
    description     ← `overview` (ru), до 20 000 символов
    release_date    ← `first_air_date` (дата первой серии)
    cover_url       ← постер `https://image.tmdb.org/t/p/w780{poster_path}`, как у фильмов
    cover_width,    ← размеры этой копии: ширина 780 (у́же оригинала — его ширина), высота по
    cover_height      пропорциям оригинала из `images` (карточка получает точное `aspect-ratio`)
    metadata        ← {"seasons", "episodes", "status", "countries", "studios", "trailers"}
                      (схема `SeriesMetadata` в `catalog::metadata`)
    external_ids    ← {"tmdb_tv": id}; индекс `entities_external_tmdb_tv_uq`
    entity_stats    ← TMDB popularity и число голосов: сортировки «Смотрят сейчас» и рейтинг
    участники       ← `created_by` → роль `creator`, первые 15 из `credits.cast` → `actor`
    теги            ← жанры TMDB (`/genre/tv/list`, ru + en); нет тега — создаётся

**Почему `tmdb_tv`, а не `tmdb`:** у TMDB свои пространства id для фильмов и сериалов (1399 — и
фильм, и «Игра престолов»), общий UNIQUE-индекс отверг бы такой сериал как дубль. И условие
`external_ids ? 'tmdb'` по всему каталогу значит «фильм из TMDB»: по нему считается видимость,
гасятся не вошедшие в топ и отбираются кандидаты в холодный архив — сериалы под этим ключом
погасли бы при первом же пересчёте. Миграция `20261127100000_external_tmdb_tv.sql`.

Список — `/discover/tv`: `--source rated` (по умолчанию) — по рейтингу среди сериалов с 500+
голосами, `voted` — по числу голосов, `popular` — по популярности. Готовый `/tv/top_rated` не
годится: он поднимает наверх свежие премьеры с десятком оценок.

Пропускаются: без названия или постера, уже записанный id (повторный прогон безопасен),
не прошедшие ограничения `catalog::metadata`.

Ключ — `TMDB_API_KEY` из окружения, корневого `.env` или `infra/.env` (v3-ключ или v4-токен).

Запуск:
    python3 -m venv scripts/catalog-import/.venv
    scripts/catalog-import/.venv/bin/pip install -r scripts/catalog-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/catalog-import/.venv/bin/python scripts/catalog-import/series_tmdb.py \\
        [--limit 100] [--source rated|voted|popular] [--dry-run] [-v]
Затем — точечная индексация записанных (скрипт печатает готовую команду):
    scripts/index-entities.sh game-of-thrones breaking-bad ...
"""

import argparse
import gzip
import json
import logging
import os
import re
import sys
import time
import unicodedata
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timedelta, timezone
from pathlib import Path

import psycopg2
import requests

ROOT = Path(__file__).resolve().parents[2]
CACHE = Path(os.path.expanduser("~/.cache/nexus-catalog-import/tmdb"))
EXPORTS = "https://files.tmdb.org/p/exports"
TMDB = "https://api.themoviedb.org/3"
POSTER = "https://image.tmdb.org/t/p/w780"
POSTER_WIDTH = 780
LANGUAGE = "ru-RU"

# Ограничения `catalog::metadata` и `catalog::validate` — больше бэкенд не примет.
MAX_TITLE = 300
MAX_DESCRIPTION = 20_000
MAX_STUDIO = 120
MAX_STUDIOS = 20
MAX_SEASONS = 10_000
MAX_EPISODES = 100_000
MAX_SLUG_BASE = 80
MAX_CAST = 15
PER_PAGE = 20  # TMDB отдаёт списки страницами по 20
# Нижняя граница голосов для списков по рейтингу: без неё `vote_average.desc` поднимает наверх
# сериалы с десятком оценок, а `/tv/top_rated` засоряется свежими премьерами.
MIN_VOTES = 500
RETRIES = 5
DELAY = 0.1  # у TMDB лимит ~50 запросов/с, сотня сериалов укладывается без пауз

# `status` TMDB → `SeriesStatus` каталога. Остальные («In Production», «Planned», «Pilot») —
# ничего: поле необязательное, а врать «идёт» про неснятое не надо.
STATUS = {
    "Returning Series": "ongoing",
    "Ended": "ended",
    "Canceled": "canceled",
}

# Жанры, у которых slug отличается от транслита названия (совпадает с `GENRE_SLUGS` фильмов).
GENRE_SLUGS = {
    16: "animation", 35: "comedy", 80: "crime", 99: "documentary", 18: "drama",
    10751: "family", 9648: "detective", 37: "western",
    10759: "action-adventure", 10762: "kids", 10763: "news", 10764: "reality",
    10765: "sci-fi-fantasy", 10766: "soap", 10767: "talk", 10768: "war-politics",
}

log = logging.getLogger("series")

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh", "з": "z",
    "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "у": "u", "ф": "f", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh",
    "щ": "shch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
    "ß": "ss", "æ": "ae", "œ": "oe", "ø": "o", "đ": "d", "ł": "l", "þ": "th", "ð": "d",
}


def slugify(text: str) -> str:
    """Как `slugify` каталога (и остальных импортов); может быть пустой."""
    text = "".join(TRANSLIT.get(ch, ch) for ch in text.lower())
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    text = re.sub(r"['’.]", "", text)
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].strip("-")


def clean(value: str | None, limit: int) -> str | None:
    value = re.sub(r"\s+", " ", (value or "")).strip()
    return value[:limit].strip() if value else None


def env(key: str) -> str | None:
    """Переменная окружения, иначе строка `KEY=value` из `.env` или `infra/.env` (как fetch_tmdb)."""
    if value := os.environ.get(key):
        return value
    for path in (ROOT / ".env", ROOT / "infra" / ".env"):
        if path.exists():
            for line in path.read_text(encoding="utf-8").splitlines():
                name, sep, value = line.strip().partition("=")
                if sep and name.strip() == key and not line.lstrip().startswith("#"):
                    return value.strip().strip("'\"") or None
    return None


# ---------------------------------------------------------------- TMDB


class Tmdb:
    """Клиент TMDB с повторами: v4-токен (`eyJ…`) идёт заголовком, v3-ключ — параметром."""

    def __init__(self, key: str, delay: float = DELAY):
        self.session = requests.Session()
        adapter = requests.adapters.HTTPAdapter(pool_connections=64, pool_maxsize=64)
        self.session.mount("https://", adapter)
        self.session.mount("http://", adapter)
        self.delay = delay
        self.params = {}
        if key.startswith("eyJ"):
            self.session.headers["Authorization"] = f"Bearer {key}"
        else:
            self.params["api_key"] = key

    def get(self, path: str, **params) -> dict | None:
        """Ответ TMDB; 404 — `None` (сериал скрыт или удалён)."""
        for attempt in range(RETRIES + 1):
            if self.delay:
                time.sleep(self.delay)
            try:
                response = self.session.get(
                    f"{TMDB}{path}", timeout=60, params={**self.params, **params})
                if response.status_code == 404:
                    return None
                if response.status_code == 429 or response.status_code >= 500:
                    raise RuntimeError(f"HTTP {response.status_code}")
                response.raise_for_status()
                return response.json()
            except (requests.RequestException, ValueError, RuntimeError) as error:
                if attempt == RETRIES:
                    raise RuntimeError(f"{path}: {error}") from error
                pause = 5 * (attempt + 1)
                log.warning("%s: %s — повтор через %s с", path, error, pause)
                time.sleep(pause)
        raise RuntimeError("unreachable")


# Что считать «топом»: порядок `/discover/tv` и порог голосов к нему.
SOURCES = {
    "rated": {"sort_by": "vote_average.desc", "vote_count.gte": MIN_VOTES},
    "voted": {"sort_by": "vote_count.desc"},
    "popular": {"sort_by": "popularity.desc", "vote_count.gte": MIN_VOTES},
    "all": {},
    "export": {},
}


def download_export(cache: Path = CACHE) -> Path:
    """Дамп публикуется около 08:00 UTC — если сегодняшнего ещё нет, берём вчерашний и т. д."""
    cache.mkdir(parents=True, exist_ok=True)
    today = datetime.now(timezone.utc).date()
    for back in range(4):
        day = today - timedelta(days=back)
        name = f"tv_series_ids_{day:%m_%d_%Y}.json.gz"
        path = cache / name
        if path.exists():
            log.info("export: %s (cache)", path)
            return path
        url = f"{EXPORTS}/{name}"
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urllib.request.urlopen(req) as resp:
                data = resp.read()
                tmp = path.with_suffix(".part")
                tmp.write_bytes(data)
                tmp.rename(path)
                log.info("export: downloaded %s (%.1f MB)", name, len(data) / 1e6)
                return path
        except Exception as e:
            log.info("export: %s not published yet (%s)", name, e)
            continue
    raise RuntimeError("no TMDB tv_series export for the last 4 days")


def select_export_ids(path: Path, min_popularity: float = 0.0, limit: int = 0) -> list[int]:
    """Строки дампа: {"id":1,"original_name":"...","popularity":4.591}."""
    rows: list[tuple[float, int]] = []
    with gzip.open(path, "rt", encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            item = json.loads(line)
            pop = item.get("popularity") or 0.0
            if pop < min_popularity:
                continue
            rows.append((-pop, item["id"]))
    rows.sort()
    ids = [item[1] for item in rows]
    if limit > 0:
        ids = ids[:limit]
    log.info("export: %d tv series selected (min_popularity=%g, limit=%d)", len(ids), min_popularity, limit)
    return ids


def top_ids_for_source(tmdb: Tmdb, source: str, limit: int = 0, min_votes: int | None = None,
                       workers: int = 16) -> list[int]:
    """Id сериалов из `/discover/tv` в выбранном порядке с параллельной загрузкой страниц."""
    params = dict(SOURCES.get(source, {}))
    if min_votes is not None:
        if min_votes > 0:
            params["vote_count.gte"] = min_votes
        elif "vote_count.gte" in params:
            del params["vote_count.gte"]

    first_page = tmdb.get("/discover/tv", language=LANGUAGE, page=1, **params)
    if not first_page or not first_page.get("results"):
        return []

    total_pages = min(first_page.get("total_pages", 1), 500)
    if limit > 0:
        target_pages = min((limit + PER_PAGE - 1) // PER_PAGE, total_pages)
    else:
        target_pages = total_pages

    page_results = {1: [item["id"] for item in (first_page.get("results") or []) if item.get("id")]}
    if target_pages > 1:
        def fetch_p(p):
            data = tmdb.get("/discover/tv", language=LANGUAGE, page=p, **params)
            return p, [item["id"] for item in (data or {}).get("results") or [] if item.get("id")]

        with ThreadPoolExecutor(max_workers=workers) as executor:
            for p, p_ids in executor.map(fetch_p, range(2, target_pages + 1)):
                page_results[p] = p_ids

    ids: list[int] = []
    for p in range(1, target_pages + 1):
        ids.extend(page_results.get(p, []))
        if limit > 0 and len(ids) >= limit:
            break
    return ids[:limit] if limit > 0 else ids


def top_ids(tmdb: Tmdb, source: str, limit: int = 0, min_votes: int | None = None,
            workers: int = 16, min_popularity: float = 0.0) -> list[int]:
    """Id сериалов из `/discover/tv` или официального дампа экспорта TMDB."""
    if source == "export":
        export_file = download_export()
        return select_export_ids(export_file, min_popularity, limit)
    if source == "all":
        seen = set()
        all_ids = []
        for src in ("voted", "popular", "rated"):
            src_ids = top_ids_for_source(tmdb, src, limit, min_votes, workers)
            for sid in src_ids:
                if sid not in seen:
                    seen.add(sid)
                    all_ids.append(sid)
            if limit > 0 and len(all_ids) >= limit:
                break
        return all_ids[:limit] if limit > 0 else all_ids
    return top_ids_for_source(tmdb, source, limit, min_votes, workers)


def genre_tags(tmdb: Tmdb) -> dict[int, tuple[str, str]]:
    """id жанра → (slug, русское название). Русские названия — из `/genre/tv/list`."""
    ru = (tmdb.get("/genre/tv/list", language=LANGUAGE) or {}).get("genres") or []
    en = (tmdb.get("/genre/tv/list", language="en-US") or {}).get("genres") or []
    en_names = {g["id"]: g["name"] for g in en}
    tags = {}
    for genre in ru:
        gid = genre["id"]
        slug = GENRE_SLUGS.get(gid) or slugify(en_names.get(gid, "")) or f"genre-{gid}"
        name = clean(genre.get("name") or en_names.get(gid) or slug, 100) or slug
        tags[gid] = (slug, name[:1].upper() + name[1:])
    return tags


def poster_size(data: dict, poster_path: str) -> tuple[int | None, int | None]:
    """Размеры копии `w780`: ширина 780 (у́же оригинала — его), высота по пропорциям оригинала.
    Оригинал — из `append_to_response=images`; постера там нет (другой язык) — размеров нет."""
    for image in ((data.get("images") or {}).get("posters") or []):
        if image.get("file_path") != poster_path:
            continue
        width, height = image.get("width"), image.get("height")
        if isinstance(width, int) and isinstance(height, int) and width > 0 and height > 0:
            if width <= POSTER_WIDTH:
                return width, height
            return POSTER_WIDTH, max(1, round(height * POSTER_WIDTH / width))
    return None, None


def trailers_of(data: dict) -> list[dict]:
    """Трейлеры YouTube (`TrailerSource` каталога): до 5, официальные первыми."""
    videos = ((data.get("videos") or {}).get("results") or [])
    picked = []
    for video in sorted(videos, key=lambda v: not v.get("official")):
        if video.get("site") != "YouTube" or video.get("type") not in ("Trailer", "Teaser"):
            continue
        key = (video.get("key") or "").strip()
        if len(key) == 11 and not any(t["id"] == key for t in picked):
            picked.append({"provider": "youtube", "id": key})
        if len(picked) == 5:
            break
    return picked


def row_of(data: dict, tags: dict[int, tuple[str, str]]) -> dict | None:
    """Строка каталога из ответа TMDB; `None` — нечего показывать."""
    title = clean(data.get("name") or data.get("original_name"), MAX_TITLE)
    poster_path = data.get("poster_path")
    if not title or not poster_path:
        return None
    original = clean(data.get("original_name"), MAX_TITLE)
    width, height = poster_size(data, poster_path)
    countries = [c.upper() for c in (data.get("origin_country") or [])
                 if isinstance(c, str) and len(c) == 2]
    studios = []
    for company in (data.get("production_companies") or [])[:MAX_STUDIOS]:
        name = clean(company.get("name"), MAX_STUDIO)
        if name and name not in studios:
            studios.append(name)
    metadata = {
        "seasons": n if 1 <= (n := data.get("number_of_seasons") or 0) <= MAX_SEASONS else None,
        "episodes": n if 1 <= (n := data.get("number_of_episodes") or 0) <= MAX_EPISODES else None,
        "status": STATUS.get(data.get("status") or ""),
        "countries": countries or None,
        "studios": studios or None,
        "trailers": trailers_of(data) or None,
    }
    creators = [name for person in (data.get("created_by") or [])
                if (name := clean(person.get("name"), MAX_TITLE))]
    cast = []
    for person in ((data.get("credits") or {}).get("cast") or [])[:MAX_CAST]:
        name = clean(person.get("name"), MAX_TITLE)
        if name:
            cast.append((name, clean(person.get("character"), MAX_TITLE)))
    year = (data.get("first_air_date") or "")[:4]
    return {
        "tmdb_tv": data["id"],
        "title": title,
        "original_title": original if original and original != title else None,
        "description": clean(data.get("overview"), MAX_DESCRIPTION),
        "release_date": data.get("first_air_date") or None,
        "cover_url": f"{POSTER}{poster_path}",
        "cover_width": width,
        "cover_height": height,
        "metadata": {k: v for k, v in metadata.items() if v is not None},
        "tags": [tags[g["id"]] for g in (data.get("genres") or []) if g.get("id") in tags],
        "creators": creators,
        "cast": cast,
        "popularity": data.get("popularity"),
        "votes": data.get("vote_count"),
        "year": int(year) if year.isdigit() else None,
    }


# ---------------------------------------------------------------- DB


def known_ids(conn) -> set[int]:
    with conn.cursor() as cur:
        cur.execute("SELECT (external_ids ->> 'tmdb_tv')::int FROM entities "
                    "WHERE external_ids ? 'tmdb_tv'")
        return {row[0] for row in cur.fetchall()}


def free_slug(cur, candidates: list[str]) -> str | None:
    cur.execute("SELECT slug FROM entities WHERE slug = ANY(%s)", (candidates,))
    taken = {row[0] for row in cur.fetchall()}
    return next((c for c in candidates if c not in taken), None)


def tag_id(cur, slug: str, name: str) -> str:
    """Тег жанра: уже заведённый (сиды, фильмы) не трогаем — ни название, ни `kind`."""
    cur.execute(
        "INSERT INTO tags (slug, name, kind) VALUES (%s, %s, 'genre') "
        "ON CONFLICT (slug) DO NOTHING RETURNING id",
        (slug, name),
    )
    row = cur.fetchone()
    if row:
        log.info("tag created: %s", slug)
        return row[0]
    cur.execute("SELECT id FROM tags WHERE slug = %s", (slug,))
    return cur.fetchone()[0]


def person_id(cur, name: str) -> str | None:
    """Человек: уже известный по имени или новый. Тёзок здесь не разбираем (как у картин) —
    у сотни сериалов их единицы, а правка в админке остаётся."""
    cur.execute("SELECT id FROM people WHERE full_name = %s ORDER BY created_at LIMIT 1", (name,))
    row = cur.fetchone()
    if row:
        return row[0]
    base = slugify(name) or "person"
    cur.execute("SELECT slug FROM people WHERE slug = ANY(%s)",
                ([base] + [f"{base}-{n}" for n in range(2, 12)],))
    taken = {r[0] for r in cur.fetchall()}
    slug = next((c for c in [base] + [f"{base}-{n}" for n in range(2, 12)] if c not in taken), None)
    if not slug:
        return None
    cur.execute("INSERT INTO people (slug, full_name) VALUES (%s, %s) RETURNING id", (slug, name))
    return cur.fetchone()[0]


def save(conn, row: dict) -> str:
    """Сериал, его участники и теги — в одной транзакции: 'written' или 'duplicate'."""
    with conn, conn.cursor() as cur:
        # Оригинальное название, затем русское: у японских и корейских сериалов транслит
        # оригинала пуст («進撃の巨人» → ''), и без русского запасного slug был бы `series-tv1429`.
        bases = [base for base in (slugify(row["original_title"] or ""),
                                   slugify(row["title"])) if base]
        candidates = []
        for base in bases:
            candidates.append(base)
            if row["year"]:
                candidates.append(f"{base}-{row['year']}")
        candidates.append(f"{bases[0] if bases else 'series'}-tv{row['tmdb_tv']}")
        slug = free_slug(cur, candidates)
        if not slug:
            return "duplicate"
        cur.execute(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, cover_width, cover_height, metadata, external_ids)
               VALUES ('series', %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
               ON CONFLICT DO NOTHING RETURNING id""",
            (
                slug, row["title"], row["original_title"], row["description"],
                row["release_date"], row["cover_url"], row["cover_width"], row["cover_height"],
                json.dumps(row["metadata"], ensure_ascii=False),
                json.dumps({"tmdb_tv": row["tmdb_tv"]}),
            ),
        )
        inserted = cur.fetchone()
        if not inserted:
            return "duplicate"
        entity_id = inserted[0]
        # Slug нужен вызывающему: по нему сериал потом досылают в поисковый индекс.
        row["slug"] = slug
        # Создатели идут перед актёрами: порядок титров — это `position`.
        credits = [(name, "creator", None) for name in row["creators"]]
        credits += [(name, "actor", character) for name, character in row["cast"]]
        for position, (name, role, character) in enumerate(credits):
            person = person_id(cur, name)
            if person:
                cur.execute(
                    "INSERT INTO entity_credits (entity_id, person_id, role, character_name,"
                    " position) VALUES (%s, %s, %s, %s, %s) ON CONFLICT DO NOTHING",
                    (entity_id, person, role, character, position),
                )
        for tag_slug, name in row["tags"]:
            cur.execute(
                "INSERT INTO entity_tags (entity_id, tag_id) VALUES (%s, %s) "
                "ON CONFLICT DO NOTHING",
                (entity_id, tag_id(cur, tag_slug, name)),
            )
        # Сортировки «Смотрят сейчас» и «Популярное»: балл «Рекомендуемые» считается только по
        # фильмам (`ranking.rs`), поэтому у сериалов он остаётся пустым.
        cur.execute(
            "INSERT INTO entity_stats (entity_id, tmdb_popularity, tmdb_votes) "
            "VALUES (%s, %s, %s) ON CONFLICT (entity_id) DO UPDATE "
            "SET tmdb_popularity = EXCLUDED.tmdb_popularity, tmdb_votes = EXCLUDED.tmdb_votes",
            (entity_id, row["popularity"], row["votes"]),
        )
    return "written"


# ---------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--limit", type=int, default=100,
                        help="сколько сериалов записать (0 — все доступные)")
    parser.add_argument("--source", choices=tuple(SOURCES), default="rated",
                        help="порядок: rated — по рейтингу среди сериалов с 500+ голосами "
                             "(по умолчанию), voted — по числу голосов, popular — по популярности, "
                             "all — объединение всех категорий discover, export — официальный дамп экспорта TMDB (все сериалы)")
    parser.add_argument("--min-votes", type=int, default=None,
                        help="минимальное число голосов (по умолчанию: 500 для rated/popular, 0 для voted)")
    parser.add_argument("--min-popularity", type=float, default=0.0,
                        help="минимальная популярность для дампа экспорта (0 — все сериалы)")
    parser.add_argument("--workers", type=int, default=16,
                        help="число параллельных потоков для загрузки TMDB")
    parser.add_argument("--delay", type=float, default=0.0,
                        help="пауза между запросами к TMDB, с (429 — увеличить)")
    parser.add_argument("--dry-run", action="store_true", help="не писать в БД")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )
    key = env("TMDB_API_KEY")
    if not key:
        log.error("TMDB_API_KEY is not set (окружение, .env или infra/.env)")
        return 2
    dsn = os.environ.get("DATABASE_URL")
    if not dsn and not args.dry_run:
        log.error("DATABASE_URL is not set")
        return 2

    tmdb = Tmdb(key, args.delay)
    conn = psycopg2.connect(dsn) if dsn else None
    known = known_ids(conn) if conn else set()
    stats = {"written": 0, "known": 0, "duplicate": 0, "skipped": 0}
    written_slugs: list[str] = []
    try:
        tags = genre_tags(tmdb)
        log.info("TMDB: %d жанров сериалов", len(tags))
        # С запасом: часть сериалов уже записана, часть отсеется без постера.
        ids = top_ids(tmdb, args.source, (args.limit * 2) if args.limit > 0 else 0,
                      min_votes=args.min_votes, workers=args.workers, min_popularity=args.min_popularity)
        log.info("TMDB /discover/tv (%s): %d сериалов", args.source, len(ids))
        fresh = [tv_id for tv_id in ids if tv_id not in known]
        stats["known"] = len(ids) - len(fresh)
        log.info("Новых сериалов для обработки: %d (уже в базе: %d)", len(fresh), stats["known"])

        target_ids = fresh if args.limit <= 0 else fresh[:args.limit * 2]

        def fetch_tv(tv_id: int):
            data = tmdb.get(
                f"/tv/{tv_id}", language=LANGUAGE,
                append_to_response="credits,videos,images",
                include_image_language="ru,en,null",
            )
            return tv_id, row_of(data, tags) if data else None

        with ThreadPoolExecutor(max_workers=args.workers) as executor:
            batch_size = args.workers * 2
            processed = 0
            for i in range(0, len(target_ids), batch_size):
                if args.limit > 0 and stats["written"] >= args.limit:
                    break
                chunk = target_ids[i:i + batch_size]
                futures = [executor.submit(fetch_tv, tv_id) for tv_id in chunk]
                for future in as_completed(futures):
                    tv_id, row = future.result()
                    processed += 1
                    if not row:
                        stats["skipped"] += 1
                        continue
                    if args.limit > 0 and stats["written"] >= args.limit:
                        continue
                    result = "written" if args.dry_run else save(conn, row)
                    stats[result] += 1
                    if result == "written" and row.get("slug"):
                        written_slugs.append(row["slug"])
                    if processed % 100 == 0 or processed == len(target_ids):
                        log.info("Прогресс: %d/%d — записано: %d, пропущено: %d, дубликатов: %d",
                                 processed, len(target_ids), stats["written"], stats["skipped"], stats["duplicate"])
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
        log.info("Записано новых сериалов: %d", len(written_slugs))
    return 0


if __name__ == "__main__":
    sys.exit(main())
