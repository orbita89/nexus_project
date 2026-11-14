#!/usr/bin/env python3
"""Самые известные картины из Wikidata в каталог Nexus (раздел «Картины»).

Зачем Wikidata: у неё есть и сами полотна, и художники, и музеи, и изображение в Wikimedia
Commons — всё под свободной лицензией и с русскими названиями. «Топ-100» — по числу языковых
разделов Википедии (`wikibase:sitelinks`): у «Моны Лизы» их больше сотни, у случайного холста —
один-два. Это самый честный доступный прокси известности.

Картина — строка `entities` (kind = 'painting'), своих таблиц нет:

    title           ← русское название (нет — английское)
    original_title  ← английское, если отличается от русского
    description     ← вступление статьи ру-Википедии (до 20 000 символов), иначе описание Wikidata
    release_date    ← год создания P571 (точность до года — 1 января этого года)
    cover_url       ← изображение P18 через Commons `Special:FilePath/<файл>?width=1000`
                      (как логотипы премий, awards_logos.py: стабильный адрес, сам отдаёт копию
                      нужной ширины; у нас картинки не хранятся)
    cover_width,    ← размеры той самой копии (`imageinfo` Commons с `iiurlwidth`): карточка
    cover_height      получает точное `aspect-ratio` до загрузки файла, и стена картин не
                      перестраивается на лету
    metadata        ← {"technique", "museum", "city", "height_cm", "width_cm",
                      "image_url", "image_width", "image_height", "iiif_base"}
                      (схема `PaintingMetadata` в `catalog::metadata`)
    image_url       ← копия Commons около 3000 px по ширине (MediaWiki округляет запрос вверх
                      до стандартной «корзины», обычно 3840): исходник, по которому работает
                      зум и считаются кропы фрагментов. Обложка выдачи (1000 px) для этого мала —
                      кроп 0.09 × 0.12 по ней расплывается
    image_width,    ← размеры именно скачиваемой копии: ширина из адреса (`3840px-…`), высота
    image_height      по пропорциям оригинала
    iiif_base       ← базовый адрес IIIF Image API из манифеста Wikidata P6108, где он есть
                      (Рейксмюсеум, Метрополитен, Лувр): с ним фронтенд строит тайловый зум и
                      просит кроп у сервера, не скачивая оригинал
    external_ids    ← {"wikidata": "Q12418"}; индекс `entities_external_wikidata_uq`
    художник        ← P170 → `people` + `entity_credits` (role = 'artist'), как режиссёр у фильма
    теги            ← жанр P136 (kind = genre) и направление P135 (kind = tag); нет тега —
                      создаётся

Данные берутся в три шага, чтобы ни один запрос не был тяжёлым:
  1. SPARQL (WDQS) — список id картин по убыванию sitelinks;
  2. `wbgetentities` пачками по 50 — заявления и названия самих картин;
  3. `wbgetentities` пачками по 50 — названия того, на что они ссылаются (художник, музей,
     материал, жанр, направление), и `extracts` ру-Википедии для описаний.

Пропускаются: без изображения, без названия, уже записанный qid (повторный прогон безопасен),
не прошедшие ограничения `catalog::metadata` (строки до 200 символов, размеры 0,1–10 000 см).

Запуск:
    python3 -m venv scripts/catalog-import/.venv
    scripts/catalog-import/.venv/bin/pip install -r scripts/catalog-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        scripts/catalog-import/.venv/bin/python scripts/catalog-import/paintings_wikidata.py \\
        [--limit 100] [--min-sitelinks 15] [--pool N] [--dry-run] [-v]
Дозаполнить размеры обложек у картин, записанных до того, как они стали сохраняться:
    ... paintings_wikidata.py --fill-sizes
Дозаполнить исходники для зума и IIIF у записанных картин (`--all` — перечитать у всех):
    ... paintings_wikidata.py --fill-images
Затем — точечная индексация записанных картин (скрипт печатает готовую команду):
    scripts/index-entities.sh mona-lisa guernica ...
Полная перестройка (scripts/reindex-search.sh) для этого не нужна: она строит оба индекса
целиком и идёт десятки минут.
"""

import argparse
import json
import logging
import os
import re
import sys
import threading
import time
import unicodedata
from concurrent.futures import ThreadPoolExecutor, as_completed
from urllib.parse import quote, unquote, urlparse

import psycopg2
import requests

WDQS = "https://query.wikidata.org/sparql"
WIKIDATA_API = "https://www.wikidata.org/w/api.php"
RUWIKI_API = "https://ru.wikipedia.org/w/api.php"
COMMONS_FILEPATH = "https://commons.wikimedia.org/wiki/Special:FilePath/"
COMMONS_API = "https://commons.wikimedia.org/w/api.php"
COVER_WIDTH = 1000
# Исходник для зума и кропов фрагментов (`metadata.image_url`). Не оригинал Commons: у полотен
# это бывает 60-мегабайтный JPEG, который телефон не откроет. 3000 px по длинной стороне хватает,
# чтобы кроп 0.09 × 0.12 остался картинкой, а не кашей из двенадцати пикселей.
IMAGE_WIDTH = 3000
USER_AGENT = "nexus-catalog-import/1.0 (Nexus dev data import{}) python-requests".format(
    f"; {c}" if (c := os.environ.get("WIKIMEDIA_CONTACT")) else "")

# Ограничения `catalog::metadata` и `catalog::validate` — больше бэкенд не примет.
MAX_TEXT = 200
MAX_TITLE = 300
MAX_DESCRIPTION = 20_000
MIN_SIZE_CM, MAX_SIZE_CM = 0.1, 10_000.0
MAX_SLUG_BASE = 80
BATCH = 50
RETRIES = 5
# Сколько id брать у WDQS за один запрос и сколько картин обрабатывать за один проход (запрос
# заявлений, названий связанных сущностей, описаний, размеров и IIIF идёт пачкой на чанк: мельче —
# лишние round-trip, крупнее — прогон долго не доходит до первой записи в БД).
SPARQL_PAGE = 10_000
CHUNK = 150
# Верхняя полоса обхода: картин с таким числом языковых разделов немного, их берём одним запросом.
TOP_BAND = 40
# Потоки на манифесты IIIF: хостов много и они чужие, лимит Wikimedia сюда не относится.
IIIF_WORKERS = 16
# Потоки на пачки к API Викимедиа. Пачки независимы, а время уходит не на их счёт, а на дорогу:
# один round-trip через прокси ~1 с, и последовательные 50 пачек на чанк — это минута ожидания.
API_WORKERS = 1

