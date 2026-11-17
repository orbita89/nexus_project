#!/usr/bin/env python3
"""Аудит фильмов и сериалов через LLM: кураторские теги, чистка жанров, валидность, разархивация.

Два шага, между ними — файл результатов, который можно просмотреть глазами:

  classify  читает из БД активные `movie` / `series` (по убыванию TMDB popularity), спрашивает
            Claude и дописывает по строке JSON на тайтл в `--out` (JSONL). Повторный запуск
            пропускает уже обработанные `id`, поэтому прогон можно прерывать и продолжать.
  apply     читает JSONL и добавляет `entity_tags`: кураторские теги и жанры, которые уже есть в
            `tags` (kind = genre). Ничего не снимает и не переименовывает. По умолчанию сухой
            прогон; запись — `--apply`.

`cleaned_title`, `is_anime`, `is_valid_title`, `restore_from_archive` в БД не пишутся: это отчёт
для людей (мусорные названия и архивная хроника — решать руками, `active` скрипт не трогает).
Холодный архив лежит вне Postgres, поэтому `restore_from_archive` у тайтлов из PG информативен;
разархивацией занимается отдельный процесс.

    ANTHROPIC_API_KEY=... DATABASE_URL=postgres://nexus_user:nexus_password@localhost:5432/nexus_db \\
        python3 tags_llm.py classify --out /tmp/tags.jsonl --limit 200     # пробная выборка
    ... python3 tags_llm.py apply --in /tmp/tags.jsonl                      # посмотреть
    ... python3 tags_llm.py apply --in /tmp/tags.jsonl --apply              # записать

Теги должны существовать (миграция `20261207100000_catalog_curated_tags.sql`). После записи —
`scripts/reindex-search.sh`, чтобы теги попали в Meilisearch. Ключ — только из окружения.
"""

import argparse
import json
import logging
import os
import re
import sys
from concurrent.futures import ThreadPoolExecutor

import psycopg2

log = logging.getLogger("tags_llm")

MODEL = "claude-haiku-5-5"

TAGS = [
    "Авторское кино", "Артхаус", "Фестивальный хит", "Инди / Независимое кино",
    "Культовое авторское кино", "Французская новая волна", "Итальянский неореализм",
    "Новый Голливуд", "Догма 95", "Новая корейская волна", "Медленное кино",
    "Магический реализм", "Сюрреализм", "Мамблкор", "Поэтическое кино", "Экстремальный артхаус",
    "Для семейного просмотра", "Для первого свидания", "Кино под пиво / с попкорном",
    "На один вечер", "Фоновое кино", "На поплакать", "Под одеяло и дождь",
    "Новогодний / Рождественский вайб", "Хэллоуинский спешл", "Кино для двоих",
    "Филгуд", "Комфортное кино", "Гримдарк", "Неоновый нуар", "Ностальгия по 80-м / 90-м",
    "Эстетика заброшенности", "Солнечный кошмар", "Коттеджкор / Пастораль",
    "Киберпанк-декаданс", "Клаустрофобия", "Vibe movie",
    "Боди-хоррор", "Слоубёрнер", "Скрытый финал / Твист", "Ненадежный рассказчик",
    "Временная петля", "Камерное действие", "Смертельная игра", "Найденная пленка",
    "Ограбление / Афера", "Дорожное приключение", "Вторжение в дом", "Месть",
    "Борьба за выживание", "Одержимость", "Космическая изоляция", "Мозгодробилка",
    "Так плохо, что даже хорошо", "Собака не погибает", "Кринж-комедия",
    "Уютная тайна", "Кино экзистенциального кризиса", "Один против всех",
    "Грустный красивый мальчик", "Женская ярость", "Обед/ужин в кадре",
    "Травма поколений", "Стекло", "Основано на реальном скандале",
    "Нелинейное повествование", "Скринлайф", "Альманах / Антология",
    "Мокьюментари", "Реальное время", "Разрушение четвертой стены",
]

_TR = dict(zip("абвгдезийклмнопрстуфъыьэ", "abvgdezijklmnoprstuf'y'e"))
_TR.update({"й": "y", "ё": "e", "ж": "zh", "х": "kh", "ц": "ts", "ч": "ch", "ш": "sh", "щ": "shch",
            "ю": "yu", "я": "ya"})


def slugify(name: str) -> str:
    """Тот же вид, что у существующих slug'ов: `vysokiy-renessans`, `syurrealizm`."""
    s = "".join(_TR.get(c, c) for c in name.lower())
    s = s.replace("'", "")
    return re.sub(r"[^a-z0-9]+", "-", s).strip("-")


