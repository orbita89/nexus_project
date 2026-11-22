#!/usr/bin/env python3
"""Порядок в книгах: архив, авторы, названия, издания, сборники, жанры. Разметка — LLM.

ГДЕ ЧТО ЛЕЖИТ
  Файлы шагов — `scripts/catalog-import/cache/books_llm/` (в .gitignore):
    system_prompt.txt   инструкция модели (пишет `export`; список жанров — slug'и из БД)
    agent_task.md       задание агенту Claude Code на одну пачку
    in_NNNN.txt         вход модели: N|название|авторы через ;|жанры через ,|год|слов|аннотация
    in_NNNN.json        номер N → id книги (модель его не читает)
    out_NNNN.txt        выход модели: N|тип|название|авторы|+жанры|-жанры|книг  («=» — без изменений)
    out_NNNN.jsonl      выход старого формата (пачки 0000–0529), читается как раньше
    archive_plan.jsonl  кандидаты в архив;  apply_report.txt  отчёт сухого прогона
  Источник и цель записи — PostgreSQL (`DATABASE_URL`): `entities`, `entity_tags`, `people`,
  `entity_credits`, `catalog_manual_ranks`. Поиск (Tantivy + слой Б в RAM) скрипт НЕ трогает:
  после записи — `nexus books index` (полная пересборка, ~11 мин) и `nexus books ranks FILE`.

ШАГИ
  1. archive-plan  без LLM: книги без обложки или с файлом < 50 КБ, кроме известных авторов
                   (≥ NOTABLE_BOOKS книг с обложкой) и ручного списка → `archive_plan.jsonl`.
                   В БД не пишет (перенос в архив — `catalog::books::archive`, отдельно).
  2. export        остальные книги (не архив, не издание, не скрытые, ещё не размеченные) пачками
                   по `--chunk`: сначала ручной список, затем по размеру файла. Номера пачек
                   продолжаются: следующий запуск берёт книги, на которых закончили.
  3. разметка      `out_NNNN.txt` любым способом:
                   - агенты Claude Code (дёшево по ключам, дорого по токенам): по агенту на
                     пачку, задание — `agent_task.md`;
                   - `classify` — напрямую через API своими ключами (экономнее: промпт кэшируется,
                     100 книг — один запрос): `--provider anthropic` (claude-haiku-5-5) или
                     `--provider openai` + `--base-url` + `--model` — любой OpenAI-совместимый
                     (OpenAI, DeepSeek, OpenRouter, Qwen, Gemini, Ollama). Ключи — `LLM_API_KEYS`
                     через запятую (по кругу), иначе `ANTHROPIC_API_KEY(S)` / `OPENAI_API_KEY`.
  4. apply         сухой прогон: план + `apply_report.txt`. `--only 530-559` — только эти пачки.
  5. apply --apply запись плана одной транзакцией (ошибка — не записано ничего).
  6. people-cleanup --apply  удалить опустевшие дубли авторов (связи TMDB/Wikidata переносятся).

ЧТО ИСПРАВЛЯЕТ apply (и как не ломает)
  - название → очищенное (старое — `metadata.title_raw`); пропал номер тома — не меняется;
  - жанры: + недостающие, − неверные (только slug'и из `tags.kind = genre`);
  - сборник (тип s, книг > 1 или по названию: «Собрание сочинений», «Книги 1-7», «Трилогия»…) →
    `metadata.collection = {books, by}`; в издания не склеивается; `--hide-collections` — скрыть;
  - статья / документ (a, d) → только `metadata.kind` (модель путает словари и классику);
  - мусор (x) → `active = false`, `metadata.archive_reason` (обратимо);
  - авторы: имя только удлиняется («Виктор Пелевин» → «Виктор Олегович Пелевин»); дубли
    сливаются только при полном ФИО с отчеством; «Неизвестный автор» не трогается;
  - издания: то же исходное название (без мусора в скобках), тот же том, та же фамилия →
    основная с обложкой и большим файлом, остальные `parent_id` = основная (как сезоны), из
    поиска выпадают; ручное место переходит основной.

    cd scripts/catalog-import && export DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db
    .venv/bin/python books_llm.py export --limit 50000
    LLM_API_KEYS=k1,k2 .venv/bin/python books_llm.py classify
    .venv/bin/python books_llm.py apply              # посмотреть отчёт
    .venv/bin/python books_llm.py apply --apply      # записать
"""

import argparse
import json
import logging
import os
import re
import sys
from collections import defaultdict

import psycopg2
import psycopg2.extras

log = logging.getLogger("books_llm")

HERE = os.path.dirname(os.path.abspath(__file__))
DIR = os.path.join(HERE, "cache", "books_llm")
MIN_SIZE = 50_000
NOTABLE_BOOKS = 10
TYPES = {"fiction", "poetry", "children", "nonfiction", "article", "document", "other", "collection"}
# Сборник по названию (для уже размеченных без `collection` и как подстраховка модели).
COLLECTION = re.compile(
    r"собрание сочинений|полное собрание|избранн(ое|ые произведения)|\(сборник\)|\[сборник\]|"
    r"антолог|омнибус|компиляц|дилоги|трилоги|тетралоги|пенталоги|гексалоги|"
    r"\bцикл\b|книги\s*\d+\s*[-–—]\s*\d+|тома?\s*\d+\s*[-–—]\s*\d+|"
    r"романы\b|повести и рассказы|все романы|все повести|в одном томе",
    re.IGNORECASE)
# Скрывается только мусор. Статья и документ — пометка `metadata.kind` без скрытия: модель
# путает словари, «Манифест» и статьи Добролюбова с «не книгами» — решать человеку.
NOT_BOOK = {"other"}
MARK_KIND = {"article", "document"}
# Мусорное название: хеш вместо названия, «Unknown», «Без названия», служебные заглушки, пусто.
JUNK_TITLE = re.compile(
    r"^\s*$|^[a-z] [0-9a-f]{12,}$|^[0-9a-f]{16,}$|^<[0-9a-f]{16,}>$|^\[no data\]$|"
    r"^[_\d\s-]{8,}$|^(unknown|untitled|без названия|оглавление|содержание|document\d*|"
    r"new document|книга\d*|noname|title|тест|test)\s*$",
    re.IGNORECASE)
# Статья — короткий текст: пометку «статья» от модели принимаем, только если слов меньше
# этого (известно число слов) — иначе это книга (эссе, трактат), тип — nonfiction.
ARTICLE_MAX_WORDS = 10_000

VISIBLE = "e.kind = 'book' AND e.active AND e.parent_id IS NULL"

# Известный автор: у человека не меньше NOTABLE_BOOKS видимых книг с обложкой.
NOTABLE = f"""
WITH notable AS (
  SELECT ec.person_id FROM entity_credits ec JOIN entities e ON e.id = ec.entity_id
  WHERE ec.role = 'author' AND {VISIBLE} AND e.cover_url IS NOT NULL
  GROUP BY ec.person_id HAVING count(*) >= {NOTABLE_BOOKS}
)"""

ARCHIVE = NOTABLE + f"""
SELECT e.id::text, e.slug, e.title,
       CASE WHEN e.cover_url IS NULL THEN 'no_cover' ELSE 'small_file' END AS reason,
       (e.metadata -> 'file' ->> 'size')::bigint AS size
FROM entities e
WHERE {VISIBLE}
  AND (e.cover_url IS NULL OR coalesce((e.metadata -> 'file' ->> 'size')::bigint, 0) < {MIN_SIZE})
  AND NOT EXISTS (SELECT 1 FROM catalog_manual_ranks r WHERE r.entity_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM entity_credits ec JOIN notable n ON n.person_id = ec.person_id
                  WHERE ec.entity_id = e.id AND ec.role = 'author')
"""