# qid → название, на весь прогон. Чанки перекрываются по художникам, музеям и техникам почти
# полностью, и повторный запрос одного и того же стоил дороже всей остальной заливки.
NAMES: dict[str, str | None] = {}
# Пауза между запросами к API Викимедиа: без неё `wbgetentities` отвечает 429 уже на второй
# пачке. Переопределяется ключом --delay.
DELAY = 1.5

# Свойства Wikidata, которые читаем.
P_IMAGE = "P18"
P_INCEPTION = "P571"
P_CREATOR = "P170"
P_COLLECTION = "P195"
P_LOCATION = "P276"
P_MATERIAL = "P186"
P_GENRE = "P136"
P_MOVEMENT = "P135"
P_HEIGHT = "P2048"
# Манифест IIIF: по нему фронтенд строит глубокий зум и кропы фрагментов, не скачивая оригинал.
P_IIIF_MANIFEST = "P6108"
P_WIDTH = "P2049"

# Единицы длины Wikidata → множитель до сантиметров.
UNITS = {"Q174728": 1.0, "Q174789": 0.1, "Q11573": 100.0, "Q218593": 2.54}

log = logging.getLogger("paintings")

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
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].strip("-")


def clean(value: str | None, limit: int) -> str | None:
    value = re.sub(r"\s+", " ", (value or "")).strip()
    return value[:limit] if value else None


# ---------------------------------------------------------------- Wikidata


def http(session: requests.Session, method: str, url: str, delay: float = 0.0,
         retries: int = RETRIES, timeout: int = 120, **kwargs) -> dict:
    """Запрос с повторами: у Wikimedia бывают 429 и 503. `delay` — пауза перед запросом.

    `retries`/`timeout` переопределяются для чужих серверов: лестница пауз рассчитана на то, что
    Wikimedia пустит нас через минуту, а у музейного хоста с протухшим сертификатом ждать нечего.
    """
    if delay:
        time.sleep(delay)
    wikimedia = ".wikimedia.org" in url or ".wikidata.org" in url or ".wikipedia.org" in url
    limiter = limiter_for(url) if wikimedia else None
    retry_after = 0.0
    for attempt in range(retries + 1):
        try:
            if limiter:
                limiter.take()
            response = session.request(method, url, timeout=timeout, **kwargs)
            if response.status_code == 429:
                # Сервер сам говорит, сколько ждать (обычно ~10 с) — это точнее нашей лестницы.
                retry_after = min(float(response.headers.get("Retry-After") or 0) or 0, 60)
                if limiter:
                    limiter.penalize()
            if response.status_code in (429, 500, 502, 503, 504):
                raise RuntimeError(f"HTTP {response.status_code}")
            response.raise_for_status()
            if limiter:
                limiter.reward()
            return response.json()
        except (requests.RequestException, ValueError, RuntimeError) as error:
            if attempt == retries:
                raise RuntimeError(f"{url}: {error}") from error
            pause = retry_after or 10 * (attempt + 1)
            retry_after = 0.0
            log.warning("%s: %s — повтор через %s с", url, error, pause)
            time.sleep(pause)
    raise RuntimeError("unreachable")


def top_paintings(session: requests.Session, limit: int, min_sitelinks: int) -> list[str]:
    """Id картин по убыванию числа языковых разделов. Только с изображением: без него карточке
    нечего показывать, а искать картинку негде (у фильмов этим занимается `make media`)."""
    sparql = f"""SELECT ?item ?sitelinks WHERE {{
  ?item wdt:P31 wd:Q3305213 ; wdt:P18 ?image ; wikibase:sitelinks ?sitelinks .
  FILTER(?sitelinks >= {min_sitelinks})
}}
ORDER BY DESC(?sitelinks)
LIMIT {limit}"""
    data = http(session, "GET", WDQS, params={"query": sparql, "format": "json"})
    ids = []
    for row in data["results"]["bindings"]:
        qid = row["item"]["value"].rsplit("/", 1)[-1]
        if qid.startswith("Q"):
            ids.append(qid)
    return ids


class Limiter:
    """Общий на весь процесс темп запросов к Викимедиа.

    Лимит у них на клиента, а не на хост: 429 прилетает одновременно от wikidata.org,
    commons.wikimedia.org и ru.wikipedia.org. Поэтому паузы «между пачками» внутри каждого
    этапа не работали — считать надо все запросы вместе, одним счётчиком. На 429 темп
    снижается вдвое и затем медленно восстанавливается: так прогон сам находит потолок.
    """

    def __init__(self, rps: float):
        self.rps = self.target = rps
        # Ёмкость корзины — не меньше одного запроса: при темпе 0,5/с корзина на 0,5 токена
        # никогда не накопит целого, и take() ждёт вечно. Так прогон и зависал.
        self.allowance = self.capacity = max(1.0, rps)
        self.last = time.monotonic()
        self.good = 0
        self.lock = threading.Lock()

    def take(self) -> None:
        while True:
            with self.lock:
                now = time.monotonic()
                self.allowance = min(self.capacity,
                                     self.allowance + (now - self.last) * self.rps)
                self.last = now
                if self.allowance >= 1:
                    self.allowance -= 1
                    return
                wait = (1 - self.allowance) / self.rps
            time.sleep(wait)

    def penalize(self) -> None:
        with self.lock:
            self.rps = max(0.5, self.rps / 2)
            self.good = 0
            log.info("429: темп снижен до %.1f запросов/с", self.rps)

    def reward(self) -> None:
        with self.lock:
            self.good += 1
            if self.good >= 50 and self.rps < self.target:
                self.rps = min(self.target, self.rps * 1.2)
                self.good = 0


# Темп на каждую вики отдельно: лимит считает каждая своя, и общий счётчик зря держал бы
# Commons на скорости самого строгого. Замер (probe_rate): `wbgetentities` по 50 id отдаёт 429
# уже на 1 запросе в секунду, при 0,5 идёт ровно; imageinfo и extracts терпят вдвое больше.
LIMITERS = {
    # Чуть ниже замеренного потолка: 429 стоит 40 секунд ожидания по их Retry-After, и дешевле
    # не доезжать до него, чем упираться и ждать.
    "wikidata.org": Limiter(0.4),
    "commons.wikimedia.org": Limiter(1.0),
    "ru.wikipedia.org": Limiter(1.5),
}
LIMITER = Limiter(1.0)  # всё прочее в домене Викимедиа