SYSTEM = """Ты — ведущий контент-редактор, дата-инженер и киноаналитик базы данных. Проведи аудит тайтла: почисти метаданные, назначь точные теги и определи, нужна ли разархивация.

ПРАВИЛА:
1. Архивная хроника, любительская съёмка, бэкстейдж, трейлер, тизер, выпуск новостей, телепередача, концертное видео, клип, технический/битый файл — не кино. Для них: "is_valid_title": false, "restore_from_archive": false, "tags": [], "cleaned_genres": [].
2. "restore_from_archive": true, если в создании участвовал хотя бы один человек из мирового Топ-10 000 киноперсон (режиссёр, исполнитель заметной роли, шоураннер, сценарист), либо у проекта заметная культурная ценность, культовый статус, высокие оценки или он значим для жанра. false — для проходных неизвестных релизов без таких персон и для видеомусора.
3. "entity_type": "movie" (полнометражный, включая полнометражное аниме) или "series" (сериал, мини-сериал, дорама, аниме-сериал). "is_anime": true для японской анимации или явной стилизации под аниме. "cleaned_title": название на русском без тегов качества, годов в скобках и кривых транслитераций. "cleaned_genres": 1–4 жанра с заглавной буквы (например ["Драма", "Триллер"]), без дублей и мусора.
4. "tags": от 0 до 3 тегов ИСКЛЮЧИТЕЛЬНО из списка ниже. Теги СТРОГИЕ и РЕДКИЕ: тег ставится только если ты уверен, что он определяет именно этот тайтл, любой сомнительный не ставь. Пустой список [] — нормальный и частый ответ (у большинства тайтлов тегов нет или один). Никогда не добавляй теги «для количества» и не выбирай ближайший по смыслу. Не выдумывай теги.
   Особые критерии:
   - «Авторское кино», «Культовое авторское кино», «Артхаус»: только если фильм — личное высказывание автора (режиссёр, часто он же сценарист), с узнаваемым почерком и полным творческим контролем. НЕ ставь для студийных жанровых и коммерческих фильмов, работ «по найму», франшиз, экранизаций бестселлеров, даже если режиссёр знаменит. Примеры «да»: Тарковский, Бергман, Лантимос, Линч, Кубрик. Примеры «нет»: «Семь», «Отступники», «Джанго освобождённый», «Оппенгеймер», блокбастеры.
   - «Кино под пиво / с попкорном», «На один вечер», «Фоновое кино», «Филгуд», «Комфортное кино», «Кино для двоих», «На поплакать», «Для первого свидания», «Для семейного просмотра»: теги повода просмотра. Не больше одного на тайтл и только для яркого случая (должно получаться не более 3–5% тайтлов на тег). «Кино под пиво» — исключительно очевидная безмозглая попкорн-развлекаловка, очень редко. «Для семейного просмотра» — детские и семейные фильмы с явной ориентацией на семью с детьми.
   - Тайтл без явных признаков тега не получает ничего: популярность и известность режиссёра — не признак.
   - Течения и стили («Французская новая волна» — Франция ~1958–1968 и круг режиссёров волны; «Новый Голливуд» — США ~1967–1980; «Итальянский неореализм»; «Догма 95»; «Новая корейская волна»): строго по эпохе, стране и режиссёру.
   - Сюжетные приёмы («Временная петля», «Ограбление / Афера», «Смертельная игра», «Найденная пленка» и т.д.): только если приём — основа сюжета, а не эпизод.
5. top_person_matched — имя человека из Топ-10 000, из-за которого true, иначе null. Не придумывай имена, которых нет в данных или которых ты не знаешь.

СПИСОК РАЗРЕШЁННЫХ ТЕГОВ:
{tags}

ФОРМАТ: на вход приходит JSON-массив тайтлов с полем "id". Ответ — СТРОГО валидный JSON-массив без markdown и без текста вокруг: по одному объекту на каждый входной тайтл, в том же порядке:
{{"id": "<тот же id>", "is_valid_title": true|false, "cleaned_title": "...", "entity_type": "movie"|"series"|null, "is_anime": true|false, "cleaned_genres": [...], "tags": [...], "top_person_matched": "..."|null, "restore_from_archive": true|false, "restore_reason": "..."}}"""

CANDIDATES = """
SELECT e.id::text, e.kind::text, e.title, e.original_title,
       EXTRACT(YEAR FROM e.release_date)::int AS year,
       left(e.description, 350) AS description,
       e.metadata->'countries' AS countries,
       (SELECT array_agg(t.name ORDER BY t.name) FROM entity_tags et
          JOIN tags t ON t.id = et.tag_id AND t.kind = 'genre' WHERE et.entity_id = e.id) AS genres,
       (SELECT array_agg(p.name) FROM (
          SELECT p.full_name AS name FROM entity_credits c JOIN people p ON p.id = c.person_id
          WHERE c.entity_id = e.id AND c.role = 'director' ORDER BY c.position LIMIT 2) p) AS directors,
       (SELECT array_agg(p.name) FROM (
          SELECT p.full_name AS name FROM entity_credits c JOIN people p ON p.id = c.person_id
          WHERE c.entity_id = e.id AND c.role = 'actor' ORDER BY c.position LIMIT 5) p) AS cast
FROM entities e
LEFT JOIN entity_stats st ON st.entity_id = e.id
WHERE e.active AND e.kind IN ('movie', 'series')
ORDER BY st.tmdb_votes DESC NULLS LAST, e.id
OFFSET %s LIMIT %s
"""