CANDIDATES = NOTABLE + f"""
SELECT e.id::text, e.title, e.original_title, e.release_date, left(e.description, 300) AS annotation,
       e.metadata ->> 'publisher' AS publisher, (e.metadata -> 'file' ->> 'size')::bigint AS size,
       (e.metadata ->> 'words')::bigint AS words, r.rank,
       (SELECT json_agg(p.full_name ORDER BY ec.position) FROM entity_credits ec
          JOIN people p ON p.id = ec.person_id
          WHERE ec.entity_id = e.id AND ec.role = 'author') AS authors,
       (SELECT json_agg(t.slug ORDER BY t.slug) FROM entity_tags et JOIN tags t ON t.id = et.tag_id
          WHERE et.entity_id = e.id AND t.kind = 'genre') AS genres
FROM entities e
LEFT JOIN catalog_manual_ranks r ON r.entity_id = e.id
WHERE {VISIBLE}
  AND (r.rank IS NOT NULL OR (
       e.cover_url IS NOT NULL AND coalesce((e.metadata -> 'file' ->> 'size')::bigint, 0) >= {MIN_SIZE})
       OR EXISTS (SELECT 1 FROM entity_credits ec JOIN notable n ON n.person_id = ec.person_id
                  WHERE ec.entity_id = e.id AND ec.role = 'author'))
  {{extra}}
ORDER BY {{order}}
LIMIT %s
"""

# Порядок выгрузки (`--order`): top — сначала ручной список, потом крупные файлы;
# newest — последние добавленные в БД.
ORDERS = {
    "top": "r.rank NULLS LAST, (e.metadata -> 'file' ->> 'size')::bigint DESC NULLS LAST, e.id",
    "newest": "e.created_at DESC, e.id",
    # Перепроверка сборников, помеченных по названию: модель решает заново по новому промпту
    # (тома одного романа — не сборник). Размеченные раньше берутся снова; после записи
    # пометка или снята, или стала `llm` — повторно они не выгружаются.
    "collections": "r.rank NULLS LAST, e.id",
}
EXTRA = {"collections": "AND e.metadata -> 'collection' ->> 'by' = 'title'"}

# Всего книг к чистке и сколько из них уже размечено (`id = ANY(%s)`) — для прогресса.
PROGRESS = NOTABLE + f"""
SELECT count(*), count(*) FILTER (WHERE e.id = ANY(%s::uuid[]))
FROM entities e
LEFT JOIN catalog_manual_ranks r ON r.entity_id = e.id
WHERE {VISIBLE}
  AND (r.rank IS NOT NULL OR (
       e.cover_url IS NOT NULL AND coalesce((e.metadata -> 'file' ->> 'size')::bigint, 0) >= {MIN_SIZE})
       OR EXISTS (SELECT 1 FROM entity_credits ec JOIN notable n ON n.person_id = ec.person_id
                  WHERE ec.entity_id = e.id AND ec.role = 'author'))
"""

# --- Формат обмена с моделью (v2, экономный) ------------------------------------------------
# Модель видит не UUID и не JSON, а короткие строки с номером книги в пачке:
#   вход  `in_NNNN.txt`:  N|название|авторы через ;|жанры через ,|год|слов|начало аннотации
#   выход `out_NNNN.txt`: N|тип|название|авторы|+жанры|-жанры|книг   («=» — без изменений)
# Номер N → id книги — в `in_NNNN.json` (его модель не читает). Старые пачки (`out_*.jsonl`,
# JSON с id) читаются как раньше.

TYPE_CODES = {"f": "fiction", "p": "poetry", "c": "children", "n": "nonfiction",
              "s": "collection", "a": "article", "d": "document", "x": "other"}
ANNOTATION_CHARS = 90  # аннотация — ~60% входа; для жанра хватает начала

SYSTEM_PROMPT = """Ты — каталогизатор книг русской электронной библиотеки.

ВХОД — строки: N|название|авторы через ;|жанры через ,|год|слов|начало аннотации
(аннотация есть не у всех: пустая — книга ясна по названию, автору и жанру; пустая
аннотация НЕ повод сомневаться, ставить x или менять жанры)
ВЫХОД — РОВНО одна строка на каждую строку входа, с тем же N, в том же порядке:
N|тип|название|авторы|+жанры|-жанры|книг

Поля выхода:
1. тип — одна буква:
   f — книга: художественная проза (роман, повесть, рассказы) и ЛЮБАЯ обычная книга,
       если не подходит ни одна другая буква. Не уверен — ставь f.
   p — стихи, поэмы
   c — детская книга, сказки
   n — нон-фикшн: история, наука, учебник, мемуары, психология, религия, справочник, словарь
   s — сборник: в файле НЕСКОЛЬКО РАЗНЫХ ПРОИЗВЕДЕНИЙ, каждое — отдельная книга:
       «Бесы. Преступление и наказание. Идиот», «Гарри Поттер. Книги 1-7», трилогия или цикл
       одним файлом, «Собрание сочинений. Том 3», где внутри несколько романов, антология
       разных авторов, омнибус.
       НЕ сборник (ставь f, n, c, p): одна книга, даже если она в томах или частях —
       «Война и мир. Том 2», «Война и мир. Тома 1-4», «История государства Российского.
       Тома 1-12», «Полное собрание сочинений. Том 12. Дело незадачливого жениха» (одно
       произведение); сборник рассказов или стихов, изданный как одна книга.
   a — короткая статья, рецензия, интервью (не книга); эссе книжного объёма — это n
   d — журнал, газета, документ, инструкция, реферат (не книга)
   x — мусор, непонятно что
2. название — очищенное: убери [litres], (сборник), (илл. ...), имя автора, расширение файла,
   КАПС («СМЫСЛ ИСТОРИИ» → «Смысл истории»). НОМЕР ТОМА, КНИГИ, ЧАСТИ ОСТАВЬ («Тарзан. Том 5»).
   Не переводи и не переименовывай. Чистить нечего — пиши =
3. авторы — полные имена через ; в ТОМ ЖЕ порядке и ТОМ ЖЕ количестве, что во входе.
   «Имя Отчество Фамилия», если отчество общеизвестно (Лев Николаевич Толстой), иначе
   «Имя Фамилия» (Стивен Кинг). Инициалы и латиницу раскрывай, только если уверен.
   Отчества не выдумывай. НИКОГДА не сокращай: имя во входе уже полное («Георгий Павлович
   Герасимов») — пиши =. Менять нечего или не уверен — пиши =
4. +жанры — до 2 недостающих жанров из СПИСКА ЖАНРОВ через запятую; нет — пусто.
5. -жанры — жанры из входа, которые явно неверны, через запятую; нет — пусто.
6. книг — сколько РАЗНЫХ произведений-книг в файле (s — 2 и больше); одна книга в томах — 1.

Ничего, кроме строк выхода: без пояснений, без нумерации списком, без ``` .

ПРИМЕР ВХОДА:
1|СМЫСЛ ИСТОРИИ|Николай Бердяев|publitsistika|1923|60000|Опыт философии человеческой судьбы
2|Петровка, 38|Юлиан Семенов|detektivy|1963|70000|Московский уголовный розыск ищет
3|Возвышение. Сборник. Книги 1-7|Андрей Земляной|fantasy|2020|900000|Все книги цикла
4|Сделай сам №3 2008|Сборник|dom-i-semya|2008|20000|Журнал для домашних мастеров
ПРИМЕР ВЫХОДА:
1|n|Смысл истории|Николай Александрович Бердяев|filosofiya||1
2|f|=|Юлиан Семёнов|||1
3|s|=|=|||7
4|d|=|=|||1

СПИСОК ЖАНРОВ: """

