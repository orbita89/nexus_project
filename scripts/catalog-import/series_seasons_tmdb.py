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
    still_width/height ← размеры этой копии кадра, только с `--still-sizes`: у TMDB они есть
                      лишь в `/tv/{id}/season/{n}/episode/{m}/images`, то есть запрос на серию

Повторный прогон безопасен и идемпотентен: сезон пишется `INSERT ... ON CONFLICT (slug)` по
детерминированному слагу `{сериал}-season-{n}`, серия — `ON CONFLICT (season_id, number)`.
Занятый чужой сущностью слаг не роняет прогон: такой сезон уходит в «остаток» и получает слаг с
суффиксом. Серии, которых больше нет у TMDB, не удаляются — на них могут стоять ссылки; снять их
можно `PUT /admin/seasons/{id}/episodes`.

Рассчитан на массовый прогон (сотни тысяч сериалов):

    * сеть и БД разведены — воркеры только качают, пишет отдельный поток пачками в одной
      транзакции (`--db-batch`), очередь с бэкпрешером;
    * очередь задач плоская, уровня сезона: сериал с сотней сезонов не держит поток;
    * скорость задаёт `--rps` (token bucket), а не число потоков; на 429 все потоки встают
      разом по `Retry-After`, лимит снижается и затем восстанавливается;
    * `--checkpoint`: уже залитые сериалы пропускаются при перезапуске, переживает `kill -9`;
    * SIGINT — дописать очередь и выйти без потери пачки.

Ключ — `TMDB_API_KEY` из окружения, корневого `.env` или `infra/.env` (v3-ключ или v4-токен).

Запуск:
    python3 -m venv scripts/catalog-import/.venv
    scripts/catalog-import/.venv/bin/pip install -r scripts/catalog-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/catalog-import/.venv/bin/python \\
        scripts/catalog-import/series_seasons_tmdb.py \\
        [--slug arcane ...] [--limit 100] [--workers 48] [--rps 45] [--refresh] [--dry-run] [-v]

