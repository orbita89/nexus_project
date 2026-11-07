#!/usr/bin/env python3
"""Сезоны и серии уже записанных сериалов из TMDB.

Дополняет `series_tmdb.py`: тот заводит сериалы, этот — их сезоны и серии. Берёт из БД сериалы с
`external_ids.tmdb_tv` и для каждого запрашивает `GET /tv/{id}` (список сезонов), а затем
`GET /tv/{id}/season/{n}` — один запрос отдаёт сезон целиком со всеми сериями.

Сезон — строка `entities` (kind = 'season') со своим slug'ом и сериалом в `parent_id`:

    slug            ← `{slug сериала}-season-{n}`, дальше живёт самостоятельно
    title           ← `name` (ru), нет — «Сезон N» («Спецвыпуски» для нулевого)
    original_title  ← `name` сезона в en-US, если отличается
    description     ← `overview` (ru, иначе en), до 2000 символов
    release_date    ← `air_date` сезона: дата выхода первой серии
    cover_url       ← постер сезона `image.tmdb.org/t/p/w780`, `cover_width`/`cover_height` —
                      размеры этой копии (карточка получает точное `aspect-ratio`)
    metadata        ← {"number", "episode_count", "status", "finale_date"}
                      (схема `SeasonMetadata` в `catalog::metadata`)
    parent_id       ← сериал
    external_ids    ← {"tmdb_tv_season": "1399:2"}

Серия — строка `episodes` (не сущность: ни slug'а, ни рецензий). Видео у нас нет, поэтому ни
«просмотрено», ни прогресса здесь не будет:

    number, title, original_title (en), overview, air_date
    runtime_minutes ← `runtime` TMDB, уже в минутах
    still_url       ← кадр из серии `image.tmdb.org/t/p/w780{still_path}`, 16:9 — не постер
    still_width/height ← размеры этой копии кадра

Повторный прогон безопасен: сезон ищется по сериалу и номеру, серия — по сезону и номеру, обе
строки обновляются на месте. Серии, которых больше нет у TMDB, не удаляются — на них могут
стоять ссылки; снять их можно `PUT /admin/seasons/{id}/episodes`.

Ключ — `TMDB_API_KEY` из окружения, корневого `.env` или `infra/.env` (v3-ключ или v4-токен).

Запуск:
    python3 -m venv scripts/catalog-import/.venv
    scripts/catalog-import/.venv/bin/pip install -r scripts/catalog-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/catalog-import/.venv/bin/python \\
        scripts/catalog-import/series_seasons_tmdb.py \\
        [--slug arcane ...] [--limit 100] [--refresh] [--dry-run] [-v]

Сезоны в Meilisearch не индексируются (в разделе «Сериалы» должен быть «Аркейн», а не его
сезоны), поэтому `scripts/index-entities.sh` после прогона не нужен — нужна пересборка статики
сериалов на фронтенде.
"""

import argparse
import json
import logging
import os
import re
import sys
import time
import unicodedata
from datetime import date
from pathlib import Path

import psycopg2
import requests

ROOT = Path(__file__).resolve().parents[2]
TMDB = "https://api.themoviedb.org/3"
IMAGE = "https://image.tmdb.org/t/p/w780"
IMAGE_WIDTH = 780
LANGUAGE = "ru-RU"
FALLBACK_LANGUAGE = "en-US"

# Ограничения `catalog::metadata`, `catalog::validate` и `catalog::admin::episodes`.
MAX_TITLE = 300
MAX_DESCRIPTION = 20_000
MAX_OVERVIEW = 2000
MAX_SEASON_NUMBER = 10_000
MAX_EPISODES = 1000
MAX_RUNTIME = 10_000
RETRIES = 5
DELAY = 0.1  # у TMDB лимит ~50 запросов/с

# `status` сериала TMDB → `SeriesStatus` каталога (как в series_tmdb.py).
STATUS = {
    "Returning Series": "ongoing",
    "Ended": "ended",
    "Canceled": "canceled",
}