AGENT_TASK = """Задание классификатору книг. Номер пачки NNNN (4 цифры).

1. Прочитай инструкцию {dir}/system_prompt.txt.
2. Прочитай вход {dir}/in_NNNN.txt (до 100 строк).
3. Запиши выход в {dir}/out_NNNN.txt инструментом Write: ровно по одной строке на каждую строку
   входа, в формате из инструкции.
4. Других файлов не читай и не трогай, в интернете не ищи.
5. В ответ верни одно число: сколько строк записал.
"""


def clean_field(text) -> str:
    return " ".join(str(text or "").replace("|", "/").split())


# Аннотация — больше половины входа. Шлём её, только если без неё модели трудно: нет жанра
# или он общий, название «грязное» или похоже на сборник/том, автора нет / инициалы / несколько,
# текст короткий (статья? документ?). Остальным хватает названия, автора и жанра.
GENERIC_GENRES = {"setevaya-literatura", "proza", "prochee", "raznoe", "other", "unrecognised",
                  "sovremennaya-proza", "publitsistika", "nonfiction"}
DOUBT_TITLE = re.compile(
    r"[\[\](){}<>«»\"/\\_#|]|\d|\b(том|т\.|книга|часть|выпуск|сборник|избранн|собрани|сочинени"
    r"|антолог|повести|рассказы|стихи|vol|book|part)\b|^[^а-яёa-z]*$", re.I)
INITIALS = re.compile(r"(^|\s)[A-ZА-ЯЁ]\.")
SHORT_WORDS = 20_000


def needs_annotation(r: dict) -> bool:
    """Нужна ли модели аннотация книги `r` (см. GENERIC_GENRES)."""
    genres = r.get("genres") or []
    if not genres or all(g in GENERIC_GENRES for g in genres):
        return True
    title = str(r.get("title") or "")
    if len(title) < 4 or DOUBT_TITLE.search(title):
        return True
    authors = r.get("authors") or []
    if len(authors) != 1 or INITIALS.search(authors[0]) or len(authors[0].split()) < 2:
        return True
    words = r.get("words")
    return bool(words) and int(words) < SHORT_WORDS


def book_line(n: int, r: dict) -> str:
    """Строка входа модели для книги `r` с номером `n`; аннотация — только если нужна."""
    year = r["release_date"].year if r.get("release_date") else ""
    annotation = clean_field(r.get("annotation"))[:ANNOTATION_CHARS] if needs_annotation(r) else ""
    return "|".join([
        str(n), clean_field(r["title"]),
        "; ".join(clean_field(a) for a in (r.get("authors") or [])),
        ",".join(r.get("genres") or []), str(year), str(r.get("words") or ""),
        annotation,
    ])


def parse_line(line: str) -> dict | None:
    """Строка выхода модели → поля; не по формату — None."""
    parts = line.strip().strip("`").split("|")
    if len(parts) < 7 or not parts[0].strip().isdigit():
        return None
    head, mid, tail = parts[:2], parts[2:-3], parts[-3:]
    if len(mid) < 2:
        return None
    code = head[1].strip().lower()
    kind = TYPE_CODES.get(code[:1]) if code not in TYPE_CODES.values() else code
    title, authors = "|".join(mid[:-1]).strip(), mid[-1].strip()
    split = lambda v: [x.strip() for x in v.replace(";", ",").split(",") if x.strip()]
    inside = tail[2].strip()
    return {
        "n": int(parts[0]),
        "type": kind or "other",
        "clean_title": None if title in ("", "=") else title,
        "authors": None if authors in ("", "=") else [a.strip() for a in authors.split(";") if a.strip()],
        "genres_add": split(tail[0].lstrip("+")),
        "genres_wrong": split(tail[1].lstrip("-")),
        "books_inside": int(inside) if inside.isdigit() else None,
    }


def connect():
    return psycopg2.connect(os.environ.get(
        "DATABASE_URL", "postgres://nexus_user:nexus_password@localhost:5432/nexus_db"))


def norm(text: str) -> str:
    text = (text or "").lower().replace("ё", "е")
    return " ".join(re.findall(r"\w+", text))


VOLUME = re.compile(
    r"(?:том|книга|кн|часть|ч|выпуск|вып|сезон|vol|volume|book|part|№)\.?\s*(\d+|[ivxlc]+)\b",
    re.IGNORECASE)


# Номер в конце названия — тоже том: «Еврейский синдром - 3», «Попаданка 2».
TRAILING = re.compile(r"[\s\-–—.,:]+(\d{1,3})\s*$")
# Явный мусор в скобках: при группировке изданий он не в счёт.
JUNK = re.compile(r"[\[(](?:litres|сборник|антология|илл[^)\]]*|иллюстрирован[^)\]]*|"
                  r"журнальный вариант|с иллюстрациями|fb2|epub)[\])]", re.IGNORECASE)


def volumes(title: str) -> frozenset:
    """Номера томов, книг, частей в названии: «Тарзан. Том 5» → {"5"}. Разные номера — разные
    книги, а не издания одной."""
    text = (title or "").replace("ё", "е")
    found = {m.group(1).lower() for m in VOLUME.finditer(text)}
    tail = TRAILING.search(JUNK.sub("", text))
    if tail:
        found.add(tail.group(1))
    return frozenset(found)


def edition_key(title: str) -> str:
    """Ключ издания: исходное название без явного мусора в скобках. Не очищенное Haiku —
    «Мир приключений. Ежегодный сборник…» разных лет не должны стать одной книгой."""
    return norm(JUNK.sub(" ", title or ""))


def fuller(name: str, full: str) -> bool:
    """`full` — то же имя, только полнее: все слова `name` в нём и фамилия та же («Виктор
    Пелевин» → «Виктор Олегович Пелевин»). Сокращение («Ричард Филлипс Фейнман» → «Ричард
    Фейнман») и другое имя — нет."""
    a, b = name_words(name), name_words(full)
    return bool(a and b) and set(a) <= set(b) and a[-1] == b[-1]


def name_words(name: str) -> list[str]:
    return [w for w in norm(name).split() if len(w) >= 2]


def batch_numbers(only: str | None) -> set[int] | None:
    """`--only 530,531` или `530-559` → номера пачек; None — все."""
    if not only:
        return None
    out = set()
    for part in only.split(","):
        a, _, b = part.strip().partition("-")
        out |= set(range(int(a), int(b or a) + 1))
    return out


def load_out(only: set[int] | None = None) -> dict[str, dict]:
    """Ответы модели по id книги: из `out_NNNN.txt` (v2, номер → id по `in_NNNN.json`) и старых
    `out_NNNN.jsonl`. Строка, номер которой не из пачки, пропускается."""
    done = {}
    for name in sorted(os.listdir(DIR)):
        m = re.fullmatch(r"out_(\d{4})\.(txt|jsonl)", name)
        if not m or (only is not None and int(m.group(1)) not in only):
            continue
        path = os.path.join(DIR, name)
        if m.group(2) == "txt":
            books = json.load(open(os.path.join(DIR, f"in_{m.group(1)}.json"), encoding="utf-8"))
            ids = {b["n"]: b["id"] for b in books if "n" in b}
            for line in open(path, encoding="utf-8"):
                r = parse_line(line)
                if r and r["n"] in ids:
                    r["id"] = ids[r["n"]]
                    done[r["id"]] = r
            continue
        for line in open(path, encoding="utf-8"):
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(r, dict) and isinstance(r.get("id"), str):
                done[r["id"]] = r
    return done


def archive_plan(args) -> None:
    os.makedirs(DIR, exist_ok=True)
    conn = connect()
    with conn.cursor() as cur:
        cur.execute(ARCHIVE)
        rows = cur.fetchall()
    conn.close()
    reasons = defaultdict(int)
    with open(os.path.join(DIR, "archive_plan.jsonl"), "w", encoding="utf-8") as f:
        for id_, slug, title, reason, size in rows:
            reasons[reason] += 1
            f.write(json.dumps({"id": id_, "slug": slug, "title": title, "reason": reason,
                                "size": size}, ensure_ascii=False) + "\n")
    log.info("в архив: %d (%s) → %s", len(rows), dict(reasons), os.path.join(DIR, "archive_plan.jsonl"))