def limiter_for(url: str) -> Limiter:
    for host, limiter in LIMITERS.items():
        if host in url:
            return limiter
    return LIMITER

_local = threading.local()


def api_session() -> requests.Session:
    """Сессия потока: `requests.Session` не потокобезопасен, а пул соединений на поток как раз и
    держит keep-alive к api.wikidata.org."""
    if not hasattr(_local, "session"):
        _local.session = requests.Session()
        _local.session.headers["User-Agent"] = USER_AGENT
    return _local.session


def fan_out(items: list, size: int, call) -> list:
    """Разбить `items` на пачки по `size` и выполнить на каждой `call(session, chunk)`.

    По умолчанию (`API_WORKERS = 1`) — последовательно, и это не упущение. Лимит Викимедиа
    считается на клиента, а не на хост и не на соединение: параллельные пачки упираются в тот же
    потолок, только приходят к нему в виде 429 и лестницы повторов. Темп держит `LIMITER`.
    """
    chunks = [items[start:start + size] for start in range(0, len(items), size)]
    if API_WORKERS <= 1 or len(chunks) <= 1:
        return [call(api_session(), chunk) for chunk in chunks]
    with ThreadPoolExecutor(max_workers=min(API_WORKERS, len(chunks))) as pool:
        return list(pool.map(lambda chunk: call(api_session(), chunk), chunks))


def _ids_page(session: requests.Session, where: str, offset: int, delay: float) -> list[str]:
    """Страница id картин по условию `where`. Порядок — по id: он стабилен между страницами,
    а `ORDER BY DESC(?sitelinks)` на больших окнах WDQS не доводит до конца (504)."""
    sparql = f"""SELECT ?item WHERE {{
  ?item wdt:P31 wd:Q3305213 ; wdt:P18 ?image .
  {where}
}}
ORDER BY ?item
LIMIT {SPARQL_PAGE} OFFSET {offset}"""
    data = http(session, "GET", WDQS, delay=delay, params={"query": sparql, "format": "json"})
    return [row["item"]["value"].rsplit("/", 1)[-1] for row in data["results"]["bindings"]
            if row["item"]["value"].rsplit("/", 1)[-1].startswith("Q")]


def sweep_paintings(session: requests.Session, min_sitelinks: int, ru: bool,
                    delay: float = 0.0, cache: str | None = None) -> list[str]:
    """Все картины от известных к неизвестным — для заливки раздела целиком.

    `top_paintings` берёт список одним запросом с `ORDER BY DESC(?sitelinks)`; на десятках тысяч
    строк WDQS на такой сортировке отваливается по таймауту. Поэтому идём полосами: отдельный
    запрос на каждое значение sitelinks сверху вниз (внутри полосы — страницами по id). Полосы
    заодно сохраняют нужный порядок: сначала «Мона Лиза», в конце — безымянные холсты.

    `ru` добавляет хвостом картины с русским названием, не дотянувшие до порога: раздел русский,
    и полотно из Третьяковки нужнее очередного provincial portrait с одной языковой ссылкой.

    Каждая полоса кладётся в свой файл в `cache`: WDQS на таких запросах регулярно отвечает 504,
    и сорванный сбор не должен стоить всех уже собранных полос.
    """
    ids: list[str] = []
    seen: set[str] = set()

    def take(where: str, what: str, key: str) -> None:
        path = os.path.join(cache, f"{key}.txt") if cache else None
        if path and os.path.exists(path):
            with open(path, encoding="utf-8") as fh:
                page_ids = [line.strip() for line in fh if line.strip()]
        else:
            page_ids, offset = [], 0
            while True:
                page = _ids_page(session, where, offset, delay)
                page_ids.extend(page)
                if len(page) < SPARQL_PAGE:
                    break
                offset += SPARQL_PAGE
            if path:
                os.makedirs(cache, exist_ok=True)
                tmp = f"{path}.tmp"
                with open(tmp, "w", encoding="utf-8") as fh:
                    fh.write("".join(f"{qid}\n" for qid in page_ids))
                os.replace(tmp, path)  # атомарно: оборванный файл не станет кэшем
        fresh = [q for q in page_ids if q not in seen]
        seen.update(fresh)
        ids.extend(fresh)
        log.info("Wikidata: %s — %d, всего %d", what, len(page_ids), len(ids))

    top = max(min_sitelinks, TOP_BAND)
    take(f"?item wikibase:sitelinks ?s . FILTER(?s >= {top})", f"sitelinks >= {top}", f"top{top}")
    for value in range(top - 1, min_sitelinks - 1, -1):
        take(f"?item wikibase:sitelinks ?s . FILTER(?s = {value})",
             f"sitelinks = {value}", f"s{value}")
    if ru:
        take(f'?item rdfs:label ?l . FILTER(lang(?l) = "ru") '
             f"?item wikibase:sitelinks ?s . FILTER(?s < {min_sitelinks})",
             "русское название ниже порога", f"ru-below{min_sitelinks}")
    return ids


def entities(session: requests.Session, ids: list[str], props: str, delay: float = DELAY) -> dict:
    """`wbgetentities` пачками по 50 (предел API), параллельно, с паузой внутри каждой.

    `session` берётся свой на поток (`fan_out`), аргумент оставлен ради вызовов из `fill_*`.
    """
    def call(s: requests.Session, chunk: list[str]) -> dict:
        return http(s, "POST", WIKIDATA_API, delay=delay, data={
            "action": "wbgetentities", "ids": "|".join(chunk), "props": props,
            "languages": "ru|en", "sitefilter": "ruwiki", "format": "json",
        })

    found: dict = {}
    for data in fan_out(ids, BATCH, call):
        found.update(data.get("entities") or {})
    return found


def label(entity: dict, lang: str) -> str | None:
    return ((entity.get("labels") or {}).get(lang) or {}).get("value")


def claims(entity: dict, prop: str) -> list[dict]:
    return [c for c in (entity.get("claims") or {}).get(prop, [])
            if (c.get("mainsnak") or {}).get("snaktype") == "value"]


def value_of(claim: dict):
    return claim["mainsnak"]["datavalue"]["value"]


def refs(entity: dict, prop: str) -> list[str]:
    """Id сущностей, на которые ссылается свойство (художник, музей, жанр…)."""
    out = []
    for claim in claims(entity, prop):
        value = value_of(claim)
        if isinstance(value, dict) and value.get("entity-type") == "item":
            out.append(value["id"])
    return out