log = logging.getLogger("seasons")

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh", "з": "z",
    "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "у": "u", "ф": "f", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh",
    "щ": "shch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
}


def clean(value: str | None, limit: int) -> str | None:
    value = re.sub(r"\s+", " ", (value or "")).strip()
    return value[:limit].strip() if value else None


def slugify(text: str) -> str:
    text = "".join(TRANSLIT.get(ch, ch) for ch in text.lower())
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    text = re.sub(r"['’.]", "", text)
    return re.sub(r"[^a-z0-9]+", "-", text).strip("-")


def env(key: str) -> str | None:
    """Переменная окружения, иначе строка `KEY=value` из `.env` или `infra/.env`."""
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
        self.delay = delay
        self.params = {}
        if key.startswith("eyJ"):
            self.session.headers["Authorization"] = f"Bearer {key}"
        else:
            self.params["api_key"] = key

    def get(self, path: str, **params) -> dict | None:
        """Ответ TMDB; 404 — `None` (сезон скрыт или удалён)."""
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


def image_size(images: list[dict], file_path: str) -> tuple[int | None, int | None]:
    """Размеры копии `w780`: ширина 780 (у́же оригинала — его), высота по пропорциям оригинала."""
    for image in images or []:
        if image.get("file_path") != file_path:
            continue
        width, height = image.get("width"), image.get("height")
        if isinstance(width, int) and isinstance(height, int) and width > 0 and height > 0:
            if width <= IMAGE_WIDTH:
                return width, height
            return IMAGE_WIDTH, max(1, round(height * IMAGE_WIDTH / width))
    return None, None


def season_rows(tmdb: Tmdb, tv_id: int, number: int, series_status: str | None) -> dict | None:
    """Сезон и его серии из `GET /tv/{id}/season/{n}`; нечего показывать — `None`.

    Второй запрос в en-US — ради оригинальных названий и описаний там, где русских нет:
    у TMDB `name` сезона и серии локализованы, оригинала в ответе нет.
    """
    data = tmdb.get(f"/tv/{tv_id}/season/{number}", language=LANGUAGE,
                    append_to_response="images", include_image_language="ru,en,null")
    if not data:
        return None
    english = tmdb.get(f"/tv/{tv_id}/season/{number}", language=FALLBACK_LANGUAGE) or {}
    en_episodes = {e.get("episode_number"): e for e in (english.get("episodes") or [])}

    title = clean(data.get("name"), MAX_TITLE) or clean(english.get("name"), MAX_TITLE)
    if not title:
        title = "Спецвыпуски" if number == 0 else f"Сезон {number}"
    original_title = clean(english.get("name"), MAX_TITLE)
    poster_path = data.get("poster_path")
    width = height = None
    if poster_path:
        width, height = image_size((data.get("images") or {}).get("posters") or [], poster_path)

    episodes = []
    today = date.today().isoformat()
    for episode in (data.get("episodes") or [])[:MAX_EPISODES]:
        episode_number = episode.get("episode_number")
        if not isinstance(episode_number, int) or episode_number < 0:
            continue
        en = en_episodes.get(episode_number) or {}
        runtime = episode.get("runtime") or en.get("runtime")
        still_path = episode.get("still_path") or en.get("still_path")
        still_width, still_height = (None, None)
        if still_path:
            still_width, still_height = image_size(
                (data.get("images") or {}).get("stills") or [], still_path)
        en_title = clean(en.get("name"), MAX_TITLE)
        episodes.append({
            "number": episode_number,
            "title": clean(episode.get("name"), MAX_TITLE) or en_title,
            "original_title": en_title,
            "overview": clean(episode.get("overview"), MAX_OVERVIEW)
                        or clean(en.get("overview"), MAX_OVERVIEW),
            "air_date": episode.get("air_date") or None,
            "runtime_minutes": runtime if isinstance(runtime, int)
                               and 1 <= runtime <= MAX_RUNTIME else None,
            "still_url": f"{IMAGE}{still_path}" if still_path else None,
            "still_width": still_width,
            "still_height": still_height,
        })

    # Дата выхода последней **вышедшей** серии: у будущих серий дата есть, но финалом она не стала.
    aired = [e["air_date"] for e in episodes if e["air_date"] and e["air_date"] <= today]
    metadata = {
        "number": number,
        "episode_count": len(episodes) or None,
        # Сезон завершён, если все его серии уже вышли; иначе — статус сериала.
        "status": "ended" if episodes and len(aired) == len(episodes) else series_status,
        "finale_date": max(aired) if aired else None,
    }
    return {
        "number": number,
        "title": title,
        "original_title": original_title if original_title and original_title != title else None,
        "description": clean(data.get("overview"), MAX_DESCRIPTION)
                       or clean(english.get("overview"), MAX_DESCRIPTION),
        "release_date": data.get("air_date") or None,
        "cover_url": f"{IMAGE}{poster_path}" if poster_path else None,
        "cover_width": width,
        "cover_height": height,
        "metadata": {k: v for k, v in metadata.items() if v is not None},
        "external_ids": {"tmdb_tv_season": f"{tv_id}:{number}"},
        "episodes": episodes,
    }