def export(args) -> None:
    os.makedirs(DIR, exist_ok=True)
    # Неразмеченные пачки уже есть — новая выгрузка взяла бы те же книги повторно (out_* нет,
    # значит они не «готовы»): за них заплатили бы дважды. Сначала доразметить.
    pending = sorted(n for n in os.listdir(DIR) if re.fullmatch(r"in_\d{4}\.txt", n)
                     and not os.path.exists(os.path.join(DIR, n.replace("in_", "out_"))))
    if pending:
        sys.exit(f"есть неразмеченные пачки ({len(pending)}: {pending[0][3:7]}–{pending[-1][3:7]}) — "
                 "сначала разметьте их, новая выгрузка повторила бы те же книги")
    done = load_out()
    conn = connect()
    with conn.cursor() as cur:
        cur.execute("SELECT slug FROM tags WHERE kind = 'genre' ORDER BY slug")
        genres = [g for (g,) in cur.fetchall()]
        name = getattr(args, "order", None) or "top"
        sql = CANDIDATES.replace("{order}", ORDERS[name]).replace("{extra}", EXTRA.get(name, ""))
        if name in EXTRA:
            done = {}  # перепроверка: уже размеченные берутся снова
        cur.execute(sql, (args.limit + len(done),))
        cols = [d[0] for d in cur.description]
        rows = [dict(zip(cols, r)) for r in cur.fetchall()]
    conn.close()
    with open(os.path.join(DIR, "system_prompt.txt"), "w", encoding="utf-8") as f:
        # Только slug'и: по ним модель понимает жанр, а русские названия удвоили бы промпт.
        f.write(SYSTEM_PROMPT + ", ".join(genres) + "\n")
    with open(os.path.join(DIR, "agent_task.md"), "w", encoding="utf-8") as f:
        f.write(AGENT_TASK.format(dir=DIR))
    rows = [r for r in rows if r["id"] not in done][: args.limit]
    used = [int(m.group(2)) for n in os.listdir(DIR)
            if (m := re.fullmatch(r"(in|out)_(\d{4})\.(json|jsonl|txt)", n))]
    base = max(used) + 1 if used else 0
    batches = 0
    for i in range(0, len(rows), args.chunk):
        chunk = rows[i:i + args.chunk]
        number = base + batches
        # Для модели — короткие строки с номером; для нас — номер → id (модель его не читает).
        with open(os.path.join(DIR, f"in_{number:04d}.txt"), "w", encoding="utf-8") as f:
            f.write("\n".join(book_line(n, r) for n, r in enumerate(chunk, 1)) + "\n")
        with open(os.path.join(DIR, f"in_{number:04d}.json"), "w", encoding="utf-8") as f:
            json.dump([{"n": n, "id": r["id"], "title": r["title"]} for n, r in enumerate(chunk, 1)],
                      f, ensure_ascii=False)
        batches += 1
    log.info("пачек: %d (с %04d), книг к разметке %d, уже готово %d", batches, base, len(rows), len(done))


MODEL = "claude-haiku-5-5"


def api_keys(provider: str) -> list[str]:
    """Ключи: `LLM_API_KEYS` (через запятую, для любого провайдера), иначе ключ провайдера по
    умолчанию — `ANTHROPIC_API_KEY(S)` или `OPENAI_API_KEY`. Пусто у Anthropic — SDK возьмёт
    профиль `ant auth login`."""
    names = ["LLM_API_KEYS"] + (["ANTHROPIC_API_KEYS", "ANTHROPIC_API_KEY"] if provider == "anthropic"
                                else ["OPENAI_API_KEYS", "OPENAI_API_KEY"])
    raw = next((os.environ[n] for n in names if os.environ.get(n)), "")
    return [k.strip() for k in raw.split(",") if k.strip()]


def resolve_provider(args) -> None:
    """Провайдер ИИ из админки (`catalog_ai_providers`, раздел «Провайдеры ИИ»): `--ai SLUG`
    или провайдер по умолчанию. Всё — протокол, URL, модель, ключи, потоки, книг в запросе,
    effort — берётся из его настроек. Ключи расшифровывает Postgres секретом `AI_SECRET_KEY`
    (в dev — `dev-ai-secret`). Провайдера нет — ключи из `LLM_API_KEYS`, Claude Haiku, effort low."""
    args.keys = None
    args.provider, args.base_url, args.model = "anthropic", None, MODEL
    args.workers, args.per_request, args.effort = 8, 100, "low"
    conn = connect()
    with conn.cursor() as cur:
        cur.execute("SELECT to_regclass('catalog_ai_providers') IS NOT NULL")
        row = None
        if cur.fetchone()[0]:
            cur.execute(
                """SELECT slug, kind, base_url, model, pgp_sym_decrypt(api_keys, %s), workers,
                          per_request, effort
                   FROM catalog_ai_providers
                   WHERE CASE WHEN %s::text IS NULL THEN is_default ELSE slug = %s END""",
                (os.environ.get("AI_SECRET_KEY") or "dev-ai-secret", args.ai, args.ai))
            row = cur.fetchone()
    conn.close()
    if row is None:
        if args.ai:
            sys.exit(f"провайдера ИИ «{args.ai}» нет — добавьте в админке: «Провайдеры ИИ»")
        log.info("провайдер ИИ не настроен: ключи из LLM_API_KEYS, %s", MODEL)
        return
    slug, args.provider, args.base_url, args.model, keys, args.workers, args.per_request, args.effort = row
    args.keys = [k for k in keys.split(",") if k]
    log.info("провайдер ИИ: %s (%s, %s, effort %s, ключей %d)",
             slug, args.provider, args.model, args.effort, len(args.keys))


USAGE = {"requests": 0, "input": 0, "cached": 0, "output": 0, "reasoning": 0}
_USAGE_LOCK = __import__("threading").Lock()


def count_usage(inp: int, cached: int, out: int, reasoning: int) -> None:
    """Токены запроса — в общий счёт (итог в логе после разметки)."""
    with _USAGE_LOCK:
        USAGE["requests"] += 1
        USAGE["input"] += inp
        USAGE["cached"] += cached
        USAGE["output"] += out
        USAGE["reasoning"] += reasoning


def usage_line() -> str:
    u = USAGE
    if not u["requests"]:
        return "токены: запросов не было"
    hit = 100 * u["cached"] / u["input"] if u["input"] else 0
    return (f"токены: запросов {u['requests']}, вход {u['input']:,} (из кеша {u['cached']:,} = {hit:.0f}%), "
            f"выход {u['output']:,} (из них размышления {u['reasoning']:,}); "
            f"на запрос: вход {u['input'] // u['requests']:,}, выход {u['output'] // u['requests']:,}").replace(",", " ")