def ref_current(entity: dict, prop: str) -> list[str]:
    """Как `refs`, но сперва заявление с рангом `preferred`, затем самые поздние: у картин с
    историей P195 перечисляет и прежние собрания («Музей в Люксембургском саду» у «Свободы»)."""
    items = [(c.get("rank"), value_of(c)) for c in claims(entity, prop)]
    items = [(rank, v["id"]) for rank, v in items
             if isinstance(v, dict) and v.get("entity-type") == "item"]
    preferred = [qid for rank, qid in items if rank == "preferred"]
    return preferred + [qid for rank, qid in reversed(items) if rank != "preferred"]


def year_of(entity: dict) -> int | None:
    """Год создания P571: в Wikidata это время вида `+1503-01-01T00:00:00Z`, иногда до н. э."""
    for claim in claims(entity, P_INCEPTION):
        value = value_of(claim)
        if not isinstance(value, dict):
            continue
        match = re.match(r"^\+(\d{4})-", value.get("time", ""))
        if match:
            year = int(match.group(1))
            if 1 <= year <= 2100:
                return year
    return None


def size_cm(entity: dict, prop: str) -> float | None:
    """Высота или ширина в сантиметрах; незнакомая единица — пропускаем."""
    for claim in claims(entity, prop):
        value = value_of(claim)
        if not isinstance(value, dict):
            continue
        unit = (value.get("unit") or "").rsplit("/", 1)[-1]
        factor = UNITS.get(unit)
        if factor is None:
            continue
        try:
            size = float(value["amount"]) * factor
        except (KeyError, ValueError):
            continue
        if MIN_SIZE_CM <= size <= MAX_SIZE_CM:
            return round(size, 1)
    return None


def cover_file(entity: dict) -> str | None:
    """Имя файла изображения P18 в Commons."""
    for claim in claims(entity, P_IMAGE):
        name = value_of(claim)
        if isinstance(name, str) and name.strip():
            return name.strip().replace(" ", "_")
    return None


def cover_of(name: str) -> str:
    """Имя файла Commons → адрес копии нужной ширины."""
    return f"{COMMONS_FILEPATH}{quote(name)}?width={COVER_WIDTH}"


def cover_sizes(session: requests.Session, names: list[str],
                delay: float = DELAY) -> dict[str, tuple[int, int]]:
    """Размеры тех самых копий шириной COVER_WIDTH, пачками по 50: `imageinfo` с `iiurlwidth`
    возвращает `thumbwidth`/`thumbheight` — ровно то, что получит браузер. У файла уже этой
    ширины или уже её (панорамы) миниатюры нет, тогда берём размеры оригинала."""
    def call(s: requests.Session, chunk: list[str]) -> dict:
        return http(s, "POST", COMMONS_API, delay=delay, data={
            # `thumbwidth`/`thumbheight` приходят только вместе с `iiprop=url`; с одним `size`
            # ответ — размеры оригинала, и в базу попадали бы они, а не скачиваемая копия.
            "action": "query", "prop": "imageinfo", "iiprop": "size|url",
            "iiurlwidth": COVER_WIDTH, "redirects": 1,
            "titles": "|".join(f"File:{name}" for name in chunk), "format": "json",
        })

    sizes = {}
    for data in fan_out(names, BATCH, call):
        for page in ((data.get("query") or {}).get("pages") or {}).values():
            info = (page.get("imageinfo") or [{}])[0]
            width = info.get("thumbwidth") or info.get("width")
            height = info.get("thumbheight") or info.get("height")
            title = (page.get("title") or "").removeprefix("File:").replace(" ", "_")
            if title and isinstance(width, int) and isinstance(height, int) \
                    and width > 0 and height > 0:
                sizes[title] = (width, height)
    return sizes


def image_copies(session: requests.Session, names: list[str],
                 delay: float = DELAY) -> dict[str, tuple[str, int, int]]:
    """Адрес и размеры копии около IMAGE_WIDTH, пачками по 50 — исходник для зума.

    Берём `thumburl` (прямой адрес на CDN Викимедиа), а не `Special:FilePath`: это та же копия
    без лишнего редиректа. Запрошенные `iiurlwidth` пикселей MediaWiki округляет вверх до
    стандартной «корзины» (3000 → 3840), и отдаёт файл именно этой ширины, а `thumbwidth` при
    этом остаётся запрошенным. Поэтому ширину читаем из самого адреса (`3840px-…`), а высоту
    считаем по пропорциям оригинала: в базе должны лежать размеры того файла, который скачает
    браузер, иначе фрагменты получат чужие координаты.

    Файл у́же запрошенного (миниатюры нет) — отдаём оригинал с его размерами.
    """
    def call(s: requests.Session, chunk: list[str]) -> dict:
        return http(s, "POST", COMMONS_API, delay=delay, data={
            "action": "query", "prop": "imageinfo", "iiprop": "size|url",
            "iiurlwidth": IMAGE_WIDTH, "redirects": 1,
            "titles": "|".join(f"File:{name}" for name in chunk), "format": "json",
        })

    copies = {}
    for data in fan_out(names, BATCH, call):
        for page in ((data.get("query") or {}).get("pages") or {}).values():
            info = (page.get("imageinfo") or [{}])[0]
            title = (page.get("title") or "").removeprefix("File:").replace(" ", "_")
            copy = copy_of(info)
            if title and copy:
                copies[title] = copy
    return copies