# ---------------------------------------------------------------- DB


def series_list(conn, slugs: list[str], limit: int | None) -> list[tuple]:
    """Сериалы с id TMDB: (id, slug, (external_ids->>'tmdb_tv')::int)."""
    sql = ["SELECT id, slug, (external_ids ->> 'tmdb_tv')::int FROM entities",
           "WHERE kind = 'series' AND external_ids ? 'tmdb_tv'"]
    params: list = []
    if slugs:
        sql.append("AND slug = ANY(%s)")
        params.append(slugs)
    sql.append("ORDER BY created_at")
    if limit:
        sql.append("LIMIT %s")
        params.append(limit)
    with conn.cursor() as cur:
        cur.execute(" ".join(sql), params)
        return cur.fetchall()


def free_slug(cur, base: str, season_id: str | None) -> str:
    """`arcane-season-2`, занятый — с суффиксом. Slug уже записанного сезона не трогаем."""
    for candidate in [base] + [f"{base}-{n}" for n in range(2, 12)]:
        cur.execute("SELECT id FROM entities WHERE slug = %s", (candidate,))
        row = cur.fetchone()
        if not row or (season_id and row[0] == season_id):
            return candidate
    return f"{base}-{int(time.time())}"


def save(conn, series_id: str, series_slug: str, row: dict, refresh: bool) -> str:
    """Сезон и его серии в одной транзакции: 'written', 'updated' или 'kept'."""
    with conn, conn.cursor() as cur:
        cur.execute(
            "SELECT id FROM entities WHERE parent_id = %s"
            " AND (metadata ->> 'number')::int = %s",
            (series_id, row["number"]),
        )
        found = cur.fetchone()
        if found and not refresh:
            # Серии всё равно досылаем: у идущего сезона их прибавляется.
            save_episodes(cur, found[0], row["episodes"])
            return "kept"
        values = (
            row["title"], row["original_title"], row["description"], row["release_date"],
            row["cover_url"], row["cover_width"], row["cover_height"],
            json.dumps(row["metadata"], ensure_ascii=False),
            json.dumps(row["external_ids"]),
        )
        if found:
            season_id = found[0]
            cur.execute(
                """UPDATE entities SET title = %s, original_title = %s, description = %s,
                       release_date = %s, cover_url = %s, cover_width = %s, cover_height = %s,
                       metadata = %s::jsonb, external_ids = %s::jsonb
                   WHERE id = %s""",
                (*values, season_id),
            )
            result = "updated"
        else:
            # Slug сезона генерится один раз и дальше живёт самостоятельно: переименование
            # сериала его не меняет.
            slug = free_slug(cur, f"{series_slug}-season-{row['number']}", None)
            cur.execute(
                """INSERT INTO entities (kind, slug, parent_id, title, original_title,
                       description, release_date, cover_url, cover_width, cover_height,
                       metadata, external_ids)
                   VALUES ('season', %s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
                   RETURNING id""",
                (slug, series_id, *values),
            )
            season_id = cur.fetchone()[0]
            result = "written"
        save_episodes(cur, season_id, row["episodes"])
    return result