Сезоны в Meilisearch не индексируются (в разделе «Сериалы» должен быть «Аркейн», а не его
сезоны), поэтому `scripts/index-entities.sh` после прогона не нужен — нужна пересборка статики
сериалов на фронтенде.
"""

import argparse
import json
import logging
import os
import queue
import random
import re
import signal
import sys
import threading
import time
import unicodedata
from collections import deque
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from datetime import date
from pathlib import Path

import psycopg2
import psycopg2.extras
import requests

ROOT = Path(__file__).resolve().parents[2]
TMDB = "https://api.themoviedb.org/3"
IMAGE = "https://image.tmdb.org/t/p/w780"
IMAGE_WIDTH = 780
LANGUAGE = "ru-RU"
FALLBACK_LANGUAGE = "en-US"
CHECKPOINT = Path(__file__).resolve().parent / "cache" / "seasons.checkpoint"

# Ограничения `catalog::metadata`, `catalog::validate` и `catalog::admin::episodes`.
MAX_TITLE = 300
MAX_DESCRIPTION = 20_000
MAX_OVERVIEW = 2000
MAX_SEASON_NUMBER = 10_000
MAX_EPISODES = 1000
MAX_RUNTIME = 10_000
RETRIES = 7
CONNECT_TIMEOUT = 5
READ_TIMEOUT = 15
# Доля серий без русского названия, с которой есть смысл во втором запросе в en-US.
ENGLISH_SHARE = 0.3

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


class Limiter:
    """Token bucket: общий потолок запросов в секунду на все потоки.

    Скорость задаётся лимитом, а не числом воркеров, поэтому потоков можно ставить много —
    лишние просто ждут. На 429 лимит снижается (AIMD) и восстанавливается по секунде.
    """

    def __init__(self, rps: float):
        self.base = max(0.1, rps)
        self.rps = self.base
        self.allowance = self.base
        self.last = time.monotonic()
        self.lock = threading.Lock()
        self.resume_at = 0.0  # пауза для всех потоков после 429

    def take(self) -> None:
        while True:
            with self.lock:
                now = time.monotonic()
                if now < self.resume_at:
                    pause = self.resume_at - now
                else:
                    self.allowance = min(self.rps, self.allowance + (now - self.last) * self.rps)
                    self.last = now
                    if self.allowance >= 1.0:
                        self.allowance -= 1.0
                        # Медленное восстановление после снижения.
                        if self.rps < self.base:
                            self.rps = min(self.base, self.rps + self.base * 0.01)
                        return
                    pause = (1.0 - self.allowance) / self.rps
            time.sleep(min(pause, 1.0))

    def back_off(self, retry_after: float) -> None:
        """429: притормозить всех и снизить лимит на 20%."""
        with self.lock:
            self.resume_at = max(self.resume_at, time.monotonic() + retry_after)
            self.rps = max(self.base * 0.1, self.rps * 0.8)


class Tmdb:
    """Клиент TMDB с повторами: v4-токен (`eyJ…`) идёт заголовком, v3-ключ — параметром."""

    def __init__(self, key: str, limiter: Limiter, workers: int = 16, verify: bool = True):
        self.session = requests.Session()
        pool = max(32, workers * 2)
        adapter = requests.adapters.HTTPAdapter(pool_connections=pool, pool_maxsize=pool)
        self.session.mount("https://", adapter)
        self.session.mount("http://", adapter)
        self.session.verify = verify
        self.limiter = limiter
        self.stats = {"requests": 0, "retry_429": 0, "retry_ssl": 0, "retry_timeout": 0,
                      "retry_other": 0}
        self.lock = threading.Lock()
        self.params = {}
        if key.startswith("eyJ"):
            self.session.headers["Authorization"] = f"Bearer {key}"
        else:
            self.params["api_key"] = key

    def count(self, key: str) -> None:
        with self.lock:
            self.stats[key] += 1

    def get(self, path: str, **params) -> dict | None:
        """Ответ TMDB; 404 — `None` (сезон скрыт или удалён)."""
        for attempt in range(RETRIES + 1):
            self.limiter.take()
            kind = "retry_other"
            try:
                self.count("requests")
                response = self.session.get(
                    f"{TMDB}{path}", timeout=(CONNECT_TIMEOUT, READ_TIMEOUT),
                    params={**self.params, **params})
                if response.status_code == 404:
                    return None
                if response.status_code == 429:
                    retry_after = float(response.headers.get("Retry-After") or 1)
                    self.limiter.back_off(retry_after)
                    kind = "retry_429"
                    raise RuntimeError("HTTP 429")
                if response.status_code >= 500:
                    raise RuntimeError(f"HTTP {response.status_code}")
                response.raise_for_status()
                return response.json()
            except (requests.RequestException, ValueError, RuntimeError) as error:
                if kind != "retry_429":
                    if isinstance(error, requests.exceptions.SSLError):
                        kind = "retry_ssl"
                    elif isinstance(error, requests.exceptions.Timeout):
                        kind = "retry_timeout"
                self.count(kind)
                if attempt == RETRIES:
                    raise RuntimeError(f"{path}: {error}") from error
                # Экспоненциально с джиттером: ошибки прокси и таймауты транзиентные,
                # ждать по пять секунд нет смысла. На 429 паузу уже держит лимитер.
                pause = min(8.0, 0.25 * 2 ** attempt) * (0.5 + random.random())
                log.debug("%s: %s — повтор через %.1f с", path, error, pause)
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


def still_size(tmdb: Tmdb, tv_id: int, season: int, episode: int,
               still_path: str) -> tuple[int | None, int | None]:
    """Размеры кадра: TMDB отдаёт их только у эпизода (`/images`) — запрос на серию.

    Поэтому и флаг `--still-sizes`: на сотне сериалов это ~10 тысяч лишних запросов. Без него
    размеры остаются пустыми, и карточка берёт обычное для кадра соотношение 16:9.
    """
    data = tmdb.get(f"/tv/{tv_id}/season/{season}/episode/{episode}/images")
    return image_size((data or {}).get("stills") or [], still_path)


def season_rows(tmdb: Tmdb, tv_id: int, number: int, series_status: str | None,
                with_still_sizes: bool = False) -> dict | None:
    """Сезон и его серии из `GET /tv/{id}/season/{n}`; нечего показывать — `None`.

    Второй запрос в en-US — ради оригинальных названий и описаний там, где русских нет:
    у TMDB `name` сезона и серии локализованы, оригинала в ответе нет. Делается, только если
    русского заметно не хватает (пустые имя/описание сезона или больше трети серий без имени) —
    иначе на массовом прогоне он удваивает трафик почти без пользы.
    """
    data = tmdb.get(f"/tv/{tv_id}/season/{number}", language=LANGUAGE,
                    append_to_response="images", include_image_language="ru,en,null")
    if not data:
        return None
    ru_episodes = data.get("episodes") or []
    nameless = sum(1 for e in ru_episodes if not clean(e.get("name"), MAX_TITLE))
    needs_english = (not clean(data.get("name"), MAX_TITLE)
                     or not clean(data.get("overview"), MAX_DESCRIPTION)
                     or (ru_episodes and nameless / len(ru_episodes) > ENGLISH_SHARE))
    english = tmdb.get(f"/tv/{tv_id}/season/{number}",
                       language=FALLBACK_LANGUAGE) if needs_english else {}
    english = english or {}
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
        # В ответе сезона размеров кадров нет (`images` сезона — только постеры), поэтому
        # без `--still-sizes` они остаются пустыми.
        still_width, still_height = (None, None)
        if still_path and with_still_sizes:
            still_width, still_height = still_size(tmdb, tv_id, number, episode_number, still_path)
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


def series_list(conn, slugs: list[str], limit: int | None, missing_only: bool = False,
                after_id: str | None = None) -> list[tuple]:
    """Сериалы с id TMDB: (id, slug, (external_ids->>'tmdb_tv')::int)."""
    sql = ["SELECT s.id, s.slug, (s.external_ids ->> 'tmdb_tv')::int FROM entities s",
           "WHERE s.kind = 'series' AND s.external_ids ? 'tmdb_tv'"]
    params: list = []
    if slugs:
        sql.append("AND s.slug = ANY(%s)")
        params.append(slugs)
    if missing_only:
        sql.append("AND NOT EXISTS (SELECT 1 FROM entities season"
                   " WHERE season.parent_id = s.id AND season.kind = 'season')")
    if after_id:
        sql.append("AND s.id > %s")
        params.append(after_id)
    # Порядок устойчив: при равных created_at (массовый импорт) id разводит строки,
    # иначе «продолжить с места» не работает.
    sql.append("ORDER BY s.created_at, s.id")
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


SEASON_COLUMNS = """INSERT INTO entities (kind, slug, parent_id, title, original_title,
        description, release_date, cover_url, cover_width, cover_height, metadata, external_ids)
    VALUES %s"""
SEASON_TEMPLATE = "('season', %s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)"
SEASON_UPDATE = """ DO UPDATE SET title = EXCLUDED.title,
        original_title = EXCLUDED.original_title, description = EXCLUDED.description,
        release_date = EXCLUDED.release_date, cover_url = EXCLUDED.cover_url,
        cover_width = EXCLUDED.cover_width, cover_height = EXCLUDED.cover_height,
        metadata = EXCLUDED.metadata, external_ids = EXCLUDED.external_ids
    WHERE entities.kind = 'season' AND entities.parent_id = EXCLUDED.parent_id"""


def season_values(series_id: str, row: dict) -> tuple:
    return (
        f"{row['series_slug']}-season-{row['number']}", series_id,
        row["title"], row["original_title"], row["description"], row["release_date"],
        row["cover_url"], row["cover_width"], row["cover_height"],
        json.dumps(row["metadata"], ensure_ascii=False), json.dumps(row["external_ids"]),
    )


class Writer(threading.Thread):
    """Пишет сезоны пачками в своём потоке: сеть не ждёт БД, БД не ждёт сеть.

    Очередь с ограничением — бэкпрешер: отстаёт БД, притормаживают и качальщики, память не растёт.
    Пачка идёт одной транзакцией; упала — перезаписывается по одному сезону, чтобы один битый
    сезон не уносил с собой остальные.
    """

    def __init__(self, dsn: str, batch: int = 200, refresh: bool = False,
                 fast_commit: bool = False, checkpoint: Path | None = None,
                 maxsize: int = 256):
        super().__init__(daemon=True, name="writer")
        self.conn = psycopg2.connect(dsn)
        self.conn.autocommit = False
        self.queue: queue.Queue = queue.Queue(maxsize=maxsize)
        self.batch = batch
        self.refresh = refresh
        self.fast_commit = fast_commit
        self.checkpoint = checkpoint
        self.stats = {"written": 0, "updated": 0, "kept": 0, "episodes": 0, "failed": 0,
                      "conflict": 0}
        self.lock = threading.Lock()
        self.error: Exception | None = None

    # -------------------------------------------------- очередь

    def put(self, item: dict) -> None:
        self.queue.put(item)

    def close(self) -> None:
        self.queue.put(None)

    def snapshot(self) -> dict:
        with self.lock:
            return dict(self.stats)

    def bump(self, key: str, value: int = 1) -> None:
        with self.lock:
            self.stats[key] += value

    # -------------------------------------------------- цикл

    def run(self) -> None:
        done = False
        while not done:
            items: list[dict] = []
            try:
                first = self.queue.get(timeout=0.5)
            except queue.Empty:
                continue
            if first is None:
                break
            items.append(first)
            while len(items) < self.batch:
                try:
                    item = self.queue.get_nowait()
                except queue.Empty:
                    break
                if item is None:
                    done = True
                    break
                items.append(item)
            try:
                self.flush(items)
            except Exception as error:  # noqa: BLE001 — писатель не должен умирать
                self.error = error
                log.error("writer: %s", error)
                self.conn.rollback()

    def flush(self, items: list[dict]) -> None:
        rows = [item for item in items if item.get("row")]
        if rows:
            try:
                self.write_batch(rows)
            except psycopg2.Error as error:
                self.conn.rollback()
                log.warning("пачка из %d сезонов не прошла (%s) — по одному", len(rows), error)
                for item in rows:
                    try:
                        self.write_batch([item])
                    except psycopg2.Error as single:
                        self.conn.rollback()
                        self.bump("failed")
                        log.warning("%s season %s: %s", item["series_slug"],
                                    item["row"]["number"], single)
        self.mark_done(items)

    def write_batch(self, items: list[dict]) -> None:
        with self.conn.cursor() as cur:
            if self.fast_commit:
                cur.execute("SET LOCAL synchronous_commit = off")
            values = [season_values(item["series_id"], item["row"]) for item in items]
            conflict = SEASON_UPDATE if self.refresh else " DO NOTHING"
            # fetch=True — иначе при разбиении на страницы вернулись бы строки только последней.
            returned = psycopg2.extras.execute_values(
                cur, f"{SEASON_COLUMNS} ON CONFLICT (slug){conflict} RETURNING id, slug",
                values, template=SEASON_TEMPLATE, page_size=max(1, len(values)), fetch=True)
            ids = {slug: season_id for season_id, slug in returned}
            self.bump("updated" if self.refresh else "written", len(ids))

            # Сезоны, которых upsert не вернул: слаг уже занят. Чаще всего это тот же сезон с
            # прошлого прогона — берём его id; если слаг занят чужой сущностью, заводим сезон
            # с суффиксом (единицы случаев на сотни тысяч).
            rest = [(item, value) for item, value in zip(items, values) if value[0] not in ids]
            if rest:
                self.resolve(cur, rest, ids)

            episodes: list[tuple] = []
            for item, value in zip(items, values):
                season_id = ids.get(value[0])
                if not season_id:
                    continue
                for episode in item["row"]["episodes"]:
                    episodes.append((
                        season_id, episode["number"], episode["title"],
                        episode["original_title"], episode["overview"], episode["air_date"],
                        episode["runtime_minutes"], episode["still_url"],
                        episode["still_width"], episode["still_height"]))
            if episodes:
                psycopg2.extras.execute_values(
                    cur,
                    """INSERT INTO episodes (season_id, number, title, original_title, overview,
                           air_date, runtime_minutes, still_url, still_width, still_height)
                       VALUES %s
                       ON CONFLICT (season_id, number) DO UPDATE SET
                           title = EXCLUDED.title, original_title = EXCLUDED.original_title,
                           overview = EXCLUDED.overview, air_date = EXCLUDED.air_date,
                           runtime_minutes = EXCLUDED.runtime_minutes,
                           still_url = EXCLUDED.still_url, still_width = EXCLUDED.still_width,
                           still_height = EXCLUDED.still_height""",
                    episodes, page_size=500)
                self.bump("episodes", len(episodes))
        self.conn.commit()

    def resolve(self, cur, rest: list[tuple], ids: dict) -> None:
        """Сезоны с занятым слагом: свой — переиспользуем, чужой — заводим с суффиксом."""
        slugs = [value[0] for _, value in rest]
        cur.execute("SELECT id, slug, parent_id, kind::text FROM entities WHERE slug = ANY(%s)",
                    (slugs,))
        existing = {slug: (season_id, parent_id, kind)
                    for season_id, slug, parent_id, kind in cur.fetchall()}
        for item, values in rest:
            slug = values[0]
            found = existing.get(slug)
            if found and found[2] == "season" and str(found[1]) == str(item["series_id"]):
                ids[slug] = found[0]  # тот же сезон: серии досылаем, сам сезон не трогаем
                self.bump("kept")
                continue
            # Слаг занят чужой сущностью (бывает: сериал и фильм с одинаковым названием).
            fresh = free_slug(cur, slug, None)
            cur.execute(
                """INSERT INTO entities (kind, slug, parent_id, title, original_title,
                       description, release_date, cover_url, cover_width, cover_height,
                       metadata, external_ids)
                   VALUES ('season', %s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
                   ON CONFLICT (slug) DO NOTHING
                   RETURNING id""",
                (fresh, *values[1:]))
            row = cur.fetchone()
            if row:
                ids[slug] = row[0]
                self.bump("written")
                self.bump("conflict")

    def mark_done(self, items: list[dict]) -> None:
        """Сериал закрыт — в чекпоинт: перезапуск его пропустит."""
        if not self.checkpoint:
            return
        finished = [str(item["done_tv_id"]) for item in items if item.get("done_tv_id")]
        if not finished:
            return
        with self.checkpoint.open("a", encoding="utf-8") as handle:
            handle.write("".join(f"{tv_id}\n" for tv_id in finished))
            handle.flush()


def load_checkpoint(path: Path) -> set[int]:
    if not path.exists():
        return set()
    done = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line.isdigit():
            done.add(int(line))
    return done


# ---------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--slug", nargs="*", default=[],
                        help="только эти сериалы (по умолчанию — все с id TMDB)")
    parser.add_argument("--limit", type=int, help="сколько сериалов обойти")
    parser.add_argument("--still-sizes", action="store_true",
                        help="дозапросить размеры кадров (по запросу на серию: ~10 тысяч "
                             "запросов на сотню сериалов); без него кадр идёт без размеров")
    parser.add_argument("--refresh", action="store_true",
                        help="перезаписать уже заведённые сезоны (по умолчанию — только серии)")
    parser.add_argument("--all", dest="missing_only", action="store_false", default=True,
                        help="обойти все сериалы, включая те, у которых уже есть сезоны "
                             "(по умолчанию — только без сезонов)")
    parser.add_argument("--after-id", help="продолжить с сериалов с id больше этого")
    parser.add_argument("--workers", type=int, default=48,
                        help="число параллельных потоков для загрузки TMDB")
    parser.add_argument("--rps", type=float, default=45.0,
                        help="потолок запросов к TMDB в секунду (у них лимит ~50)")
    parser.add_argument("--db-batch", type=int, default=200,
                        help="сколько сезонов писать одной транзакцией")
    parser.add_argument("--fast-commit", action="store_true",
                        help="synchronous_commit = off на время записи (импорт повторяем)")
    parser.add_argument("--checkpoint", default=str(CHECKPOINT),
                        help="файл уже залитых сериалов; '' — не вести")
    parser.add_argument("--insecure", action="store_true",
                        help="не проверять сертификат TMDB (MITM-прокси)")
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

    limiter = Limiter(args.rps)
    tmdb = Tmdb(key, limiter, workers=args.workers, verify=not args.insecure)
    conn = psycopg2.connect(dsn)
    checkpoint = Path(args.checkpoint) if args.checkpoint and not args.dry_run else None
    if checkpoint:
        checkpoint.parent.mkdir(parents=True, exist_ok=True)

    stop = threading.Event()

    def on_signal(_sig, _frame):
        if stop.is_set():
            raise KeyboardInterrupt
        log.warning("SIGINT: дописываю очередь, повтор — выход сразу")
        stop.set()

    signal.signal(signal.SIGINT, on_signal)

    writer = None
    stats = {"skipped": 0, "seasons": 0}
    touched: set[str] = set()
    started = time.monotonic()
    try:
        series = series_list(
            conn, args.slug, args.limit,
            missing_only=args.missing_only and not args.refresh and not args.slug,
            after_id=args.after_id)
        conn.close()
        if checkpoint:
            done = load_checkpoint(checkpoint)
            if done:
                before = len(series)
                series = [item for item in series if item[2] not in done]
                log.info("чекпоинт: пропущено %d сериалов", before - len(series))
        total = len(series)
        log.info("сериалов с id TMDB: %d", total)
        if not total:
            return 0

        if not args.dry_run:
            writer = Writer(dsn, batch=args.db_batch, refresh=args.refresh,
                            fast_commit=args.fast_commit, checkpoint=checkpoint,
                            maxsize=args.workers * 8)
            writer.start()

        def fetch_head(item):
            """Только список сезонов и статус — без локализации, ответ меньше."""
            series_id, series_slug, tv_id = item
            data = tmdb.get(f"/tv/{tv_id}")
            if not data:
                return series_id, series_slug, tv_id, None, []
            numbers = sorted({
                season.get("season_number") for season in (data.get("seasons") or [])
                if isinstance(season.get("season_number"), int)
                and 0 <= season["season_number"] <= MAX_SEASON_NUMBER
            })
            return series_id, series_slug, tv_id, STATUS.get(data.get("status") or ""), numbers

        def fetch_season(task):
            series_id, series_slug, tv_id, status, number = task
            row = season_rows(tmdb, tv_id, number, status, args.still_sizes)
            if row:
                row["series_slug"] = series_slug
            return series_id, series_slug, tv_id, row

        # Плоская очередь: задача — сезон, а не сериал. Сериал с сотней сезонов больше не
        # держит поток, и хвост прогона не простаивает.
        backlog: deque = deque()
        remaining: dict[int, int] = {}
        series_iter = iter(series)
        pending: dict = {}
        in_flight = args.workers * 4
        processed = 0
        last_log = time.monotonic()

        def emit(series_id, series_slug, tv_id, row):
            """Строку — писателю; последний сезон сериала несёт отметку для чекпоинта."""
            nonlocal processed
            left = remaining.get(tv_id, 0) - 1
            remaining[tv_id] = left
            item = {"series_id": series_id, "series_slug": series_slug, "row": row,
                    "done_tv_id": tv_id if left <= 0 else None}
            if left <= 0:
                remaining.pop(tv_id, None)
                processed += 1
            if row:
                stats["seasons"] += 1
                touched.add(series_slug)
            if writer:
                writer.put(item)

        with ThreadPoolExecutor(max_workers=args.workers) as executor:
            while True:
                while len(pending) < in_flight and not stop.is_set():
                    if backlog:
                        task = backlog.popleft()
                        pending[executor.submit(fetch_season, task)] = "season"
                        continue
                    item = next(series_iter, None)
                    if item is None:
                        break
                    pending[executor.submit(fetch_head, item)] = "head"
                if not pending:
                    break
                finished, _ = wait(list(pending), return_when=FIRST_COMPLETED)
                for future in finished:
                    kind = pending.pop(future)
                    try:
                        result = future.result()
                    except RuntimeError as error:
                        log.warning("%s", error)
                        stats["skipped"] += 1
                        continue
                    if kind == "head":
                        series_id, series_slug, tv_id, status, numbers = result
                        if not numbers:
                            stats["skipped"] += 1
                            processed += 1
                            if writer:
                                writer.put({"series_id": series_id, "series_slug": series_slug,
                                            "row": None, "done_tv_id": tv_id})
                            continue
                        remaining[tv_id] = len(numbers)
                        for number in numbers:
                            backlog.append((series_id, series_slug, tv_id, status, number))
                    else:
                        series_id, series_slug, tv_id, row = result
                        emit(series_id, series_slug, tv_id, row)
                now = time.monotonic()
                if now - last_log >= 10:
                    last_log = now
                    db = writer.snapshot() if writer else {}
                    speed = stats["seasons"] / max(0.001, now - started)
                    log.info(
                        "сериалов %d/%d · сезонов %d (%.1f/с) · БД: записано %d, обновлено %d, "
                        "оставлено %d, ошибок %d · сеть: %d запросов, 429 %d, ssl %d, таймаут %d",
                        processed, total, stats["seasons"], speed,
                        db.get("written", 0), db.get("updated", 0), db.get("kept", 0),
                        db.get("failed", 0), tmdb.stats["requests"], tmdb.stats["retry_429"],
                        tmdb.stats["retry_ssl"], tmdb.stats["retry_timeout"])
                if stop.is_set() and not pending:
                    break
    except KeyboardInterrupt:
        log.warning("interrupted")
    except RuntimeError as error:
        log.error("%s", error)
        return 1
    finally:
        if writer:
            writer.close()
            writer.join(timeout=300)
            writer.conn.close()
        if not conn.closed:
            conn.close()

    elapsed = time.monotonic() - started
    db = writer.snapshot() if writer else {}
    log.info("done за %.0f с: сезонов %d (%.1f/с), %s, сеть %s",
             elapsed, stats["seasons"], stats["seasons"] / max(0.001, elapsed), db, tmdb.stats)
    if touched and not args.dry_run:
        # Сезоны не индексируются, но страницы сериалов надо пересобрать: в шапке переключатель.
        shown = list(touched)[:50]
        log.info("rebuild their pages: %s%s", " ".join(f"/series/{slug}" for slug in shown),
                 "" if len(touched) <= 50 else f" … и ещё {len(touched) - 50}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