def commons_sizes(session: requests.Session, names: list[str],
                  delay: float = DELAY) -> tuple[dict[str, tuple[int, int]],
                                                 dict[str, tuple[str, int, int]]]:
    """Размеры обложки и большой копии — из размеров оригинала, одним лёгким запросом на 50 файлов.

    `cover_sizes` и `image_copies` просят `imageinfo` с `iiurlwidth`, а это не чтение, а заказ:
    Commons рендерит миниатюру каждого полотна и отвечает, только когда готова вся пачка. На
    свежих файлах (а у нас все свежие) это десятки секунд на пачку и главная статья расходов
    всего прогона.

    Между тем сами миниатюры при импорте никому не нужны — нужны их размеры, а MediaWiki
    масштабирует пропорционально, так что они считаются из размеров оригинала. Адреса строим
    через `Special:FilePath` с нужной шириной: тем же способом, что и `cover_url`, который так
    работает с первого прогона. Отрендерит их Commons лениво — когда первый читатель откроет
    картину, по одной, а не 47 тысяч разом.
    """
    sizes: dict[str, tuple[int, int]] = {}
    copies: dict[str, tuple[str, int, int]] = {}

    def call(s: requests.Session, chunk: list[str]) -> dict:
        return http(s, "POST", COMMONS_API, delay=delay, data={
            "action": "query", "prop": "imageinfo", "iiprop": "size", "redirects": 1,
            "titles": "|".join(f"File:{name}" for name in chunk), "format": "json",
        })

    for data in fan_out(names, BATCH, call):
        for page in ((data.get("query") or {}).get("pages") or {}).values():
            info = (page.get("imageinfo") or [{}])[0]
            title = (page.get("title") or "").removeprefix("File:").replace(" ", "_")
            full_width, full_height = info.get("width"), info.get("height")
            if not (title and isinstance(full_width, int) and isinstance(full_height, int)
                    and full_width > 0 and full_height > 0):
                continue
            for width, into in ((COVER_WIDTH, sizes), (IMAGE_WIDTH, copies)):
                # Оригинал у́же запрошенного — его и отдадут, увеличивать нечего.
                shown = min(width, full_width)
                height = max(1, round(full_height * shown / full_width))
                if into is sizes:
                    sizes[title] = (shown, height)
                else:
                    copies[title] = (f"{COMMONS_FILEPATH}{quote(title)}?width={shown}",
                                     shown, height)
    return sizes, copies


def copy_of(info: dict) -> tuple[str, int, int] | None:
    """`imageinfo` одного файла → (адрес, ширина, высота) скачиваемой копии."""
    full_width, full_height = info.get("width"), info.get("height")
    url = info.get("thumburl")
    if not url:
        # Миниатюры нет: файл и так не шире запрошенного.
        if info.get("url") and isinstance(full_width, int) and isinstance(full_height, int):
            return info["url"], full_width, full_height
        return None
    # Адрес без служебных параметров (`utm_*`, которые API добавляет к thumburl).
    url = url.split("?", 1)[0]
    width, height = info.get("thumbwidth"), info.get("thumbheight")
    bucket = re.search(r"/(\d+)px-[^/]*$", url)
    if bucket and isinstance(full_width, int) and isinstance(full_height, int) and full_width > 0:
        width = int(bucket.group(1))
        height = max(1, round(full_height * width / full_width))
    if isinstance(width, int) and isinstance(height, int) and width > 0 and height > 0:
        return url, width, height
    return None


def iiif_base_of(session: requests.Session, manifest_url: str, delay: float) -> str | None:
    """Базовый адрес IIIF Image API из манифеста: `{base}/{x},{y},{w},{h}/600,/0/default.jpg`.

    В манифесте лежит не он, а описание холстов, поэтому достаём `service` у первого
    изображения — и у Presentation 2 (`@id`), и у Presentation 3 (`id`). Манифест не открылся
    или без сервиса изображений — `None`, тогда у картины останется только `image_url`.
    """
    try:
        # Без повторов и с коротким таймаутом: манифест живёт на сервере музея, а не у Wikimedia.
        # Половина их отвечает 403, 404 или протухшим сертификатом, и лестница 10+20+30+40+50 с
        # на каждой такой картине обходилась дороже всей остальной заливки. IIIF — украшение
        # (зум тайлами), без него остаётся `image_url`; добрать его можно `--fill-images`.
        data = http(session, "GET", manifest_url, delay=delay, retries=0, timeout=10)
    except RuntimeError as error:
        log.debug("манифест %s: %s", manifest_url, error)
        return None
    services = []
    # Presentation 2: sequences → canvases → images → resource.service
    for sequence in (data.get("sequences") or []):
        for canvas in (sequence.get("canvases") or []):
            for image in (canvas.get("images") or []):
                services.append(((image.get("resource") or {}).get("service")))
    # Presentation 3: items → items → items → body.service
    for canvas in (data.get("items") or []):
        for page in (canvas.get("items") or []):
            for annotation in (page.get("items") or []):
                services.append(((annotation.get("body") or {}).get("service")))
    for service in services:
        for entry in (service if isinstance(service, list) else [service]):
            if isinstance(entry, dict):
                base = entry.get("@id") or entry.get("id")
                if isinstance(base, str) and base.startswith(("http://", "https://")):
                    # http:// переводим в https://: сайт у нас на https, и браузер заблокировал
                    # бы такие тайлы как mixed content — зум просто не открылся бы. Хост без
                    # https не хуже: по http он всё равно не загрузится.
                    return base.replace("http://", "https://", 1).rstrip("/")
    return None


def iiif_bases(session: requests.Session, entities_by_id: dict,
               delay: float = DELAY) -> dict[str, str]:
    """qid → базовый адрес IIIF у тех картин, у которых в Wikidata есть манифест (P6108).

    Манифесты тянем параллельно: это десятки разных музейных серверов, очередь к ним не нужна
    (пауза `delay` существует ради лимитов Wikimedia, а здесь мы не у Wikimedia), а медленный
    хост не должен задерживать остальные.
    """
    wanted: dict[str, str] = {}
    for qid, entity in entities_by_id.items():
        for claim in claims(entity, P_IIIF_MANIFEST):
            url = value_of(claim)
            if isinstance(url, str) and url.startswith(("http://", "https://")):
                wanted[qid] = url
            break
    if not wanted:
        return {}
    bases: dict[str, str] = {}
    with ThreadPoolExecutor(max_workers=IIIF_WORKERS) as pool:
        # `api_session()` зовётся внутри задачи: вычисли её здесь — и все потоки получили бы
        # одну сессию главного потока, а она не потокобезопасна.
        futures = {pool.submit(lambda u: iiif_base_of(api_session(), u, 0.0), url): qid
                   for qid, url in wanted.items()}
        for future in as_completed(futures):
            try:
                if base := future.result():
                    bases[futures[future]] = base
            except Exception as error:  # чужой сервер не должен ронять прогон
                log.debug("манифест %s: %s", wanted[futures[future]], error)
    return bases


def ruwiki_titles(entities_by_id: dict) -> dict[str, str]:
    """qid → заголовок статьи ру-Википедии (для описания)."""
    titles = {}
    for qid, entity in entities_by_id.items():
        site = (entity.get("sitelinks") or {}).get("ruwiki")
        if site and site.get("title"):
            titles[qid] = site["title"]
    return titles


