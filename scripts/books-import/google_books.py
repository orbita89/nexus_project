#!/usr/bin/env python3
"""Метаданные русских книг из Google Books API или Open Library в каталог Nexus (без своих таблиц).

Книга — строка `entities` (kind = 'book'), как у `nexus books import` (LibGen):

    title           ← volumeInfo.title (до 300 символов)
    description     ← volumeInfo.description (HTML убирается, до 20 000 символов)
    release_date    ← volumeInfo.publishedDate: 2005 → 2005-01-01, 2005-03 → 2005-03-01
    cover_url       ← первая настоящая обложка (см. ниже); нет — NULL
    metadata        ← {"isbn", "pages", "publisher"} (схема `BookMetadata`, админка её принимает)
    авторы          ← people + entity_credits (role = 'author', порядок как в API)

Язык — только русский: `langRestrict=ru` и проверка `volumeInfo.language == 'ru'`. Отдельной
колонки языка в `entities` нет. Дедупликация — по ISBN (ISBN-13, иначе ISBN-10, с проверкой
контрольной цифры): книга с тем же `metadata.isbn` (в том числе из LibGen) пропускается. Книги без
ISBN пропускаются — их не с чем сверять. Автор находится по имени среди уже записанных авторов
(`replace(lower(full_name), 'ё', 'е')`, индекс `people_name_key_idx`), иначе создаётся.
Обложки не скачиваются — только ссылка. Кандидаты по порядку: Google по ISBN
(`books/content?vid=ISBN…&zoom=1`, отвечает за доли секунды), `imageLinks` из ответа Google,
Open Library по id обложки (настоящие, но через archive.org — секунды), Open Library по ISBN
(`?default=false`: нет обложки — 404). Каждый проверяется одним GET: годится JPEG/PNG больше 1 КБ,
кроме заглушек Google (серая «обложка» — `GOOGLE_PLACEHOLDERS`; PNG у Google — «image not available»
или фрагмент страницы). `--fix-covers` — то же для уже записанных книг с ISBN, у которых обложки нет
или она из Open Library.

Open Library (`--source openlibrary`, без ключа): `search.json` с `language=rus`, `offset`/`limit`
по 40; `inauthor:` запроса превращается в `author:`. Название должно быть кириллицей (у работы бывают
издания на других языках), ISBN — сначала российский (978-5…), обложка — `covers.openlibrary.org/b/id`,
дата — `first_publish_year`. Описаний в поиске Open Library нет — `description` пустой.

Запросы — пачками по 40 (`maxResults`), страницы через `startIndex`, пауза между запросами,
повторы с нарастающей паузой на 429/5xx. Без ключа квота Google маленькая — задайте
GOOGLE_BOOKS_API_KEY.

Запуск:
    pip install -r scripts/books-import/requirements.txt
    DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
    GOOGLE_BOOKS_API_KEY=... python3 scripts/books-import/google_books.py [--max-per-query 200]
        [--delay 1.0] [--query 'inauthor:Пелевин' ...] [--source google|openlibrary] [--limit N]
        [--dry-run]
    python3 scripts/books-import/google_books.py --fix-covers   # обложки уже записанных книг
Затем scripts/reindex-search.sh — новые книги и авторы попадут в поиск сайта.
Перезапуск безопасен: уже записанные ISBN пропускаются.
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
from dataclasses import dataclass, field
from datetime import date

import psycopg2
import requests

API_URL = "https://www.googleapis.com/books/v1/volumes"
OPEN_LIBRARY_COVER = "https://covers.openlibrary.org/b/isbn/{isbn}-L.jpg?default=false"
GOOGLE_COVER = (
    "https://books.google.com/books/content?vid=ISBN{isbn}&printsec=frontcover&img=1&zoom=1"
)
# md5 заглушки Google «обложки нет» (серый прямоугольник с полосками), zoom=1.
GOOGLE_PLACEHOLDERS = {"d7c21c65fc861fc5128753e9e091b23c"}
MIN_COVER_BYTES = 1000
# Обложки отдаёт свой nginx с кэшем (infra/nginx.conf, `/covers/...`): в `cover_url` —
# `{PUBLIC_URL}/covers/ol/...` и `/covers/google/...` вместо внешних адресов.
COVERS_BASE = (
    os.environ.get("COVERS_BASE_URL") or os.environ.get("PUBLIC_URL") or "http://localhost"
).rstrip("/")
PROXIED = {
    "https://covers.openlibrary.org/": "/covers/ol/",
    "https://books.google.com/": "/covers/google/",
}
OPEN_LIBRARY_COVER_ID = "https://covers.openlibrary.org/b/id/{id}-L.jpg"
OPEN_LIBRARY_URL = "https://openlibrary.org/search.json"
OPEN_LIBRARY_FIELDS = "key,title,author_name,isbn,first_publish_year,publisher,number_of_pages_median,cover_i"
PAGE_SIZE = 40  # максимум Google Books за запрос
MAX_TITLE = 300
MAX_TEXT = 20_000
MAX_PUBLISHER = 200
MAX_NAME = 200
MAX_AUTHORS = 5
MAX_SLUG_BASE = 80
RETRIES = 5

# Пример для массовой скачки: жанры (subject:) и авторы (inauthor:). Google отдаёт по запросу не
# больше ~1000 результатов, поэтому запросов много и они узкие.
DEFAULT_QUERIES = [
    # Жанры и темы
    "subject:Фантастика",
    "subject:Фэнтези",
    "subject:Детектив",
    "subject:Роман",
    "subject:Поэзия",
    "subject:История",
    "subject:Философия",
    "subject:Психология",
    "subject:Биография",
    "subject:Детская литература",
    "subject:Fiction",
    "subject:History",
    "subject:Science",
    # Классики
    "inauthor:Толстой",
    "inauthor:Достоевский",
    "inauthor:Чехов",
    "inauthor:Пушкин",
    "inauthor:Гоголь",
    "inauthor:Тургенев",
    "inauthor:Булгаков",
    "inauthor:Набоков",
    "inauthor:Бунин",
    "inauthor:Пастернак",
    "inauthor:Солженицын",
    # Современные и фантастика
    "inauthor:Стругацкий",
    "inauthor:Лукьяненко",
    "inauthor:Пелевин",
    "inauthor:Акунин",
    "inauthor:Улицкая",
    "inauthor:Водолазкин",
    "inauthor:Глуховский",
    "inauthor:Донцова",
    "inauthor:Маринина",
]

log = logging.getLogger("google_books")

TRANSLIT = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e", "ж": "zh", "з": "z",
    "и": "i", "й": "y", "к": "k", "л": "l", "м": "m", "н": "n", "о": "o", "п": "p", "р": "r",
    "с": "s", "т": "t", "у": "u", "ф": "f", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh",
    "щ": "shch", "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
}


def slugify(text: str) -> str:
    """«Мастер и Маргарита» → master-i-margarita (как `slugify` каталога); может быть пустой."""
    text = "".join(TRANSLIT.get(ch, ch) for ch in text.lower())
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    text = re.sub(r"['’.]", "", text)
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    return text[:MAX_SLUG_BASE].rstrip("-")


def clean(text: str | None, limit: int) -> str | None:
    """Схлопывает пробелы и обрезает; пустая строка → None."""
    if not text:
        return None
    text = " ".join(text.split())[:limit].rstrip()
    return text or None


def strip_html(text: str | None) -> str | None:
    if not text:
        return None
    text = re.sub(r"<br\s*/?>|</p>", "\n", text, flags=re.I)
    text = html.unescape(re.sub(r"<[^>]+>", "", text))
    lines = [" ".join(line.split()) for line in text.splitlines()]
    text = "\n".join(line for line in lines if line)[:MAX_TEXT].rstrip()
    return text or None


def name_key(name: str) -> str:
    """Как `books::name_key` и индекс `people_name_key_idx`."""
    return " ".join(name.split()).lower().replace("ё", "е")


def isbn_normalize(value: str) -> str | None:
    """Без дефисов и пробелов, с проверкой контрольной цифры (как `metadata::isbn_normalize`)."""
    isbn = re.sub(r"[-\s]", "", value).upper()
    if len(isbn) == 10 and re.fullmatch(r"\d{9}[\dX]", isbn):
        total = sum((10 if c == "X" else int(c)) * (10 - i) for i, c in enumerate(isbn))
        return isbn if total % 11 == 0 else None
    if len(isbn) == 13 and isbn.isdigit():
        total = sum(int(c) * (1 if i % 2 == 0 else 3) for i, c in enumerate(isbn))
        return isbn if total % 10 == 0 else None
    return None


def pick_isbn(info: dict) -> str | None:
    ids = {i.get("type"): i.get("identifier", "") for i in info.get("industryIdentifiers", [])}
    for kind in ("ISBN_13", "ISBN_10"):
        isbn = isbn_normalize(ids.get(kind, ""))
        if isbn:
            return isbn
    return None


def parse_date(value: str | None) -> date | None:
    """2005, 2005-03, 2005-03-12 (и 2005-03-12T00:00:00) → date; иначе None."""
    m = re.match(r"^(\d{4})(?:-(\d{2}))?(?:-(\d{2}))?", value or "")
    if not m:
        return None
    try:
        d = date(int(m[1]), int(m[2] or 1), int(m[3] or 1))
    except ValueError:
        return None
    return d if 1000 <= d.year <= 2100 else None


def google_covers(info: dict, isbn: str) -> list[str]:
    """Кандидаты обложки книги из Google Books: по ISBN, `imageLinks`, Open Library по ISBN."""
    urls = [GOOGLE_COVER.format(isbn=isbn)]
    links = info.get("imageLinks") or {}
    for size in ("extraLarge", "large", "medium", "small", "thumbnail", "smallThumbnail"):
        url = links.get(size)
        if url:
            url = re.sub(r"^http://", "https://", url)
            urls.append(re.sub(r"&edge=curl", "", url))
            break
    urls.append(OPEN_LIBRARY_COVER.format(isbn=isbn))
    return urls


def proxied(url: str) -> str:
    """Внешняя ссылка на обложку → через кэш nginx; уже проксированная и чужая — как есть.
    Параметры у Open Library отбрасываются (`?default=false`: без обложки — заглушка < 1 КБ,
    её отсеет `cover_ok`)."""
    for origin, path in PROXIED.items():
        if url.startswith(origin):
            rest = url[len(origin):]
            if path == "/covers/ol/":
                rest = rest.split("?", 1)[0]
            return f"{COVERS_BASE}{path}{rest}"
    return url


def is_google(url: str) -> bool:
    return "books.google." in url or "/covers/google/" in url


def cover_ok(session: requests.Session, url: str) -> bool:
    """Обложка открывается через кэш nginx (и заодно попадает в него): картинка больше 1 КБ,
    не заглушка Google."""
    try:
        resp = session.get(proxied(url), timeout=40)
    except requests.RequestException:
        return False
    kind = resp.headers.get("content-type", "")
    if resp.status_code != 200 or not kind.startswith("image/") or len(resp.content) < MIN_COVER_BYTES:
        return False
    if is_google(url):
        return kind == "image/jpeg" and hashlib.md5(resp.content).hexdigest() not in GOOGLE_PLACEHOLDERS
    return True


def pick_cover(session: requests.Session, urls: list[str]) -> str | None:
    """Первая настоящая обложка из кандидатов — ссылкой через кэш."""
    return next((proxied(url) for url in urls if cover_ok(session, url)), None)


@dataclass
class Book:
    google_id: str
    isbn: str
    title: str
    authors: list[str]
    description: str | None
    release_date: date | None
    covers: list[str]  # кандидаты обложки по порядку, см. pick_cover
    metadata: dict


@dataclass
class Stats:
    found: int = 0  # элементов в ответах API
    russian: int = 0
    no_isbn: int = 0
    duplicates: int = 0
    written: int = 0
    people_created: int = 0
    no_cover: int = 0
    errors: int = 0
    seen: set = field(default_factory=set)  # ISBN этого запуска

    def line(self) -> str:
        return (
            f"found={self.found} russian={self.russian} no_isbn={self.no_isbn} "
            f"duplicates={self.duplicates} written={self.written} "
            f"people_created={self.people_created} no_cover={self.no_cover} errors={self.errors}"
        )


def to_book(item: dict) -> Book | None | str:
    """Элемент ответа → Book; не русская — None; без ISBN — 'no_isbn'."""
    info = item.get("volumeInfo") or {}
    if info.get("language") != "ru":
        return None
    title = clean(info.get("title"), MAX_TITLE)
    if not title:
        return None
    isbn = pick_isbn(info)
    if not isbn:
        return "no_isbn"
    metadata: dict = {"isbn": isbn}
    pages = info.get("pageCount")
    if isinstance(pages, int) and 1 <= pages <= 100_000:
        metadata["pages"] = pages
    publisher = clean(info.get("publisher"), MAX_PUBLISHER)
    if publisher:
        metadata["publisher"] = publisher
    authors: list[str] = []
    for name in info.get("authors") or []:
        name = clean(name, MAX_NAME)
        if name and name_key(name) not in {name_key(a) for a in authors}:
            authors.append(name)
    return Book(
        google_id=item.get("id", ""),
        isbn=isbn,
        title=title,
        authors=authors[:MAX_AUTHORS],
        description=strip_html(info.get("description")),
        release_date=parse_date(info.get("publishedDate")),
        covers=google_covers(info, isbn),
        metadata=metadata,
    )


CYRILLIC = re.compile(r"[а-яё]", re.I)


def to_book_openlibrary(doc: dict) -> Book | None | str:
    """Документ `search.json` Open Library → Book; не кириллица — None; без ISBN — 'no_isbn'."""
    title = clean(doc.get("title"), MAX_TITLE)
    if not title or not CYRILLIC.search(title):
        return None
    isbns = [i for i in (isbn_normalize(v) for v in doc.get("isbn") or []) if i]
    if not isbns:
        return "no_isbn"
    isbn = next((i for i in isbns if i.startswith("9785")), isbns[0])
    metadata: dict = {"isbn": isbn}
    pages = doc.get("number_of_pages_median")
    if isinstance(pages, int) and 1 <= pages <= 100_000:
        metadata["pages"] = pages
    publishers = doc.get("publisher") or []
    publisher = next((p for p in publishers if CYRILLIC.search(p)), publishers[0] if publishers else None)
    publisher = clean(publisher, MAX_PUBLISHER)
    if publisher:
        metadata["publisher"] = publisher
    authors: list[str] = []
    for name in doc.get("author_name") or []:
        name = clean(name, MAX_NAME)
        if name and name_key(name) not in {name_key(a) for a in authors}:
            authors.append(name)
    # В `author_name` работы — и переводчики изданий на других языках: есть русские имена —
    # остаются только они.
    if any(CYRILLIC.search(a) for a in authors):
        authors = [a for a in authors if CYRILLIC.search(a)]
    year = doc.get("first_publish_year")
    cover = doc.get("cover_i")
    return Book(
        google_id=doc.get("key", ""),
        isbn=isbn,
        title=title,
        authors=authors[:MAX_AUTHORS],
        description=None,
        release_date=date(year, 1, 1) if isinstance(year, int) and 1000 <= year <= 2100 else None,
        covers=[GOOGLE_COVER.format(isbn=isbn)]
        + ([OPEN_LIBRARY_COVER_ID.format(id=cover)] if cover else [])
        + [OPEN_LIBRARY_COVER.format(isbn=isbn)],
        metadata=metadata,
    )


# ---------------------------------------------------------------- API


def get_json(session: requests.Session, url: str, params: dict, retries: int = RETRIES) -> dict:
    for attempt in range(1, retries + 1):
        try:
            resp = session.get(url, params=params, timeout=30 if retries > 1 else 10)
        except requests.RequestException as e:
            wait = 2**attempt
            log.warning("request failed (%s), retry %d/%d in %ds", e, attempt, retries, wait)
            if attempt == retries:
                break
            time.sleep(wait)
            continue
        if resp.status_code == 200:
            return resp.json()
        if resp.status_code in (429, 500, 502, 503, 504):
            wait = int(resp.headers.get("Retry-After") or 2**attempt * 5)
            log.warning("HTTP %d, retry %d/%d in %ds", resp.status_code, attempt, retries, wait)
            if attempt == retries:
                break
            time.sleep(wait)
            continue
        raise RuntimeError(f"HTTP {resp.status_code}: {resp.text[:300]}")
    raise RuntimeError(f"giving up after {retries} attempts")


def fetch_page(session, source: str, query: str, start: int, key: str | None) -> tuple[list, int]:
    """Страница результатов: (элементы, всего найдено)."""
    if source == "openlibrary":
        params = {
            "q": query.replace("inauthor:", "author:"),
            "language": "rus",
            "fields": OPEN_LIBRARY_FIELDS,
            "limit": PAGE_SIZE,
            "offset": start,
        }
        data = get_json(session, OPEN_LIBRARY_URL, params)
        return data.get("docs") or [], data.get("numFound", 0)
    params = {
        "q": query,
        "langRestrict": "ru",
        "printType": "books",
        "maxResults": PAGE_SIZE,
        "startIndex": start,
    }
    if key:
        params["key"] = key
    data = get_json(session, API_URL, params)
    return data.get("items") or [], data.get("totalItems", 0)


# ---------------------------------------------------------------- DB


def isbn_exists(cur, isbn: str) -> bool:
    # @> по GIN entities_metadata_gin (jsonb_path_ops).
    cur.execute(
        "SELECT 1 FROM entities WHERE kind = 'book' AND metadata @> %s::jsonb LIMIT 1",
        (json.dumps({"isbn": isbn}),),
    )
    return cur.fetchone() is not None


def free_slug(cur, table: str, candidates: list[str]) -> str | None:
    cur.execute(f"SELECT slug FROM {table} WHERE slug = ANY(%s)", (candidates,))
    taken = {row[0] for row in cur.fetchall()}
    return next((c for c in candidates if c not in taken), None)


def author_id(cur, name: str, stats: Stats) -> str | None:
    key = name_key(name)
    cur.execute(
        """SELECT p.id FROM people p
           WHERE replace(lower(p.full_name), 'ё', 'е') = %s
             AND EXISTS (SELECT 1 FROM entity_credits c
                         WHERE c.person_id = p.id AND c.role = 'author')
           ORDER BY p.created_at LIMIT 1""",
        (key,),
    )
    row = cur.fetchone()
    if row:
        return row[0]
    base = slugify(name)
    suffix = hashlib.md5(key.encode()).hexdigest()[:8]
    candidates = [base, f"{base}-{suffix}"] if base else [f"author-{suffix}"]
    slug = free_slug(cur, "people", candidates)
    if not slug:
        return None
    cur.execute(
        "INSERT INTO people (slug, full_name) VALUES (%s, %s) "
        "ON CONFLICT (slug) DO NOTHING RETURNING id",
        (slug, name),
    )
    row = cur.fetchone()
    if row:
        stats.people_created += 1
        return row[0]
    return None


def save(conn, session, book: Book, stats: Stats, require_cover: bool = False) -> str:
    """Книга, авторы и титры в одной транзакции. Итог: 'written', 'duplicate' (ISBN уже есть
    или не нашлось slug) или 'no_cover' (с `require_cover`)."""
    with conn, conn.cursor() as cur:
        if isbn_exists(cur, book.isbn):
            return "duplicate"
        cover = pick_cover(session, book.covers)
        if not cover:
            stats.no_cover += 1
            if require_cover:
                return "no_cover"
        base = slugify(book.title)
        candidates = [base, f"{base}-{book.isbn}"] if base else [f"book-{book.isbn}"]
        slug = free_slug(cur, "entities", candidates)
        if not slug:
            return "duplicate"
        cur.execute(
            """INSERT INTO entities (kind, slug, title, description, release_date, cover_url,
                                     metadata)
               VALUES ('book', %s, %s, %s, %s, %s, %s::jsonb)
               ON CONFLICT (slug) DO NOTHING RETURNING id""",
            (
                slug,
                book.title,
                book.description,
                book.release_date,
                cover,
                json.dumps(book.metadata, ensure_ascii=False),
            ),
        )
        row = cur.fetchone()
        if not row:
            return "duplicate"
        entity_id = row[0]
        added: set = set()
        for name in book.authors:
            person = author_id(cur, name, stats)
            if person and person not in added:
                cur.execute(
                    "INSERT INTO entity_credits (entity_id, person_id, role, position) "
                    "VALUES (%s, %s, 'author', %s)",
                    (entity_id, person, len(added)),
                )
                added.add(person)
    return "written"


# ---------------------------------------------------------------- main


def run_query(session, conn, query: str, args, stats: Stats) -> None:
    before = Stats(**{k: v for k, v in vars(stats).items() if k != "seen"})
    start = 0
    while start < args.max_per_query:
        items, total = fetch_page(session, args.source, query, start, args.api_key)
        stats.found += len(items)
        convert = to_book_openlibrary if args.source == "openlibrary" else to_book
        for item in items:
            if args.limit and stats.written >= args.limit:
                break
            book = convert(item)
            if book is None:
                continue
            stats.russian += 1
            if book == "no_isbn":
                stats.no_isbn += 1
                continue
            if book.isbn in stats.seen:
                stats.duplicates += 1
                continue
            stats.seen.add(book.isbn)
            if args.dry_run:
                log.debug("dry-run: %s — %s (%s)", book.isbn, book.title, ", ".join(book.authors))
                stats.written += 1
                continue
            try:
                result = save(conn, session, book, stats, args.require_cover)
                if result == "written":
                    stats.written += 1
                elif result == "duplicate":
                    stats.duplicates += 1
            except psycopg2.Error as e:
                stats.errors += 1
                log.error("%s (%s): %s", book.title, book.isbn, e)
        # Последняя страница: меньше 40 элементов или дальше totalItems.
        start += PAGE_SIZE
        if len(items) < PAGE_SIZE or start >= total or (args.limit and stats.written >= args.limit):
            break
        time.sleep(args.delay)
    log.info(
        "%-30s found=%d written=%d duplicates=%d no_isbn=%d",
        query,
        stats.found - before.found,
        stats.written - before.written,
        stats.duplicates - before.duplicates,
        stats.no_isbn - before.no_isbn,
    )
    time.sleep(args.delay)


def api_covers(session, isbn: str, key: str | None) -> list[str]:
    """Обложки изданий с этим ISBN из Google Books API (`q=isbn:…`): `imageLinks` по порядку."""
    if not key:
        return []
    try:
        data = get_json(
            session, API_URL, {"q": f"isbn:{isbn}", "maxResults": 5, "key": key}, retries=2
        )
    except RuntimeError as e:
        log.warning("isbn:%s lookup failed: %s", isbn, e)
        return []
    urls = []
    for item in data.get("items") or []:
        urls.extend(u for u in google_covers(item.get("volumeInfo") or {}, isbn)[1:-1] if u not in urls)
    return urls


def openlibrary_cover(session, isbn: str) -> str | None:
    """Обложка Open Library по id из поиска по ISBN (`search.json?isbn=`)."""
    try:
        data = get_json(
            session, OPEN_LIBRARY_URL, {"isbn": isbn, "fields": "cover_i", "limit": 1}, retries=1
        )
    except RuntimeError:
        return None
    docs = data.get("docs") or []
    cover = docs[0].get("cover_i") if docs else None
    return OPEN_LIBRARY_COVER_ID.format(id=cover) if cover else None


def timed_get(session, url: str) -> float | None:
    """Время полной загрузки картинки, с; не открылась — None."""
    start = time.monotonic()
    try:
        resp = session.get(url, timeout=30)
    except requests.RequestException:
        return None
    return time.monotonic() - start if resp.status_code == 200 else None


def fix_covers(conn, session, args) -> None:
    """Обложки уже записанных книг с ISBN: нет обложки или она из Open Library (медленно,
    бывают заглушки) — заново, первым Google: по ISBN, затем издания из Google Books API
    (`q=isbn:…`, нужен ключ). В конце — время загрузки выбранных обложек по источникам."""
    with conn.cursor() as cur:
        cur.execute(
            """SELECT id, title, metadata->>'isbn', cover_url FROM entities
               WHERE kind = 'book' AND metadata ? 'isbn'
                 AND (cover_url IS NULL OR cover_url LIKE '%%openlibrary.org%%'
                      OR cover_url LIKE '%%/covers/ol/%%')
               ORDER BY created_at"""
        )
        rows = cur.fetchall()
    log.info("books to check: %d", len(rows))
    stats = {"google": 0, "openlibrary": 0, "none": 0, "unchanged": 0}
    for n, (entity_id, title, isbn, current) in enumerate(rows, 1):
        candidates = [GOOGLE_COVER.format(isbn=isbn), *api_covers(session, isbn, args.api_key)]
        cover = pick_cover(session, candidates)
        if not cover and current and "/b/id/" in current:
            # Open Library отвечает медленно и бывает 502: рабочую ссылку не проверяем заново и
            # не затираем.
            cover = current
        if not cover:
            ol = openlibrary_cover(session, isbn)
            candidates = ([ol] if ol else []) + [OPEN_LIBRARY_COVER.format(isbn=isbn)]
            cover = pick_cover(session, candidates)
        if not cover and current:
            cover = current  # новой не нашлось — старая остаётся
        kind = "none" if not cover else "google" if is_google(cover) else "openlibrary"
        stats[kind] += 1
        if cover == current:
            stats["unchanged"] += 1
        elif not args.dry_run:
            with conn, conn.cursor() as cur:
                cur.execute("UPDATE entities SET cover_url = %s WHERE id = %s", (cover, entity_id))
        log.info("[%d/%d] %-40s %s", n, len(rows), title[:40], cover or "-")
        time.sleep(args.delay / 4)
    log.info("covers: %s", stats)
    # Скорость: как долго открывается каждая выбранная обложка (первый запрос, без кэша).
    with conn.cursor() as cur:
        cur.execute("SELECT cover_url FROM entities WHERE kind = 'book' AND cover_url IS NOT NULL")
        urls = [row[0] for row in cur.fetchall()]
    timings: dict[str, list[float]] = {}
    failed: dict[str, int] = {}
    for url in urls:
        source = "google" if is_google(url) else "openlibrary"
        t = timed_get(session, url)
        if t is None:
            failed[source] = failed.get(source, 0) + 1
        else:
            timings.setdefault(source, []).append(t)
    for source, ts in sorted(timings.items()):
        ts.sort()
        log.info(
            "load %-11s n=%d avg=%.2fs median=%.2fs max=%.2fs failed=%d",
            source, len(ts), sum(ts) / len(ts), ts[len(ts) // 2], ts[-1], failed.get(source, 0),
        )


def proxy_covers(conn, session, args) -> None:
    """Уже записанные книги: внешние ссылки на обложки → через кэш nginx; каждая открывается
    один раз (прогрев кэша), затем второй — время из кэша. Не открылась — ссылка не меняется."""
    with conn.cursor() as cur:
        cur.execute(
            "SELECT id, cover_url FROM entities WHERE kind = 'book' AND cover_url IS NOT NULL"
        )
        rows = cur.fetchall()
    todo = [(i, u, proxied(u)) for i, u in rows if proxied(u) != u or u.startswith(f"{COVERS_BASE}/covers/")]
    log.info("books with covers: %d, to proxy and warm: %d", len(rows), len(todo))
    first: list[float] = []
    cached: list[float] = []
    failed = 0
    for n, (entity_id, url, new) in enumerate(todo, 1):
        t1 = timed_get(session, new)
        t2 = timed_get(session, new) if t1 is not None else None
        if t1 is None or t2 is None:
            failed += 1
            log.warning("[%d/%d] not loaded: %s", n, len(todo), new)
            continue
        first.append(t1)
        cached.append(t2)
        if new != url and not args.dry_run:
            with conn, conn.cursor() as cur:
                cur.execute("UPDATE entities SET cover_url = %s WHERE id = %s", (new, entity_id))
        if n % 50 == 0:
            log.info("[%d/%d] warmed", n, len(todo))
    for name, ts in (("first (origin)", first), ("cached", cached)):
        if ts:
            ts.sort()
            log.info(
                "load %-15s n=%d avg=%.2fs median=%.2fs max=%.2fs",
                name, len(ts), sum(ts) / len(ts), ts[len(ts) // 2], ts[-1],
            )
    log.info("failed: %d", failed)


def env_file_key() -> str | None:
    """GOOGLE_BOOKS_API_KEY из infra/.env (только эта строка — файл не исполняется)."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "infra", ".env")
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                if line.startswith("GOOGLE_BOOKS_API_KEY="):
                    return line.split("=", 1)[1].strip().strip("'\"") or None
    except OSError:
        pass
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--query", action="append", help="запрос Google Books (можно несколько); "
                        "по умолчанию — DEFAULT_QUERIES")
    parser.add_argument("--max-per-query", type=int, default=400,
                        help="не больше стольких результатов на запрос (по 40 за страницу)")
    parser.add_argument("--delay", type=float, default=1.0, help="пауза между запросами, с")
    parser.add_argument("--api-key", default=os.environ.get("GOOGLE_BOOKS_API_KEY"))
    parser.add_argument("--source", choices=["google", "openlibrary"], default="google")
    parser.add_argument("--limit", type=int, help="остановиться после стольких записанных книг")
    parser.add_argument("--require-cover", action="store_true",
                        help="не записывать книги, для которых не нашлось обложки")
    parser.add_argument("--proxy-covers", action="store_true",
                        help="перевести обложки записанных книг на кэш nginx и прогреть его")
    parser.add_argument("--fix-covers", action="store_true",
                        help="только обновить обложки уже записанных книг")
    parser.add_argument("--dry-run", action="store_true", help="не писать в БД")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    if not args.api_key:
        args.api_key = env_file_key()
    dsn = os.environ.get("DATABASE_URL")
    if not dsn and not args.dry_run:
        log.error("DATABASE_URL is not set")
        return 2
    if args.proxy_covers:
        if not dsn:
            log.error("DATABASE_URL is not set")
            return 2
        conn = psycopg2.connect(dsn)
        try:
            proxy_covers(conn, requests.Session(), args)
        finally:
            conn.close()
        return 0
    if args.fix_covers:
        if not dsn:
            log.error("DATABASE_URL is not set")
            return 2
        conn = psycopg2.connect(dsn)
        session = requests.Session()
        session.headers["User-Agent"] = "nexus-books-import/1.0 (anton.gusev@optimax.dev)"
        log.info("Google Books API key: %s", "set" if args.api_key else "not set")
        try:
            fix_covers(conn, session, args)
        finally:
            conn.close()
        log.info("run scripts/reindex-search.sh to refresh covers in site search")
        return 0
    if not args.api_key and args.source == "google":
        log.warning("GOOGLE_BOOKS_API_KEY is not set: anonymous quota is small, expect HTTP 429")

    conn = psycopg2.connect(dsn) if not args.dry_run else None
    session = requests.Session()
    # Open Library просит указывать контакт в User-Agent.
    session.headers["User-Agent"] = "nexus-books-import/1.0 (anton.gusev@optimax.dev)"
    stats = Stats()
    queries = args.query or DEFAULT_QUERIES
    try:
        for n, query in enumerate(queries, 1):
            if args.limit and stats.written >= args.limit:
                break
            log.info("[%d/%d] %s", n, len(queries), query)
            try:
                run_query(session, conn, query, args, stats)
            except RuntimeError as e:
                stats.errors += 1
                log.error("query %r failed: %s", query, e)
            log.info("total: %s", stats.line())
    except KeyboardInterrupt:
        log.warning("interrupted")
    finally:
        if conn:
            conn.close()
    log.info("done: %s", stats.line())
    if stats.written and not args.dry_run:
        log.info("run scripts/reindex-search.sh to put new books into site search")
    return 0


if __name__ == "__main__":
    sys.exit(main())