def load_done(path: str) -> set[str]:
    done: set[str] = set()
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            for line in f:
                try:
                    done.add(json.loads(line)["id"])
                except (ValueError, KeyError):
                    pass
    return done


def brief(row: dict) -> dict:
    out = {"id": row["id"], "kind": row["kind"], "title": row["title"]}
    for k in ("original_title", "year", "description", "countries", "genres", "directors", "cast"):
        if row.get(k):
            out[k] = row[k]
    return out


def normalize(res: dict, allowed: set[str]) -> dict:
    """Приводит ответ к контракту: теги только из списка, 0–3 штуки; у невалидного — пусто."""
    if not res.get("is_valid_title"):
        res.update(is_valid_title=False, restore_from_archive=False, tags=[], cleaned_genres=[])
        return res
    tags = []
    for t in res.get("tags") or []:
        if t in allowed and t not in tags:
            tags.append(t)
    res["tags"] = tags[:3]
    res["cleaned_genres"] = [g for g in dict.fromkeys(res.get("cleaned_genres") or [])][:4]
    return res


def ask(client, model: str, system: list, batch: list[dict]) -> list[dict]:
    msg = client.messages.create(
        model=model, max_tokens=4096, system=system,
        messages=[{"role": "user", "content": json.dumps([brief(r) for r in batch], ensure_ascii=False)}],
    )
    text = "".join(b.text for b in msg.content if b.type == "text").strip()
    return json.loads(text)


def classify(args) -> None:
    import anthropic

    client = anthropic.Anthropic()
    system = [{"type": "text", "text": SYSTEM.format(tags=json.dumps(TAGS, ensure_ascii=False)),
               "cache_control": {"type": "ephemeral"}}]
    allowed = set(TAGS)
    done = load_done(args.out)
    conn = psycopg2.connect(os.environ["DATABASE_URL"])
    with conn.cursor() as cur:
        cur.execute(CANDIDATES, (args.offset, args.limit))
        cols = [d[0] for d in cur.description]
        rows = [dict(zip(cols, r)) for r in cur.fetchall()]
    conn.close()
    rows = [r for r in rows if r["id"] not in done]
    batches = [rows[i:i + args.batch] for i in range(0, len(rows), args.batch)]
    log.info("к обработке %d тайтлов (%d запросов), уже готово %d", len(rows), len(batches), len(done))

    def work(batch):
        try:
            got = {r["id"]: r for r in ask(client, args.model, system, batch) if isinstance(r, dict)}
        except Exception as e:  # битый JSON или сбой API — батч попробуем при следующем запуске
            log.warning("батч пропущен: %s", e)
            return []
        return [normalize(got[r["id"]], allowed) for r in batch if r["id"] in got]

    ok = 0
    with ThreadPoolExecutor(args.workers) as pool, open(args.out, "a", encoding="utf-8") as out:
        for results in pool.map(work, batches):
            for r in results:
                out.write(json.dumps(r, ensure_ascii=False) + "\n")
            out.flush()
            ok += len(results)
    log.info("записано %d из %d", ok, len(rows))


def export(args) -> None:
    """Пачки по `--chunk` тайтлов в `--dir/in_<номер>.json`. Тайтлы с готовым `out_*.jsonl` пропускаются,
    номера продолжают уже занятые, поэтому завтрашний `--offset 10000` не затрёт сделанное."""
    os.makedirs(args.dir, exist_ok=True)
    done: set[str] = set()
    for name in os.listdir(args.dir):
        if name.startswith("out_") and name.endswith(".jsonl"):
            done |= load_done(os.path.join(args.dir, name))
    conn = psycopg2.connect(os.environ["DATABASE_URL"])
    with conn.cursor() as cur:
        cur.execute(CANDIDATES, (args.offset, args.limit))
        cols = [d[0] for d in cur.description]
        rows = [dict(zip(cols, r)) for r in cur.fetchall()]
    conn.close()
    rows = [r for r in rows if r["id"] not in done]
    # номера продолжают уже занятые (in_/out_), пачки полные — без дыр от готовых тайтлов
    used = [int(n[3:7]) for n in os.listdir(args.dir)
            if re.fullmatch(r"(in|out)_\d{4}\.(json|jsonl)", n)]
    base = max(used) + 1 if used else 0
    n = 0
    for i in range(0, len(rows), args.chunk):
        with open(os.path.join(args.dir, f"in_{base + n:04d}.json"), "w", encoding="utf-8") as f:
            json.dump([brief(r) for r in rows[i:i + args.chunk]], f, ensure_ascii=False)
        n += 1
    log.info("пачек: %d, тайтлов к разметке %d, уже готово %d", n, len(rows), len(done))
    return
    log.info("пачек: %d, тайтлов выбрано %d, уже готово %d", n, len(rows), len(done))