def extracts(session: requests.Session, titles: dict[str, str],
             delay: float = DELAY) -> dict[str, str]:
    """Вступления статей ру-Википедии пачками по 20: это описание карточки."""
    def call(s: requests.Session, chunk: list[str]) -> dict:
        return http(s, "POST", RUWIKI_API, delay=delay, data={
            "action": "query", "prop": "extracts", "exintro": 1, "explaintext": 1,
            "redirects": 1, "titles": "|".join(chunk), "format": "json",
        })

    by_title = {}
    for data in fan_out(list(titles.values()), 20, call):
        for page in (((data.get("query") or {}).get("pages")) or {}).values():
            if page.get("title") and page.get("extract"):
                by_title[page["title"]] = page["extract"]
    return {qid: by_title[title] for qid, title in titles.items() if title in by_title}


def row_of(qid: str, entity: dict, names: dict[str, str], description: str | None,
           sizes: dict[str, tuple[int, int]],
           images: dict[str, tuple[str, int, int]] | None = None,
           iiif: dict[str, str] | None = None) -> dict | None:
    """Строка каталога из записи Wikidata; `None` — нечего показывать."""
    title_ru, title_en = label(entity, "ru"), label(entity, "en")
    title = clean(title_ru or title_en, MAX_TITLE)
    file_name = cover_file(entity)
    if not title or not file_name:
        return None
    cover = cover_of(file_name)
    width, height = sizes.get(file_name, (None, None))
    original = clean(title_en, MAX_TITLE)
    year = year_of(entity)
    museum = next((names[q] for q in ref_current(entity, P_COLLECTION) if q in names), None)
    city = next((names[q] for q in ref_current(entity, P_LOCATION) if q in names), None)
    materials = [names[q] for q in refs(entity, P_MATERIAL) if q in names]
    # Исходник для зума и кропов фрагментов — отдельно от обложки выдачи.
    image_url, image_width, image_height = (images or {}).get(file_name, (None, None, None))
    metadata = {
        "image_url": image_url,
        "image_width": image_width,
        "image_height": image_height,
        "iiif_base": (iiif or {}).get(qid),
        "technique": clean(", ".join(materials), MAX_TEXT),
        "museum": clean(museum, MAX_TEXT),
        # P276 часто повторяет музей — тогда город не пишем, пусть поле останется пустым.
        "city": clean(city, MAX_TEXT) if city and city != museum else None,
        "height_cm": size_cm(entity, P_HEIGHT),
        "width_cm": size_cm(entity, P_WIDTH),
    }
    tags = []
    for prop, kind in ((P_GENRE, "genre"), (P_MOVEMENT, "tag")):
        for q in refs(entity, prop):
            name = clean(names.get(q), MAX_TEXT)
            slug = slugify(name or "")
            if name and slug and slug not in {t[0] for t in tags}:
                tags.append((slug, name[:100], kind))
    artists = []
    for q in refs(entity, P_CREATOR):
        name = clean(names.get(q), MAX_TEXT)
        if name:
            artists.append(name)
    return {
        "wikidata": qid,
        "title": title,
        "original_title": original if original and original != title else None,
        "description": clean(description, MAX_DESCRIPTION),
        "release_date": f"{year:04d}-01-01" if year else None,
        "cover_url": cover,
        "cover_width": width,
        "cover_height": height,
        "metadata": {k: v for k, v in metadata.items() if v is not None},
        "tags": tags,
        "artists": artists,
        "year": year,
    }


# ---------------------------------------------------------------- DB


def known_qids(conn) -> set[str]:
    with conn.cursor() as cur:
        cur.execute("SELECT external_ids ->> 'wikidata' FROM entities "
                    "WHERE external_ids ? 'wikidata'")
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


def person_id(cur, name: str) -> str | None:
    """Художник: уже известный по имени или новый. Тёзки здесь не разбираем — у картин их
    практически нет, а ручная правка в админке остаётся."""
    cur.execute("SELECT id FROM people WHERE full_name = %s ORDER BY created_at LIMIT 1", (name,))
    row = cur.fetchone()
    if row:
        return row[0]
    base = slugify(name) or "artist"
    slug = free_person_slug(cur, [base] + [f"{base}-{n}" for n in range(2, 12)])
    if not slug:
        return None
    cur.execute("INSERT INTO people (slug, full_name) VALUES (%s, %s) RETURNING id", (slug, name))
    return cur.fetchone()[0]


def free_person_slug(cur, candidates: list[str]) -> str | None:
    cur.execute("SELECT slug FROM people WHERE slug = ANY(%s)", (candidates,))
    taken = {row[0] for row in cur.fetchall()}
    return next((c for c in candidates if c not in taken), None)


def save(conn, row: dict) -> str:
    """Картина, её художники и теги — в одной транзакции: 'written' или 'duplicate'."""
    with conn, conn.cursor() as cur:
        base = slugify(row["original_title"] or row["title"])
        candidates = [c for c in (base, f"{base}-{row['year']}" if base and row["year"] else None)
                      if c]
        candidates.append(f"{base or 'painting'}-{row['wikidata'].lower()}")
        slug = free_slug(cur, candidates)
        if not slug:
            return "duplicate"
        cur.execute(
            """INSERT INTO entities (kind, slug, title, original_title, description, release_date,
                                     cover_url, cover_width, cover_height, metadata, external_ids)
               VALUES ('painting', %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s::jsonb)
               ON CONFLICT DO NOTHING RETURNING id""",
            (
                slug, row["title"], row["original_title"], row["description"],
                row["release_date"], row["cover_url"], row["cover_width"], row["cover_height"],
                json.dumps(row["metadata"], ensure_ascii=False),
                json.dumps({"wikidata": row["wikidata"]}),
            ),
        )
        inserted = cur.fetchone()
        if not inserted:
            return "duplicate"
        entity_id = inserted[0]
        # Slug нужен вызывающему: по нему картину потом досылают в поисковый индекс.
        row["slug"] = slug
        for position, name in enumerate(row["artists"]):
            person = person_id(cur, name)
            if person:
                cur.execute(
                    "INSERT INTO entity_credits (entity_id, person_id, role, position) "
                    "VALUES (%s, %s, 'artist', %s) ON CONFLICT DO NOTHING",
                    (entity_id, person, position),
                )
        for tag_slug, name, kind in row["tags"]:
            cur.execute(
                "INSERT INTO entity_tags (entity_id, tag_id) VALUES (%s, %s) "
                "ON CONFLICT DO NOTHING",
                (entity_id, tag_id(cur, tag_slug, name, kind)),
            )
    return "written"