def save_episodes(cur, season_id: str, episodes: list[dict]) -> None:
    """Серии сезона: повторный прогон обновляет строку с тем же номером, а не множит её."""
    for episode in episodes:
        cur.execute(
            """INSERT INTO episodes (season_id, number, title, original_title, overview,
                   air_date, runtime_minutes, still_url, still_width, still_height)
               VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
               ON CONFLICT (season_id, number) DO UPDATE SET
                   title = EXCLUDED.title, original_title = EXCLUDED.original_title,
                   overview = EXCLUDED.overview, air_date = EXCLUDED.air_date,
                   runtime_minutes = EXCLUDED.runtime_minutes, still_url = EXCLUDED.still_url,
                   still_width = EXCLUDED.still_width, still_height = EXCLUDED.still_height""",
            (season_id, episode["number"], episode["title"], episode["original_title"],
             episode["overview"], episode["air_date"], episode["runtime_minutes"],
             episode["still_url"], episode["still_width"], episode["still_height"]),
        )


# ---------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--slug", nargs="*", default=[],
                        help="только эти сериалы (по умолчанию — все с id TMDB)")
    parser.add_argument("--limit", type=int, help="сколько сериалов обойти")
    parser.add_argument("--refresh", action="store_true",
                        help="перезаписать уже заведённые сезоны (по умолчанию — только серии)")
    parser.add_argument("--delay", type=float, default=DELAY,
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
    if not dsn:
        log.error("DATABASE_URL is not set")
        return 2

    tmdb = Tmdb(key, args.delay)
    conn = psycopg2.connect(dsn)
    stats = {"written": 0, "updated": 0, "kept": 0, "skipped": 0, "episodes": 0}
    touched: list[str] = []
    try:
        series = series_list(conn, args.slug, args.limit)
        log.info("сериалов с id TMDB: %d", len(series))
        for series_id, series_slug, tv_id in series:
            data = tmdb.get(f"/tv/{tv_id}", language=LANGUAGE)
            if not data:
                log.warning("%s: сериала %s нет у TMDB", series_slug, tv_id)
                stats["skipped"] += 1
                continue
            series_status = STATUS.get(data.get("status") or "")
            numbers = sorted({
                season.get("season_number") for season in (data.get("seasons") or [])
                if isinstance(season.get("season_number"), int)
                and 0 <= season["season_number"] <= MAX_SEASON_NUMBER
            })
            log.debug("%s: сезоны %s", series_slug, numbers)
            for number in numbers:
                row = season_rows(tmdb, tv_id, number, series_status)
                if not row:
                    stats["skipped"] += 1
                    continue
                if args.dry_run:
                    result = "written"
                else:
                    result = save(conn, series_id, series_slug, row, args.refresh)
                stats[result] += 1
                stats["episodes"] += len(row["episodes"])
                if series_slug not in touched:
                    touched.append(series_slug)
                log.debug("%s %s-season-%s: %d серий", result, series_slug, number,
                          len(row["episodes"]))
    except KeyboardInterrupt:
        log.warning("interrupted")
    except RuntimeError as error:
        log.error("%s", error)
        return 1
    finally:
        conn.close()
    log.info("done: %s", stats)
    if touched and not args.dry_run:
        # Сезоны не индексируются, но страницы сериалов надо пересобрать: в шапке переключатель.
        log.info("rebuild their pages: %s", " ".join(f"/series/{slug}" for slug in touched))
    return 0


if __name__ == "__main__":
    sys.exit(main())