def apply(args) -> None:
    conn = psycopg2.connect(os.environ["DATABASE_URL"])
    with conn.cursor() as cur:
        cur.execute("SELECT id::text, name, kind FROM tags")
        rows = cur.fetchall()
    # Кураторские теги ищем по имени без учёта регистра и вида: «сюрреализм» (tag) и
    # «магический реализм» (genre) заведены раньше строчными, миграция их не дублирует.
    by_lower = {n.lower(): i for i, n, k in rows}
    by_genre = {n.lower(): i for i, n, k in rows if k == "genre"}
    missing = [t for t in TAGS if t.lower() not in by_lower]
    if missing:
        sys.exit(f"нет тегов в БД ({len(missing)}): применить миграцию 20261207100000. Например: {missing[:3]}")

    pairs, stats, unknown, seen = set(), {"valid": 0, "invalid": 0}, {}, set()
    allowed = set(TAGS)
    for path in args.inp:
        with open(path, encoding="utf-8") as f:
            for line in f:
                try:
                    r = normalize(json.loads(line), allowed)  # ответу модели не доверяем
                except (ValueError, KeyError, TypeError):
                    continue
                seen.add(r["id"])
                if not r.get("is_valid_title"):
                    stats["invalid"] += 1
                    continue
                stats["valid"] += 1
                for t in r["tags"]:
                    pairs.add((r["id"], by_lower[t.lower()]))
                for g in r.get("cleaned_genres") or []:
                    gid = by_genre.get(g.lower())
                    if gid:
                        pairs.add((r["id"], gid))
                    else:
                        unknown[g] = unknown.get(g, 0) + 1
    log.info("валидных %d, не кино %d, связей %d", stats["valid"], stats["invalid"], len(pairs))
    if unknown:
        top = sorted(unknown.items(), key=lambda x: -x[1])[:10]
        log.info("жанры без тега в БД (пропущены): %s", top)
    if not args.apply:
        log.info("сухой прогон, запись — --apply")
        return
    with conn, conn.cursor() as cur:
        if args.replace:
            # Перезапись разметки обработанных тайтлов: снимаются только кураторские теги
            # (жанры и прочие теги не трогаем) — так строгий прогон заменяет щедрый.
            curated = [by_lower[t.lower()] for t in TAGS]
            cur.execute("DELETE FROM entity_tags WHERE entity_id = ANY(%s::uuid[]) AND tag_id = ANY(%s::uuid[])",
                        (list(seen), curated))
            log.info("снято старых кураторских связей: %d", cur.rowcount)
        cur.execute("CREATE TEMP TABLE _pairs (entity_id uuid, tag_id uuid) ON COMMIT DROP")
        psycopg2.extras.execute_values(cur, "INSERT INTO _pairs VALUES %s", list(pairs), page_size=5000)
        cur.execute("""INSERT INTO entity_tags (entity_id, tag_id)
                       SELECT p.entity_id, p.tag_id FROM _pairs p
                       JOIN entities e ON e.id = p.entity_id
                       ON CONFLICT DO NOTHING""")
        log.info("добавлено связей: %d", cur.rowcount)
    conn.close()
    log.info("готово; обновить поиск: scripts/reindex-search.sh")


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s")
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("classify")
    c.add_argument("--out", required=True)
    c.add_argument("--limit", type=int, default=200)
    c.add_argument("--offset", type=int, default=0)
    c.add_argument("--batch", type=int, default=10)
    c.add_argument("--workers", type=int, default=4)
    c.add_argument("--model", default=MODEL)
    e = sub.add_parser("export", help="выгрузить пачки in_*.json для ручной/агентной разметки")
    e.add_argument("--dir", required=True)
    e.add_argument("--offset", type=int, default=0)
    e.add_argument("--limit", type=int, default=10000)
    e.add_argument("--chunk", type=int, default=100)
    a = sub.add_parser("apply")
    a.add_argument("--in", dest="inp", nargs="+", required=True)
    a.add_argument("--apply", action="store_true")
    a.add_argument("--replace", action="store_true",
                   help="сначала снять кураторские теги у обработанных тайтлов")
    args = ap.parse_args()
    {"classify": classify, "apply": apply, "export": export}[args.cmd](args)


if __name__ == "__main__":
    import psycopg2.extras  # noqa: F401  (execute_values)

    main()