def fill_sizes(conn, session: requests.Session, delay: float, dry_run: bool,
               every: bool = False, limit: int | None = None) -> int:
    """Дозаполнить `cover_width`/`cover_height` у картин, записанных до того, как размеры стали
    сохраняться (`every` — перечитать у всех, а не только у пустых). Имя файла Commons берётся
    из самого `cover_url`."""
    with conn.cursor() as cur:
        cur.execute("SELECT id, cover_url FROM entities WHERE kind = 'painting' "
                    "AND cover_url LIKE %s" + ("" if every else " AND cover_width IS NULL")
                    + (" LIMIT %s" if limit else ""),
                    (f"{COMMONS_FILEPATH}%", *((limit,) if limit else ())))
        rows = cur.fetchall()
    by_name: dict[str, list[str]] = {}
    for entity_id, url in rows:
        name = unquote(urlparse(url).path.rsplit("/", 1)[-1])
        by_name.setdefault(name, []).append(entity_id)
    log.info("дозаполнение размеров: %d картин, %d файлов", len(rows), len(by_name))
    sizes = cover_sizes(session, sorted(by_name), delay)
    filled = 0
    for name, ids in by_name.items():
        size = sizes.get(name)
        if not size:
            continue
        filled += len(ids)
        if dry_run:
            continue
        with conn, conn.cursor() as cur:
            cur.execute("UPDATE entities SET cover_width = %s, cover_height = %s "
                        "WHERE id = ANY(%s::uuid[])", (size[0], size[1], ids))
    return filled


def fill_images(conn, session: requests.Session, delay: float, dry_run: bool,
                every: bool = False, limit: int | None = None) -> int:
    """Дозаполнить `metadata.image_url` (исходник для зума) и `iiif_base` у записанных картин.

    Имя файла Commons берётся из `cover_url`, как в `fill_sizes`; манифест IIIF — по id Wikidata
    из `external_ids`. `every` — перечитать у всех, а не только у тех, где исходника ещё нет.
    """
    with conn.cursor() as cur:
        cur.execute(
            "SELECT id, cover_url, external_ids ->> 'wikidata' FROM entities "
            "WHERE kind = 'painting' AND cover_url LIKE %s"
            + ("" if every else " AND NOT (metadata ? 'image_url')")
            + (" LIMIT %s" if limit else ""),
            (f"{COMMONS_FILEPATH}%", *((limit,) if limit else ())))
        rows = cur.fetchall()
    by_name: dict[str, list[tuple[str, str | None]]] = {}
    for entity_id, url, qid in rows:
        name = unquote(urlparse(url).path.rsplit("/", 1)[-1])
        by_name.setdefault(name, []).append((entity_id, qid))
    log.info("дозаполнение исходников: %d картин, %d файлов", len(rows), len(by_name))
    copies = image_copies(session, sorted(by_name), delay)
    qids = sorted({qid for pairs in by_name.values() for _, qid in pairs if qid})
    found = entities(session, qids, "claims", delay) if qids else {}
    bases = iiif_bases(session, found, delay)
    log.info("IIIF: манифест есть у %d картин из %d", len(bases), len(qids))
    filled = 0
    for name, pairs in by_name.items():
        copy = copies.get(name)
        if not copy:
            continue
        url, width, height = copy
        for entity_id, qid in pairs:
            patch = {"image_url": url, "image_width": width, "image_height": height}
            if base := bases.get(qid or ""):
                patch["iiif_base"] = base
            filled += 1
            if dry_run:
                continue
            with conn, conn.cursor() as cur:
                cur.execute("UPDATE entities SET metadata = metadata || %s::jsonb "
                            "WHERE id = %s", (json.dumps(patch), entity_id))
    return filled


# ---------------------------------------------------------------- main


def read_checkpoint(path: str) -> set[str]:
    """Пройденные qid. Нужны не вместо `known_qids`, а вместе: в базу попадают только записанные,
    а отсеянные (без названия, без размеров) иначе перечитывались бы при каждом перезапуске."""
    try:
        with open(path, encoding="utf-8") as fh:
            return {line.strip() for line in fh if line.strip()}
    except FileNotFoundError:
        return set()


def append_checkpoint(path: str, qids: list[str]) -> None:
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("".join(f"{qid}\n" for qid in qids))
        fh.flush()
        os.fsync(fh.fileno())