def make_asker(args, system_text: str):
    """Функция «запрос → текст ответа» для провайдера и список клиентов (по ключу на клиента).

    - `anthropic` — Claude через официальный SDK, системный промпт кешируется;
    - `openai` — любой OpenAI-совместимый API (OpenAI, DeepSeek, OpenRouter, Qwen, Gemini,
      локальные Ollama / vLLM): адрес — `--base-url` или `LLM_BASE_URL` / `OPENAI_BASE_URL`."""
    keys = getattr(args, "keys", None) or api_keys(args.provider)
    if args.provider == "anthropic":
        import anthropic
        clients = [anthropic.Anthropic(api_key=k, max_retries=8) for k in keys] or [anthropic.Anthropic(max_retries=8)]
        system = [{"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}]

        # none — без effort: размышления у Claude и так выключены, если не включать thinking
        extra = {} if args.effort == "none" else {"output_config": {"effort": args.effort}}

        def ask(client, content: str) -> str | None:
            response = client.messages.create(
                model=args.model, max_tokens=16000, system=system,
                messages=[{"role": "user", "content": content}], **extra)
            u = response.usage
            count_usage(u.input_tokens + (u.cache_read_input_tokens or 0) + (u.cache_creation_input_tokens or 0),
                        u.cache_read_input_tokens or 0, u.output_tokens, 0)
            if response.stop_reason == "refusal":
                return None
            return "".join(b.text for b in response.content if b.type == "text")
        return ask, clients, (anthropic.APIError,)

    import openai
    if not keys:
        sys.exit("нет ключей: добавьте провайдера в админке («Провайдеры ИИ»)")
    clients = [openai.OpenAI(api_key=k, base_url=args.base_url, max_retries=8) for k in keys]
    # `reasoning_effort` понимают только модели с рассуждением: модель без него отвечает 400 —
    # тогда параметр больше не шлётся.
    # none — размышления выключены (`thinking: disabled`, так у DeepSeek); иначе reasoning_effort.
    effort = ({"extra_body": {"thinking": {"type": "disabled"}}} if args.effort == "none"
              else {"reasoning_effort": args.effort})

    def done(response) -> str:
        u = response.usage
        if u is not None:
            details = getattr(u, "prompt_tokens_details", None)
            cached = (getattr(u, "prompt_cache_hit_tokens", None)          # DeepSeek
                      or (getattr(details, "cached_tokens", None) if details else None) or 0)
            out_details = getattr(u, "completion_tokens_details", None)
            reasoning = (getattr(out_details, "reasoning_tokens", None) if out_details else None) or 0
            count_usage(u.prompt_tokens or 0, cached, u.completion_tokens or 0, reasoning)
        return response.choices[0].message.content or ""

    def ask(client, content: str) -> str | None:
        messages = [{"role": "system", "content": system_text}, {"role": "user", "content": content}]
        if effort:
            try:
                return done(client.chat.completions.create(model=args.model, messages=messages, **effort))
            except openai.BadRequestError as error:
                text = str(error).lower()
                if "reasoning" not in text and "thinking" not in text:
                    raise
                log.info("модель %s не понимает %s — параметр не передаётся", args.model, list(effort))
                effort.clear()
        return done(client.chat.completions.create(model=args.model, messages=messages))
    return ask, clients, (openai.APIError,)


def classify(args) -> None:
    """Разметка без агентов Claude Code: пачки `in_NNNN.json` без готового `out_NNNN.jsonl` —
    напрямую в API Anthropic (ключи по кругу, по `--per-request` книг на запрос). Файл
    результата пишется целиком, когда пачка готова: прерванный прогон продолжается."""
    from concurrent.futures import ThreadPoolExecutor, as_completed

    resolve_provider(args)
    system_text = open(os.path.join(DIR, "system_prompt.txt"), encoding="utf-8").read()
    request, clients, api_errors = make_asker(args, system_text)
    # Только пачки v2 (`in_NNNN.txt`) без готового ответа.
    todo = sorted(
        int(m.group(1)) for n in os.listdir(DIR)
        if (m := re.fullmatch(r"in_(\d{4})\.txt", n))
        and not os.path.exists(os.path.join(DIR, f"out_{m.group(1)}.txt")))
    if getattr(args, "batches", 0):
        todo = todo[: args.batches]
    log.info("пачек к разметке: %d, ключей: %d, потоков: %d", len(todo), len(clients), args.workers)

    def run(slot: int, number: int) -> tuple[int, int, int]:
        client = clients[slot % len(clients)]
        lines = [l for l in open(os.path.join(DIR, f"in_{number:04d}.txt"), encoding="utf-8").read().splitlines() if l]
        got = {}
        for i in range(0, len(lines), args.per_request):
            part = lines[i:i + args.per_request]
            wanted = {int(l.split("|", 1)[0]) for l in part}
            text = request(client, "\n".join(part))
            if text is None:
                log.warning("пачка %04d: отказ модели на %d книгах — пропущены", number, len(part))
                continue
            for line in text.splitlines():
                r = parse_line(line)
                if r and r["n"] in wanted:
                    got[r["n"]] = line.strip()
        path = os.path.join(DIR, f"out_{number:04d}.txt")
        with open(path + ".tmp", "w", encoding="utf-8") as f:
            f.write("\n".join(got[n] for n in sorted(got)) + "\n")
        os.replace(path + ".tmp", path)
        return number, len(got), len(lines)

    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = [pool.submit(run, slot, n) for slot, n in enumerate(todo)]
        for done, future in enumerate(as_completed(futures), 1):
            try:
                number, got, total = future.result()
                log.info("%d/%d: пачка %04d — %d из %d", done, len(todo), number, got, total)
                if done % 10 == 0:
                    log.info(usage_line())
            except api_errors as error:
                log.warning("%d/%d: пачка не размечена: %s", done, len(todo), error)
                if getattr(error, "status_code", None) in (401, 402, 403):
                    # нет денег / ключ отозван — остальные пачки упадут так же: не ждать их
                    log.error("провайдер отказал (%s) — разметка остановлена, пачки останутся на потом",
                              getattr(error, "status_code", None))
                    for f in futures:
                        f.cancel()
                    break
    log.info(usage_line())


def apply(args) -> None:
    results = load_out(batch_numbers(args.only))
    if not results:
        sys.exit("нет out_*.jsonl — сначала export и разметка")
    ids = list(results)
    conn = connect()
    cur = conn.cursor(cursor_factory=psycopg2.extras.DictCursor)
    cur.execute("SELECT id::text, slug FROM tags WHERE kind = 'genre'")
    genre_id = {s: i for i, s in cur.fetchall()}
    cur.execute(
        """SELECT e.id::text, e.title, e.cover_url IS NOT NULL AS cover,
                  coalesce((e.metadata -> 'file' ->> 'size')::bigint, 0) AS size,
                  coalesce(e.metadata ->> 'title_raw', e.title) AS raw_title,
                  e.metadata -> 'collection' ->> 'by' AS marked_by,
                  (e.metadata ->> 'words')::bigint AS words
           FROM entities e WHERE e.id = ANY(%s::uuid[])""", (ids,))
    books = {r["id"]: dict(r) for r in cur.fetchall()}
    cur.execute(
        """SELECT ec.entity_id::text, p.id::text, p.full_name FROM entity_credits ec
           JOIN people p ON p.id = ec.person_id
           WHERE ec.entity_id = ANY(%s::uuid[]) AND ec.role = 'author' ORDER BY ec.entity_id, ec.position""",
        (ids,))
    credits = defaultdict(list)
    for eid, pid, name in cur.fetchall():
        credits[eid].append((pid, name))
    cur.execute(
        """SELECT et.entity_id::text, t.slug FROM entity_tags et JOIN tags t ON t.id = et.tag_id
           WHERE et.entity_id = ANY(%s::uuid[]) AND t.kind = 'genre'""", (ids,))
    have = defaultdict(set)
    for eid, slug in cur.fetchall():
        have[eid].add(slug)

    plan = {"titles": [], "genres_add": [], "genres_del": [], "not_book": [], "kinds": [], "collections": [],
            "uncollect": [], "merges": [], "editions": []}
    canonical = defaultdict(set)  # person_id -> полные имена от Haiku
    names = {}
    for eid, r in results.items():
        if eid not in books:
            continue
        kind = r.get("type") if r.get("type") in TYPES else "other"
        # «Мусор» от модели скрывается, только если и название мусорное (`JUNK_TITLE`:
        # «i 1e1bb09d45949090», «Unknown», «Оглавление»…). Иначе это книга, которую модель
        # просто не узнала, — остаётся обычной книгой.
        if kind in NOT_BOOK and not JUNK_TITLE.search(books[eid]["raw_title"] or ""):
            kind = "fiction"
        if kind in NOT_BOOK:
            plan["not_book"].append((eid, books[eid]["title"], kind))
            continue
        words = books[eid].get("words")
        if kind == "article" and words and words >= ARTICLE_MAX_WORDS:
            kind = "nonfiction"  # длинный текст — книга, не статья
        if kind in MARK_KIND:
            plan["kinds"].append((eid, books[eid]["title"], kind))
        # Сборник: по ответу модели (`collection`, `books_inside` > 1) или по исходному названию.
        inside = r.get("books_inside") if isinstance(r.get("books_inside"), int) else None
        by_model = kind == "collection" or (inside or 0) > 1
        # Сборник решает только модель: правило по названию путало тома одного романа
        # («Война и мир. Тома 1-4») со сборником. Пометка по названию, с которой модель не
        # согласна, снимается.
        if by_model and books[eid]["marked_by"] != "llm":
            plan["collections"].append((eid, books[eid]["raw_title"], inside, "llm"))
        elif not by_model and books[eid]["marked_by"] == "title":
            plan["uncollect"].append((eid, books[eid]["raw_title"]))
        clean = (r.get("clean_title") or "").strip()
        # Модель меняет «№» на «No» и «…» на «...» — возвращаем символы оригинала.
        if "№" in books[eid]["title"]:
            clean = re.sub(r"\bNo(?=\s*\d)", "№", clean)
        if "…" in books[eid]["title"]:
            clean = clean.replace("...", "…")
        # Номер тома/части пропал — название не трогаем: «Тарзан. Том 5» ≠ «Тарзан».
        if clean and not volumes(books[eid]["title"]) <= volumes(clean):
            clean = ""
            r["clean_title"] = None
        if clean and clean != books[eid]["title"] and len(clean) <= 500:
            plan["titles"].append((eid, books[eid]["title"], clean))
        for g in r.get("genres_add") or []:
            if g in genre_id and g not in have[eid]:
                plan["genres_add"].append((eid, g))
        # Последний жанр не снимается без замены: книга без жанра выпадает из всех фильтров.
        added = [g for g in r.get("genres_add") or [] if g in genre_id and g not in have[eid]]
        wrong = [g for g in r.get("genres_wrong") or [] if g in have[eid]]
        if wrong and not added and len(wrong) >= len(have[eid]):
            wrong = wrong[:-1]
        for g in wrong:
            plan["genres_del"].append((eid, g))
        got = r.get("authors") or []
        if len(got) == len(credits[eid]):
            for (pid, name), full in zip(credits[eid], got):
                if isinstance(full, str) and full.strip():
                    # Только полнее того же имени; иначе имя остаётся как есть.
                    canonical[pid].add(full.strip() if fuller(name, full.strip()) else name)
                    names[pid] = name

    # 100% дубли людей: одно полное имя от Haiku и слова одного имени — подмножество другого.
    by_name = defaultdict(set)
    for pid, fulls in canonical.items():
        if len(fulls) == 1 and "неизвестн" not in names[pid].lower():
            by_name[next(iter(fulls))].add(pid)
    if by_name:
        cur.execute(
            """SELECT p.id::text, p.full_name, count(ec.*) FROM people p
               LEFT JOIN entity_credits ec ON ec.person_id = p.id
               WHERE p.full_name = ANY(%s) GROUP BY p.id""", (list(by_name),))
        for pid, name, _ in cur.fetchall():
            if "неизвестн" not in name.lower():
                by_name[name].add(pid)
                names[pid] = name
    people_ids = list({p for s in by_name.values() for p in s})
    cur.execute(
        """SELECT p.id::text, count(ec.*) FROM people p LEFT JOIN entity_credits ec ON ec.person_id = p.id
           WHERE p.id = ANY(%s::uuid[]) GROUP BY p.id""", (people_ids,))
    weight = dict(cur.fetchall())
    for full, pids in by_name.items():
        # 100% — только полное имя с отчеством: «Юрий Александров» или «А. Сергеев» у двух
        # записей — возможно, разные люди.
        if len([w for w in name_words(full) if len(w) > 1]) < 3:
            continue
        full_words = set(name_words(full))
        same = [p for p in pids if set(name_words(names[p])) <= full_words
                and name_words(names[p]) and name_words(names[p])[-1] == name_words(full)[-1]]
        if len(same) < 2:
            continue
        keep = max(same, key=lambda p: (weight.get(p, 0), len(names[p]), p))
        plan["merges"].append((keep, full, [p for p in same if p != keep]))

    # Сборники и не-книги изданиями не бывают: «Собрание сочинений, том 3» ≠ издание романа.
    skip_editions = ({n[0] for n in plan["not_book"]} | {c[0] for c in plan["collections"]}
                     | {k[0] for k in plan["kinds"]})
    skip_editions |= {e for e, b in books.items() if b.get("marked")}
    # Издания: тот же ключ названия (очищенного) и та же фамилия первого автора.
    groups = defaultdict(set)
    surname = {}
    for eid, r in results.items():
        if eid not in books or eid in skip_editions:
            continue
        title = edition_key(books[eid]["title"])
        authors = r.get("authors") or [n for _, n in credits[eid]]
        if not title or not authors:
            continue
        surname[eid] = name_words(authors[0])[-1] if name_words(authors[0]) else ""
        groups[(title, volumes(books[eid]["title"]), surname[eid])].add(eid)
    keys = list({k[0] for k in groups})
    if keys:
        cur.execute(
            f"""SELECT e.id::text, lower(e.title), e.cover_url IS NOT NULL,
                       coalesce((e.metadata -> 'file' ->> 'size')::bigint, 0),
                       (SELECT p.full_name FROM entity_credits ec JOIN people p ON p.id = ec.person_id
                        WHERE ec.entity_id = e.id AND ec.role = 'author' ORDER BY ec.position LIMIT 1)
                FROM entities e WHERE {VISIBLE}
                  AND btrim(regexp_replace(lower(replace(e.title, 'ё', 'е')), '[^[:alnum:]]+', ' ', 'g')) = ANY(%s)""",
            ([k for k in keys],))
        for eid, title, cover, size, author in cur.fetchall():
            words = name_words(author or "")
            key = (edition_key(title), volumes(title), words[-1] if words else "")
            if key in groups:
                groups[key].add(eid)
                books.setdefault(eid, {"id": eid, "title": title, "cover": cover, "size": size})
    for key, members in groups.items():
        if len(members) < 2:
            continue
        main = max(members, key=lambda e: (books[e]["cover"], books[e]["size"], e))
        plan["editions"].append((main, key[0], sorted(m for m in members if m != main)))

    report(plan, names)
    if not args.apply:
        log.info("сухой прогон: ничего не записано (отчёт %s)", os.path.join(DIR, "apply_report.txt"))
        return
    write(conn, plan, genre_id, args.hide_collections)
    conn.close()
    mark_applied(batch_numbers(args.only) or set(out_batches()))


def report(plan, names) -> None:
    lines = []
    for key, items in plan.items():
        lines.append(f"# {key}: {len(items)}")
    lines.append("")
    lines.append("## Названия (было → стало)")
    lines += [f"{a} → {b}" for _, a, b in plan["titles"]]
    lines.append("\n## Не книги")
    lines += [f"[{k}] {t}" for _, t, k in plan["not_book"]]
    lines.append("\n## Статьи и документы (только пометка metadata.kind)")
    lines += [f"[{k}] {t}" for _, t, k in plan["kinds"]]
    lines.append("\n## Сборники (книг внутри, откуда)")
    lines += [f"[{n or '?'}, {src}] {t}" for _, t, n, src in plan["collections"]]
    lines.append("\n## Не сборник: снять пометку по названию (модель: одна книга)")
    lines += [t for _, t in plan["uncollect"]]
    lines.append("\n## Слияние людей (основная ← дубли)")
    lines += [f"{full}: {names.get(keep)} ← {', '.join(names.get(d, d) for d in dups)}"
              for keep, full, dups in plan["merges"]]
    lines.append("\n## Издания (основная ← издания)")
    lines += [f"{title}: {main} ← {len(rest)} изд." for main, title, rest in plan["editions"]]
    lines.append("\n## Жанры: добавить / снять")
    lines += [f"+ {e} {g}" for e, g in plan["genres_add"]]
    lines += [f"- {e} {g}" for e, g in plan["genres_del"]]
    with open(os.path.join(DIR, "apply_report.txt"), "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    for key, items in plan.items():
        log.info("%s: %d", key, len(items))


# Пользовательские данные о человеке: с ними дубль не удаляется (решать руками).
USER_REFS = ["user_person_interests", "person_likes", "person_notes", "chat_rooms", "award_nominations"]


def absorb(cur, keep: str, dup: str) -> bool:
    """Дубль `dup` без участий — в `keep`: связи импорта (TMDB, Wikidata) переносятся, запись
    удаляется (счётчики и статистика — каскадом). Есть пользовательские данные или ещё участия —
    не удаляется. True — удалён."""
    cur.execute("SELECT 1 FROM entity_credits WHERE person_id = %s LIMIT 1", (dup,))
    if cur.fetchone():
        return False
    for table in USER_REFS:
        cur.execute(f"SELECT 1 FROM public.{table} WHERE person_id = %s LIMIT 1", (dup,))
        if cur.fetchone():
            return False
    cur.execute("UPDATE wikidata_import.people SET person_id = %s WHERE person_id = %s", (keep, dup))
    cur.execute("SELECT tmdb_id, qid FROM catalog_import.people WHERE person_id = %s", (dup,))
    row = cur.fetchone()
    if row:
        cur.execute("DELETE FROM catalog_import.people WHERE person_id = %s", (dup,))
        cur.execute(
            """INSERT INTO catalog_import.people (person_id, tmdb_id, qid, created)
               VALUES (%s, %s, %s, false)
               ON CONFLICT (person_id) DO UPDATE SET
                 tmdb_id = coalesce(catalog_import.people.tmdb_id, EXCLUDED.tmdb_id),
                 qid = coalesce(catalog_import.people.qid, EXCLUDED.qid)""",
            (keep, row[0], row[1]))
    cur.execute("DELETE FROM people WHERE id = %s", (dup,))
    return True


def people_cleanup(args) -> None:
    """Дубли, слитые раньше (участия уже у основной записи): пары берутся из отчёта
    `apply_report.txt` (раздел «Слияние людей») по именам."""
    path = os.path.join(DIR, "apply_report.txt")
    pairs = []
    section = False
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("## "):
            section = line.startswith("## Слияние людей")
            continue
        if section and ": " in line and " ← " in line:
            full, rest = line.split(": ", 1)
            dups = rest.split(" ← ", 1)[1].split(", ")
            pairs.append((full, dups))
    conn = connect()
    cur = conn.cursor()
    deleted = kept = 0
    for full, dups in pairs:
        cur.execute(
            """SELECT p.id::text FROM people p WHERE p.full_name = %s
               ORDER BY (SELECT count(*) FROM entity_credits ec WHERE ec.person_id = p.id) DESC LIMIT 1""",
            (full,))
        row = cur.fetchone()
        if not row:
            continue
        keep = row[0]
        cur.execute(
            """SELECT p.id::text FROM people p WHERE p.full_name = ANY(%s) AND p.id <> %s
               AND NOT EXISTS (SELECT 1 FROM entity_credits ec WHERE ec.person_id = p.id)""",
            (dups, keep))
        for (dup,) in cur.fetchall():
            if absorb(cur, keep, dup):
                deleted += 1
            else:
                kept += 1
    if args.apply:
        conn.commit()
        log.info("удалено дублей: %d, оставлено (есть данные пользователей): %d", deleted, kept)
    else:
        conn.rollback()
        log.info("сухой прогон: удалилось бы %d, осталось бы %d; запись — --apply", deleted, kept)
    conn.close()


def write(conn, plan, genre_id, hide_collections: bool = False) -> None:
    cur = conn.cursor()
    for eid, _, kind in plan["kinds"]:
        cur.execute(
            """UPDATE entities SET metadata = jsonb_set(coalesce(metadata, '{}'), '{kind}', to_jsonb(%s::text))
               WHERE id = %s""", (kind, eid))
    for eid, _, inside, source in plan["collections"]:
        mark = json.dumps({"books": inside, "by": source})
        cur.execute(
            """UPDATE entities SET metadata = jsonb_set(coalesce(metadata, '{}'), '{collection}', %s::jsonb)
                 || CASE WHEN %s THEN '{"archive_reason": "collection"}'::jsonb ELSE '{}'::jsonb END,
                 active = CASE WHEN %s THEN false ELSE active END
               WHERE id = %s""", (mark, hide_collections, hide_collections, eid))
    for eid, _ in plan["uncollect"]:
        cur.execute(
            """UPDATE entities SET metadata = metadata - 'collection', updated_at = now()
               WHERE id = %s AND metadata -> 'collection' ->> 'by' = 'title'""", (eid,))
    for eid, old, new in plan["titles"]:
        cur.execute(
            """UPDATE entities SET title = %s,
                 metadata = jsonb_set(coalesce(metadata, '{}'), '{title_raw}', to_jsonb(%s::text))
               WHERE id = %s""", (new, old, eid))
    for eid, g in plan["genres_add"]:
        cur.execute("INSERT INTO entity_tags (entity_id, tag_id) VALUES (%s, %s) ON CONFLICT DO NOTHING",
                    (eid, genre_id[g]))
    for eid, g in plan["genres_del"]:
        cur.execute("DELETE FROM entity_tags WHERE entity_id = %s AND tag_id = %s", (eid, genre_id[g]))
    for eid, _, kind in plan["not_book"]:
        cur.execute(
            """UPDATE entities SET active = false, active_manual = false,
                 metadata = jsonb_set(coalesce(metadata, '{}'), '{archive_reason}', to_jsonb(%s::text))
               WHERE id = %s""", (f"not_a_book:{kind}", eid))
    deleted = 0
    for keep, full, dups in plan["merges"]:
        cur.execute("UPDATE people SET full_name = %s WHERE id = %s", (full, keep))
        for dup in dups:
            # Участие, которое у основной записи уже есть (та же книга и роль), не переносится.
            cur.execute(
                """UPDATE entity_credits ec SET person_id = %s WHERE ec.person_id = %s
                   AND NOT EXISTS (SELECT 1 FROM entity_credits k WHERE k.person_id = %s
                                   AND k.entity_id = ec.entity_id AND k.role = ec.role)""",
                (keep, dup, keep))
            cur.execute("DELETE FROM entity_credits WHERE person_id = %s", (dup,))
            if absorb(cur, keep, dup):
                deleted += 1
    for main, _, rest in plan["editions"]:
        cur.execute("UPDATE entities SET parent_id = %s WHERE id = ANY(%s::uuid[])", (main, rest))
        # Ручное место издания — основной книге, если у неё своего нет.
        cur.execute(
            """UPDATE catalog_manual_ranks SET entity_id = %s
               WHERE entity_id = (SELECT entity_id FROM catalog_manual_ranks
                                  WHERE entity_id = ANY(%s::uuid[]) ORDER BY rank LIMIT 1)
                 AND NOT EXISTS (SELECT 1 FROM catalog_manual_ranks WHERE entity_id = %s)""",
            (main, rest, main))
        cur.execute("DELETE FROM catalog_manual_ranks WHERE entity_id = ANY(%s::uuid[])", (rest,))
    conn.commit()
    log.info("записано; людей удалено: %d", deleted)


APPLIED = "applied.txt"


def out_batches() -> list[int]:
    return sorted(int(m.group(1)) for n in os.listdir(DIR)
                  if (m := re.fullmatch(r"out_(\d{4})\.(txt|jsonl)", n)))


def applied_batches() -> set[int]:
    """Пачки, уже записанные в БД (`applied.txt`, по номеру в строке)."""
    path = os.path.join(DIR, APPLIED)
    if not os.path.exists(path):
        return set()
    return {int(l) for l in open(path, encoding="utf-8").read().split() if l.isdigit()}


def mark_applied(numbers: set[int]) -> None:
    with open(os.path.join(DIR, APPLIED), "a", encoding="utf-8") as f:
        f.write("".join(f"{n:04d}\n" for n in sorted(numbers - applied_batches())))


def span(numbers: list[int]) -> str:
    """[531, 532, 533, 540] → «531-533,540» для `--only`."""
    parts, start = [], None
    for i, n in enumerate(numbers):
        if start is None:
            start = n
        if i + 1 == len(numbers) or numbers[i + 1] != n + 1:
            parts.append(f"{start}" if start == n else f"{start}-{n}")
            start = None
    return ",".join(parts)


def progress() -> dict:
    """Где мы: сколько книг к чистке всего, сколько размечено и записано, сколько осталось.
    Пишется в `progress.json` (админка показывает его над формой) и в лог.

    Повторов нет: выгрузка берёт книги по порядку (место в ручном списке, потом размер файла) и
    пропускает уже размеченные (`out_*`), поэтому offset не нужен — каждый запуск берёт
    следующие."""
    done = load_out()
    applied = applied_batches()
    written = sum(1 for n in out_batches() if n in applied)
    # записано = размеченные книги из записанных пачек (книги, пропущенные моделью, не в счёт —
    # они вернутся в следующую выгрузку)
    written_books = sum(1 for eid in load_out(set(applied)) if eid in done)
    exported = sum(len(json.load(open(os.path.join(DIR, n), encoding="utf-8")))
                   for n in os.listdir(DIR) if re.fullmatch(r"in_\d{4}\.json", n))
    conn = connect()
    with conn.cursor() as cur:
        cur.execute(PROGRESS, (list(done),))
        total, done_visible = cur.fetchone()
    conn.close()
    batches = out_batches()
    out = {
        "total": total,                       # видимых книг-кандидатов сейчас
        "classified": len(done),              # размечено ИИ (всего, с начала)
        "written": written_books,             # из них записано в БД
        "remaining": max(total - done_visible, 0),  # ещё не размечено
        "pending_write": max(len(done) - written_books, 0),  # размечено, но не записано
        "exported_not_classified": max(exported - len(done), 0),
        "last_batch": batches[-1] if batches else None,
        "batches_written": written,
        "updated_at": __import__("datetime").datetime.now().isoformat(timespec="seconds"),
    }
    with open(os.path.join(DIR, "progress.json"), "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=1)
    pct = 100 * (total - out["remaining"]) / total if total else 100
    log.info("ПРОГРЕСС: размечено %d (записано %d), осталось %d из %d (%.1f%%), последняя пачка %s",
             out["classified"], out["written"], out["remaining"], total, pct,
             f"{out['last_batch']:04d}" if out["last_batch"] is not None else "—")
    return out


def run(args) -> None:
    """Весь цикл одной командой: выгрузка (если неразмеченных пачек нет) → разметка
    провайдером из админки → сухой прогон с отчётом → вопрос → запись, чистка людей.
    Повторный запуск продолжает с места остановки. Код выхода 0 — записано (дальше
    `make books-llm` пересобирает индекс и ручной порядок), 3 — ничего не записано."""
    os.makedirs(DIR, exist_ok=True)
    progress()
    pending_in = [n for n in os.listdir(DIR) if re.fullmatch(r"in_\d{4}\.txt", n)
                  and not os.path.exists(os.path.join(DIR, n.replace("in_", "out_")))]
    unapplied = [n for n in out_batches() if n not in applied_batches()]
    if not pending_in and not unapplied:
        log.info("== 1. выгрузка %d книг", args.limit)
        log.info("порядок: %s", args.order)
        export(argparse.Namespace(limit=args.limit, chunk=100, order=args.order))
    else:
        log.info("== 1. новые книги не берутся: есть неразмеченные (%d) или незаписанные (%d) пачки",
                 len(pending_in), len(unapplied))
        if unapplied:
            log.warning("ВНИМАНИЕ: %d пачек уже обработаны ИИ, но не записаны в БД — повторно они "
                        "НЕ обрабатываются. Запишите их (--yes / кнопка «Записать в БД»).", len(unapplied))
    log.info("== 2. разметка")
    classify(args)
    todo = [n for n in out_batches() if n not in applied_batches()]
    if not todo:
        log.info("нечего записывать")
        sys.exit(3)
    only = span(todo)
    log.info("== 3. сухой прогон пачек %s", only)
    apply(argparse.Namespace(only=only, apply=False, hide_collections=False))
    log.info("отчёт с примерами: %s", os.path.join(DIR, "apply_report.txt"))
    progress()
    if not args.yes:
        try:
            answer = input("Записать в БД? [y/N] ").strip().lower()
        except EOFError:
            answer = ""
        if answer not in ("y", "yes", "д", "да"):
            log.info("не записано. Повторный запуск продолжит с этого места.")
            sys.exit(3)
    log.info("== 4. запись")
    apply(argparse.Namespace(only=only, apply=True, hide_collections=False))
    people_cleanup(argparse.Namespace(apply=True))
    progress()


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("archive-plan")
    e = sub.add_parser("export")
    e.add_argument("--limit", type=int, default=3000)
    e.add_argument("--chunk", type=int, default=100)
    e.add_argument("--order", choices=list(ORDERS), default="top")
    a = sub.add_parser("apply")
    a.add_argument("--apply", action="store_true")
    a.add_argument("--only", default=None, help="только эти пачки: 530,531 или 530-559")
    a.add_argument("--hide-collections", action="store_true",
                   help="сборники не только пометить, но и скрыть (active = false)")
    k = sub.add_parser("classify", help="разметка через API (свои ключи): Anthropic или OpenAI-совместимый")
    r = sub.add_parser("run", help="всё одной командой: выгрузка → разметка → сухой прогон → вопрос → запись")
    r.add_argument("--limit", type=int, default=5000, help="сколько книг выгрузить, если неразмеченных пачек нет")
    r.add_argument("--yes", action="store_true", help="записать без вопроса")
    r.add_argument("--order", choices=list(ORDERS), default="top",
                   help="какие книги брать: top — ручной список и крупные, newest — последние добавленные")
    for q in (k, r):
        q.add_argument("--ai", default=None,
                       help="провайдер ИИ из админки (имя); по умолчанию — провайдер по умолчанию")
    k.add_argument("--batches", type=int, default=0, help="сколько пачек разметить (0 — все)")
    sub.add_parser("progress", help="сколько книг размечено, записано и осталось")
    c = sub.add_parser("people-cleanup")
    c.add_argument("--apply", action="store_true")
    args = p.parse_args()
    {"archive-plan": archive_plan, "export": export, "classify": classify, "apply": apply,
     "people-cleanup": people_cleanup, "run": run, "progress": lambda _: progress()}[args.cmd](args)


if __name__ == "__main__":
    main()