def process(conn, session: requests.Session, chunk: list[str], args, limit: int,
            stats: dict, written_slugs: list[str]) -> None:
    """Один проход: от заявлений Wikidata до строк в БД. Запросы связанных данных (названия
    художников и музеев, описания, размеры, IIIF) идут пачками на весь чанк, а не на картину."""
    spent: dict[str, float] = {}

    def timed(name: str, call):
        started = time.monotonic()
        try:
            return call()
        finally:
            spent[name] = round(time.monotonic() - started, 1)
            log.info("  этап %s: %.1f с", name, spent[name])

    found = timed("картины", lambda: entities(session, chunk, "labels|claims|sitelinks",
                                              args.delay))
    linked = sorted({q for entity in found.values()
                     for prop in (P_CREATOR, P_COLLECTION, P_LOCATION, P_MATERIAL,
                                  P_GENRE, P_MOVEMENT)
                     for q in refs(entity, prop)})
    # Художники, музеи, техники и жанры повторяются от чанка к чанку: «холст, масло» стоит у
    # каждой второй картины, Лувр — у тысяч. Спрашиваем только тех, кого ещё не видели, —
    # на этом этапе уходило больше времени, чем на всё остальное вместе.
    unknown = [q for q in linked if q not in NAMES]
    files = sorted({name for entity in found.values() if (name := cover_file(entity))})
    # Этапы идут друг за другом: хосты разные, но лимит Викимедиа — общий, и разводить их по
    # потокам означало лишь быстрее упереться в 429. Темп держит LIMITER, он один на всех.
    fetched = timed("связанные", lambda: entities(session, unknown, "labels", args.delay))
    descriptions = timed("описания", lambda: extracts(session, ruwiki_titles(found), args.delay))
    sizes, images = timed("картинки", lambda: commons_sizes(session, files, args.delay))
    # IIIF там, где он есть у источника: с ним зум идёт тайлами, а кроп считает сервер.
    # В массовом прогоне его не трогаем: это десятки запросов на чанк к чужим музейным
    # серверам ради 3% попаданий. Добирается потом одним `--fill-images`.
    iiif = ({} if args.no_iiif
            else timed("iiif", lambda: iiif_bases(session, found, args.delay)))
    for qid, entity in fetched.items():
        NAMES[qid] = clean(label(entity, "ru") or label(entity, "en"), MAX_TEXT)
    for qid in unknown:  # не нашёлся — помним и это, иначе будем спрашивать его вечно
        NAMES.setdefault(qid, None)
    names = {qid: name for qid in linked if (name := NAMES.get(qid))}
    log.info("этапы, с: %s · IIIF у %d из %d", spent, len(iiif), len(found))
    write_started = time.monotonic()
    for qid in chunk:
        if limit and stats["written"] >= limit:
            return
        entity = found.get(qid)
        row = row_of(qid, entity, names, descriptions.get(qid), sizes, images, iiif) \
            if entity else None
        if not row:
            stats["skipped"] += 1
            continue
        result = "written" if args.dry_run else save(conn, row)
        stats[result] += 1
        if result == "written" and row.get("slug"):
            written_slugs.append(row["slug"])
        log.debug("%s %s (%s, %s)", result, row["title"], row["year"],
                  row["metadata"].get("museum"))
    log.info("запись в БД: %.1f с", time.monotonic() - write_started)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--limit", type=int, default=None,
                        help="сколько картин записать (по умолчанию 100); с --fill-sizes и "
                             "--fill-images — сколько картин обойти, по умолчанию все")
    parser.add_argument("--min-sitelinks", type=int, default=15,
                        help="нижняя граница известности: языковых разделов Википедии")
    parser.add_argument("--pool", type=int, default=None,
                        help="сколько id брать у Wikidata (по умолчанию --limit * 3). Нужно для "
                             "добора: самые известные уже записаны, и окно id надо раздвинуть "
                             "дальше них, не поднимая число записываемых")
    parser.add_argument("--sweep", action="store_true",
                        help="залить раздел целиком: обход полосами сверху вниз вместо одного "
                             "запроса «топ-N». С ним --limit по умолчанию без ограничения")
    parser.add_argument("--ru", action="store_true",
                        help="с --sweep: добрать картины с русским названием, не дотянувшие до "
                             "--min-sitelinks")
    parser.add_argument("--no-iiif", action="store_true",
                        help="не ходить за манифестами IIIF (музейные серверы медленные, "
                             "попаданий ~3%%); добрать потом ключом --fill-images")
    parser.add_argument("--ids-cache", default=None,
                        help="каталог, куда складывать собранные полосы id (обычно "
                             "cache/paintings-ids): перезапуск не пересобирает их у WDQS")
    parser.add_argument("--checkpoint", default=None,
                        help="файл пройденных qid; перезапуск продолжает с него "
                             "(обычно cache/paintings.checkpoint)")
    parser.add_argument("--delay", type=float, default=0.0,
                        help="дополнительная пауза перед каждым запросом, с (обычно не нужна: "
                             "темп держит общий лимитер --rps)")
    parser.add_argument("--rps", type=float, default=1.0,
                        help="множитель к темпу запросов (1 — замеренный потолок Викимедиа: "
                             "0,5 запроса/с к Wikidata, 1,5 к Commons и Википедии). На 429 "
                             "скрипт снижает темп сам и слушает Retry-After")
    parser.add_argument("--fill-sizes", action="store_true",
                        help="только дозаполнить cover_width/cover_height у записанных картин")
    parser.add_argument("--fill-images", action="store_true",
                        help="только дозаполнить metadata.image_url (исходник для зума) и "
                             "iiif_base у записанных картин")
    parser.add_argument("--all", action="store_true",
                        help="с --fill-sizes или --fill-images: перечитать у всех, а не только "
                             "у пустых")
    parser.add_argument("--dry-run", action="store_true", help="не писать в БД")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )
    if args.rps:  # --rps масштабирует все лимиты разом, сохраняя их соотношение
        for limiter in (*LIMITERS.values(), LIMITER):
            limiter.rps = limiter.target = limiter.target * args.rps
            limiter.allowance = limiter.capacity = max(1.0, limiter.rps)
    dsn = os.environ.get("DATABASE_URL")
    if not dsn and not args.dry_run:
        log.error("DATABASE_URL is not set")
        return 2

    session = requests.Session()
    session.headers["User-Agent"] = USER_AGENT
    conn = psycopg2.connect(dsn) if dsn else None
    if args.fill_sizes or args.fill_images:
        if not conn:
            log.error("--fill-sizes и --fill-images требуют DATABASE_URL")
            return 2
        fill = fill_images if args.fill_images else fill_sizes
        try:
            log.info("done: %d картин", fill(conn, session, args.delay,
                                             args.dry_run, args.all, args.limit))
        except RuntimeError as error:
            log.error("%s", error)
            return 1
        finally:
            conn.close()
        return 0
    known = known_qids(conn) if conn else set()
    done = read_checkpoint(args.checkpoint) if args.checkpoint else set()
    stats = {"written": 0, "known": 0, "duplicate": 0, "skipped": 0}
    written_slugs: list[str] = []
    try:
        limit = args.limit if args.limit is not None else (0 if args.sweep else 100)
        if args.sweep:
            ids = sweep_paintings(session, args.min_sitelinks, args.ru, args.delay,
                                  args.ids_cache)
        else:
            # С запасом: часть картин уже записана, часть отсеется без названия или изображения.
            ids = top_paintings(session, args.pool or (limit or 100) * 3, args.min_sitelinks)
        log.info("Wikidata: %d картин по известности", len(ids))
        fresh = [qid for qid in ids if qid not in known and qid not in done]
        stats["known"] = len(ids) - len(fresh)
        if not args.sweep:
            fresh = fresh[:max(limit * 2, limit + 100)]
        log.info("в работе: %d (уже в базе или пройдено: %d)", len(fresh), stats["known"])
        for start in range(0, len(fresh), CHUNK):
            if limit and stats["written"] >= limit:
                break
            chunk = fresh[start:start + CHUNK]
            process(conn, session, chunk, args, limit, stats, written_slugs)
            if args.checkpoint and not args.dry_run:
                append_checkpoint(args.checkpoint, chunk)
            log.info("обработано %d/%d · %s", min(start + CHUNK, len(fresh)), len(fresh), stats)
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
        # Точечная индексация: полная перестройка ради сотни картин не нужна.
        log.info("index them: scripts/index-entities.sh %s", " ".join(written_slugs))
    return 0


if __name__ == "__main__":
    sys.exit(main())
