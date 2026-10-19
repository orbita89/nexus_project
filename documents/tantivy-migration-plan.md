# План архитектурной подготовки и будущей миграции каталога на Tantivy

> **Статус:** внутренний регламент команды разработки Nexus · версия 1.0 · 2026-10-19
> **Владелец:** backend (модуль `catalog`) · **Пересмотр:** перед стартом Фазы 2 (≈ декабрь 2026)
> **Краткая версия для агентов:** `.docs/backend/tantivy-roadmap.md`

---

## 1. Дисклеймер и цель документа (Future Outlook)

> ⚠️ **Это стратегический план на будущее, а не задача на текущий спринт.**
> Ни один раздел ниже не требует переписывать работающий код *сейчас*. Документ задаёт
> **ограничения**, которые соблюдаются в каждом новом PR, чтобы через 1–2 месяца перенос каталога
> в Tantivy занял **один день** и прошёл **без рефакторинга схемы БД**.

**Главная цель сейчас:** писать схему и код PostgreSQL так, чтобы любая сущность каталога
(фильм, сериал, книга, игра) превращалась в документ поискового индекса **одним `SELECT` из одной
строки**, без JOIN и без «сборки» из пяти таблиц.

**Где мы сейчас (точка отсчёта, 2026-10-19):**

```
                    ┌───────────────────────────┐
  запись (админка,  │ PostgreSQL 18             │  источник правды
  импорт, архив) ──►│ entities (+ external_ids, │
                    │ metadata jsonb), people,  │
                    │ entity_tags, entity_credits│ ← M2M (наследие, заморожено)
                    └──────┬─────────────┬──────┘
                           │ publish::run│ nexus archive build
                           ▼             ▼
                ┌────────────────┐  ┌──────────────────────┐
  списки/поиск ◄│ Meilisearch    │  │ Tantivy (холодный    │► поиск по архиву
                │ entities,people│  │ архив, ~1,07 млн)    │
                └────────────────┘  └──────────────────────┘
```

Tantivy в проекте уже работает: холодный архив фильмов (`catalog::archive`, `ARCHIVE_DIR`).
Фаза 2 расширяет этот опыт на весь каталог.

**Что уже сделано в духе этого регламента:**
- 2026-10-19 удалены таблицы под одного поставщика (`entity_tmdb`, `archived_tmdb`,
  `entity_tmdb_data`, `person_tmdb_data`). Внешние id теперь в `entities.external_ids jsonb`, а
  список архива ведёт сам индекс Tantivy.
- Данные по типам лежат в `entities.metadata jsonb`, а не в таблицах `movies`/`books`.

### Фазы

| Фаза | Срок | Суть |
|---|---|---|
| **1. Подготовка** (сейчас) | окт–ноя 2026 | Правило Flat First для всего нового; денормализованные копии списков в строке `entities`; служебные поля |
| **2. Переход** | ≈ дек 2026, 1 день | Индексы Tantivy строятся из `entities` одним проходом; чтение каталога переключается флагом |
| **3. Эксплуатация** | после | Ночной batch sync + точечные обновления при публикации; Meilisearch для каталога выключен |

---

## 2. Правило «Flat First» для БД

### 2.1. Запрет M2M-таблиц для данных каталога

**Запрещено** заводить новые промежуточные таблицы для *описательных* списков сущности каталога:
`film_genres`, `film_actors`, `book_authors`, `entity_countries`, `entity_keywords` и т. п.

**Почему:** документ поискового индекса плоский. Каждая M2M-таблица — это JOIN при каждой
синхронизации, отдельный триггер `updated_at` и источник рассинхронизации. Ровно так в прошлом
появились `entity_tmdb`/`*_tmdb_data`, которые пришлось удалять.

```
  ❌ Так не делаем                         ✅ Так делаем
  ┌──────────┐   ┌────────────┐            ┌─────────────────────────────────┐
  │ entities │──<│film_genres │>──┐        │ entities                        │
  └──────────┘   └────────────┘   │        │  genres     text[]  {drama,war} │
       │         ┌────────────┐ ┌─┴────┐   │  countries  text[]  {US,GB}     │
       └────────<│film_actors │ │genres│   │  credits    jsonb   [{...}]     │
                 └────────────┘ └──────┘   │  updated_at timestamptz         │
                                           └─────────────────────────────────┘
       документ = JOIN × 4                      документ = SELECT одной строки
```

**Чем M2M отличается от допустимой связи:**

| Допустимо (отдельная таблица) | Запрещено (только в строке) |
|---|---|
| Данные **пользователей**: `reviews`, `entity_ratings`, `collection_items`, `user_interests`, `entity_marks` | Жанры, страны, студии, ключевые слова |
| Связь с **самостоятельной сущностью**, у которой своя страница и свои поля: `people` (персона) | Список авторов/актёров *как он показан в карточке* |
| Служебные счётчики с частой записью: `entity_stats`, `person_credit_counts` | Любые id внешних сервисов (→ `external_ids`) |

**Наследие:** `entity_tags`, `entity_credits`, `entity_relations` остаются. Их читают админка,
страницы людей и фильтры, и миграция ради миграции противоречит цели «без рефакторинга».
Правило для них в Фазе 1 — **dual-write**: при каждой записи в эти таблицы в той же транзакции
обновляется денормализованная копия в строке `entities` (п. 2.3). В Фазе 2 индекс читает только
копию.

### 2.2. Типы для списков

| Данные | Тип PostgreSQL | Пример | В Tantivy |
|---|---|---|---|
| Простой список кодов/слагов | `text[]` | `genres = '{drama,war}'` | `STRING` (multi-value), фасет |
| Список объектов | `jsonb` (массив) | `credits = '[{"slug":"...","name":"...","role":"actor"}]'` | развернуть: `person_slugs` STRING + `people_names` TEXT |
| Поля конкретного типа | `jsonb` `metadata` | `{"runtime_min":155,"studios":[...]}` | поле на ключ |
| Id внешних сервисов | `jsonb` `external_ids` | `{"tmdb":550}` | `tmdb_id` u64 INDEXED |

```sql
-- ✅ Новое поле-список — колонка в entities, индекс GIN для фильтров в PG до Фазы 2.
ALTER TABLE entities ADD COLUMN genres text[] NOT NULL DEFAULT '{}';
CREATE INDEX entities_genres_gin ON entities USING gin (genres);

-- Фильтр «драма» до Фазы 2 (в PG — только админка; публичные списки — Meilisearch):
SELECT slug FROM entities WHERE genres @> '{drama}';

-- ❌ НЕ создаём:
-- CREATE TABLE entity_genres (entity_id uuid REFERENCES entities, genre text, PRIMARY KEY (...));
```

**Ограничения записи** (проверяет `validate.rs`, а не БД): элементов `text[]` — до 50, строка —
до 120 символов, `jsonb`-список — до 200 объектов. Хотя бы в одном из полей слаг ссылки
(`person_slug`, `studio_slug`) обязателен: индекс не должен ходить в PG за адресом страницы.

### 2.3. Обязательные служебные поля

Каждая таблица, строки которой станут документами индекса (`entities`, `people`), обязана иметь:

| Поле | Тип | Назначение для синхронизации |
|---|---|---|
| `id` | `uuid` (`uuidv7()`) | Неизменяемый ключ документа: по нему `delete_term` |
| `updated_at` | `timestamptz NOT NULL` | Курсор инкрементальной загрузки (триггер `set_updated_at()`) |
| `is_archived` | `boolean NOT NULL DEFAULT false` | В какой индекс идёт документ: активный или архивный |
| `deleted_at` | `timestamptz` (NULL — жива) | Надгробие: ночная загрузка должна *увидеть* удаление |

> **Решение по `id`: `uuid`, а не `BIGINT`.** Изначально предлагался `id BIGINT`, но у нас
> PK — `uuid` со значением по умолчанию `uuidv7()` (возрастает со временем, как BIGINT), и на него
> ссылаются FK всех модулей (`reviews`, `forum_thread_entities`, …). Замена PK на BIGINT — это
> переписывание схемы, ровно то, чего документ требует избежать. В Tantivy `id` хранится как
> `STRING`-терм (36 байт): для `delete_term` этого достаточно. Требование к `id` — только
> **неизменяемость**. Публичный id в URL остаётся `slug` (CLAUDE.md §6).

```sql
-- Фаза 1: служебные поля (новая миграция; применённые файлы не трогаем).
ALTER TABLE entities ADD COLUMN is_archived boolean     NOT NULL DEFAULT false;
ALTER TABLE entities ADD COLUMN deleted_at  timestamptz;

-- Курсор ночной загрузки: «что поменялось после T» без полного прохода по 1 млн строк.
CREATE INDEX entities_updated_at_idx ON entities (updated_at, id);

-- updated_at уже ведёт триггер (миграция init_schema):
--   CREATE TRIGGER entities_set_updated_at BEFORE UPDATE ON entities
--       FOR EACH ROW EXECUTE FUNCTION set_updated_at();
```

**Правило «изменил копию — изменился `updated_at`»:** dual-write из п. 2.1 (теги, титры)
обязан делать `UPDATE entities SET genres = …`. Триггер тогда сдвинет `updated_at`, и ночная
загрузка увидит изменение. Изменение, после которого `updated_at` строки не сдвинулся, **не попадёт
в индекс** — это главный источник будущих багов.

**Удаление:** `DELETE FROM entities` стирает строку бесследно, и инкрементальная загрузка его не
увидит. Поэтому в Фазе 1 вводится мягкое удаление `deleted_at = now()`. Физическое удаление (кроме
архива, п. 4) делает отдельная уборка после того, как ночная синхронизация обработала надгробие.

**Что не меняет `updated_at` и это нормально:** `entity_stats` (popularity, рейтинги IMDb,
`rank_score`) живёт отдельной узкой таблицей. Её массовое обновление не должно перестраивать GIN
`entities`. В индекс такие числа попадают отдельным быстрым проходом (п. 5, шаг 4).

---

## 3. Разделение зон ответственности (после перехода)

```
 ┌──────────────────────────── PostgreSQL (Primary DB) ────────────────────────────┐
 │  Источник правды. ACID. Все записи идут только сюда.                            │
 │                                                                                 │
 │  catalog: entities (плоские), people, studios, entity_stats                     │
 │  auth:    users, refresh_tokens, oauth_accounts                                 │
 │  social:  reviews, review_comments, review_votes, entity_ratings, collections,  │
 │           forum_threads, forum_posts, follows, user_interests, entity_marks     │
 │           ── ссылаются на каталог по entity_id (uuid) ──                        │
 └────────────────────────────────┬────────────────────────────────────────────────┘
                                  │ batch sync (ночью) + publish::run (сразу)
                                  ▼  только каталог, только чтение
 ┌──────────────────────────── Tantivy (Search Core) ──────────────────────────────┐
 │  Денормализованный read-only кэш каталога. Можно удалить и пересобрать из PG.   │
 │  полнотекстовый поиск · фасеты (жанр, год, страна, студия, роль) · сортировки   │
 │  списков · карточка фильма для чтения без PG (stored-поля документа)            │
 └─────────────────────────────────────────────────────────────────────────────────┘
```

| Вопрос | PostgreSQL | Tantivy |
|---|---|---|
| Кто пишет | API, админка, импорт | только конвейер синхронизации |
| Можно ли потерять | нет (бэкапы) | да: пересборка из PG |
| Транзакции | да | нет (только атомарный `commit` сегментов) |
| Что лежит | всё | плоский каталог + агрегаты (`rating_avg`, `reviews_count`) |
| Читает API | данные пользователей, админка, запись | публичные списки, поиск, карточка |

**Запрещено передавать в Tantivy динамические данные:** ветки и сообщения форума, комментарии,
рецензии целиком, отметки и коллекции пользователей, ленту. Их меняют каждую секунду, у них
права доступа и модерация. Индекс с коммитом раз в ночь их бы отставал, а частые коммиты
раздувают число сегментов.

**Допустимы агрегаты:** число рецензий, средняя оценка Nexus. Их уже сейчас раз в 60 с
пересылает в Meilisearch задача `sync_ratings`. После перехода она так же пишет поле
документа по `id`.

**Связь с текущим правилом «чтение через Meilisearch»:** до Фазы 2 публичные списки, поиск и
(по решению 2026-10-19) отображение фильмов идут через Meilisearch. Карточка должна
показываться, даже если PG недоступен. Tantivy в Фазе 2 берёт на себя ровно эту роль. Поэтому
всё, что сейчас кладём в документ Meilisearch, — тот же плоский набор полей, который потом
ляжет в Tantivy. Одна функция «строка `entities` → документ» обслуживает оба движка.

---

## 4. Будущая архитектура раздельных индексов (Index Partitioning)

### 4.1. Структура на диске

```
/var/lib/nexus/tantivy/                 (TANTIVY_DIR; сейчас ARCHIVE_DIR = …/archive)
├── movies/                ← активные фильмы (горячий слой, ~120 тыс.)
│   ├── meta.json
│   ├── .tantivy-writer.lock
│   └── <segment>.{idx,pos,term,store,fast,fieldnorm}
├── movies_archive/        ← холодный архив фильмов (~1,07 млн, сегодняшний archive/)
├── series/
├── series_archive/
├── books/                 ← книги LibGen
├── books_archive/
└── people/                ← персоны (страницы людей, поиск людей)
```

Имя индекса = `<kind>` или `<kind>_archive`, где `kind` — значение `entity_kind`. В какой индекс
попадает документ, определяет `entities.is_archived`.

### 4.2. Почему раздельные индексы

| Довод | Пояснение |
|---|---|
| **Изоляция ночных обновлений** | У Tantivy один писатель на каталог (`LockBusy` у второго). Ночная перестройка `movies` не блокирует запись в `books`, а промоушен из архива не ждёт сборки книг |
| **Разный ритм** | `movies` меняется каждый день (popularity, новинки), `*_archive` — раз в месяцы. Раздельно: маленький индекс коммитится часто, большой почти не трогается и остаётся в page cache |
| **Экономия диска** | Схема архива — «скелет» (название, дата, постер, id): ~5× меньше документа горячего слоя. В одном индексе пришлось бы держать полную схему для миллиона строк |
| **Производительность** | Горячий индекс в 10 раз меньше: TopK и фасеты считаются по 120 тыс. документов, а не по 1,2 млн. Слияние сегментов не затрагивает архив |
| **Схема своя у каждого типа** | У книги `isbn`, `pages`, у фильма `runtime_min`. Поля одного типа не раздувают другие |
| **Простой откат** | Сломался `books` — его пересобирают из PG за минуты, фильмы продолжают работать |

### 4.3. Сквозной поиск (активные + архив)

В Tantivy нет готового `MultiSearcher` (как в Lucene). Сквозной поиск собираем сами: запрос
параллельно уходит в каждый индекс, результаты сливаются по общему score.

```
              GET /search?q=гамлет&scope=all
                         │
          ┌──────────────┼──────────────────┐
          ▼              ▼                  ▼
   searcher(movies)  searcher(movies_archive)  searcher(books)
   TopDocs(k=off+lim) TopDocs(k=off+lim)       TopDocs(k=off+lim)
          │              │                  │
          └──────► merge по score (k-way, heap) ◄──┘
                         │  tie-break: active > archive, затем popularity
                         ▼
                 страница [offset .. offset+limit]
                 + total = Σ Count каждого индекса
```

```rust
/// Сквозной поиск: каждый индекс отдаёт свои `offset + limit` лучших, слияние по score.
/// BM25 сопоставим между индексами одной схемы полей title/orig_title. У архива
/// `ARCHIVE_PENALTY` < 1: при равной релевантности выше фильм с сайта.
pub fn multi_search(parts: &[(&CatalogIndex, f32)], q: &str, limit: usize, offset: usize)
    -> tantivy::Result<(Vec<Hit>, usize)>
{
    let k = offset + limit;
    let mut hits = Vec::with_capacity(k * parts.len());
    let mut total = 0;
    for (index, weight) in parts {                    // в проде — rayon / spawn_blocking
        let searcher = index.reader.searcher();
        let query = index.parse(q);
        let (top, count) = searcher.search(&query, &(TopDocs::with_limit(k), Count))?;
        total += count;
        hits.extend(top.into_iter().map(|(score, addr)| Hit::new(index, score * weight, addr)));
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));  // k·n элементов: сортировка дешёвая
    Ok((hits.into_iter().skip(offset).take(limit).collect(), total))
}
```

Глубокие страницы (`offset` > 1000) сквозной поиск не отдаёт, как сегодня `archive::MAX_OFFSET`:
стоимость растёт как `n × (offset + limit)`.

---

## 5. План-сценарий ночной синхронизации (Batch Sync)

**Расписание:** 03:30 по Москве, после `«Обновить рейтинги»`. Один процесс `nexus index sync`
(CLI, как `nexus archive build`). Курсор хранится в каталоге индекса (`meta.json` → `payload`), а
не в PG: индекс сам знает, до какого момента он актуален.

```
 03:30  ┌─ 1. lock: writer(movies) ─────────────── занят → выход, повтор через 10 мин
        │
        ├─ 2. since = payload.synced_until − 5 мин (запас на долгие транзакции)
        │     until = now()
        │
        ├─ 3. SELECT … FROM entities                (курсор, пачки по 1000)
        │     WHERE updated_at > since AND updated_at <= until
        │     ORDER BY updated_at, id
        │       ├─ deleted_at IS NOT NULL → delete_term(id) во всех индексах kind
        │       ├─ is_archived            → delete_term(id) в movies,
        │       │                            upsert в movies_archive
        │       └─ иначе                  → delete_term(id) + add_document в movies
        │
        ├─ 4. числа из entity_stats (popularity, rank_score, рейтинги) —
        │     обновить документы, где stats.updated_at > since
        │
        ├─ 5. ЕДИНЫЙ commit() с payload.synced_until = until
        │     (до commit поиск видит старое состояние целиком; после — новое целиком)
        │
        └─ 6. reload читателей, метрики: upserted / deleted / moved / секунды
```

**Почему `since − 5 мин`:** транзакция, начатая в 03:29:59 и закоммиченная в 03:30:05, получит
`updated_at` = 03:29:59 (время начала), то есть *до* `until` прошлого прогона. Перекрытие
окна ловит её. Повторная обработка документа безопасна: upsert идемпотентен.

```sql
-- Шаг 3: инкремент за окно. Индекс (updated_at, id) — без полного прохода.
SELECT e.id, e.slug, e.kind, e.title, e.original_title, e.description, e.release_date,
       e.cover_url, e.metadata, e.external_ids, e.genres, e.credits,
       e.is_archived, e.deleted_at, e.updated_at
FROM entities e
WHERE e.updated_at >  $1          -- since
  AND e.updated_at <= $2          -- until
ORDER BY e.updated_at, e.id
LIMIT 1000;                        -- следующая пачка: (updated_at, id) > (последние)
```

```rust
/// Ночной инкремент: удаление по id → добавление → один коммит на весь прогон.
pub async fn nightly_sync(db: &PgPool, idx: &CatalogIndexes) -> anyhow::Result<SyncReport> {
    let mut writer = idx.movies.writer()?;              // LockBusy → Err, cron повторит
    let since = idx.movies.synced_until()? - Duration::minutes(5);
    let until = Utc::now();
    let mut report = SyncReport::default();

    let mut rows = sqlx::query_as::<_, CatalogRow>(SYNC_SQL)
        .bind(since).bind(until)
        .fetch(db);                                    // курсор: память не растёт
    while let Some(row) = rows.try_next().await? {
        let id = Term::from_field_text(idx.fields.id, &row.id.to_string());
        writer.delete_term(id.clone());                // 1. delete_term(id) — всегда
        idx.movies_archive.delete_term(&id);           //    (документ мог сменить индекс)
        match (row.deleted_at, row.is_archived) {
            (Some(_), _) => report.deleted += 1,       //    надгробие: только удаление
            (None, true) => idx.movies_archive.add(row.to_skeleton())?,
            (None, false) => {
                writer.add_document(row.to_document(&idx.fields))?;   // 2. add_document
                report.upserted += 1;
            }
        }
    }
    let mut prepared = writer.prepare_commit()?;       // 3. ЕДИНЫЙ commit()
    prepared.set_payload(&until.to_rfc3339());         //    курсор — вместе с данными
    prepared.commit()?;
    idx.movies_archive.commit()?;                      //    (свой писатель у архива)
    idx.reload_all()?;
    Ok(report)
}
```

**Правила синхронизации:**
1. `delete_term(id)` **всегда** перед `add_document`. В Tantivy нет update, документ без удаления
   задублируется.
2. Один `commit()` на прогон, не на пачку. Каждый commit — новый сегмент и fsync. Исключение —
   первичная сборка: commit каждые 100 000 документов (как `COMMIT_EVERY` в `pipeline.rs`).
3. Точечные изменения из админки не ждут ночи: `publish::run` делает тот же upsert одного
   документа с коммитом. Ночной прогон — страховка от пропусков.
4. Полная пересборка (`--fresh`) — новый каталог `movies.v<ts>/`, затем атомарная замена ссылки.
   Так же сегодня устроена перестройка Meilisearch (`entities_v<ms>` + swap).
5. Упал посреди прогона — коммита не было: индекс остался в состоянии прошлой ночи, курсор
   не сдвинулся, следующий запуск повторит окно.

---

## 6. Чек-лист готовности к переходу (перед стартом Фазы 2)

Фаза 2 начинается, когда **все** пункты отмечены. Каждый проверяется запросом или командой.

- [ ] **1. Ни одной новой M2M-таблицы для данных каталога** с даты этого регламента.
      `make boundaries` (расширить скрипт) и ревью: новые `CREATE TABLE entity_*` в `migrations/`
      — только пользовательские данные.
- [ ] **2. Документ собирается из одной строки.** Функция `CatalogRow → Document` берёт только
      колонки `entities` (`genres`, `credits`, `metadata`, `external_ids`) без JOIN. Тест:
      документ из строки совпадает с документом Meilisearch для 100 случайных фильмов.
- [ ] **3. Dual-write покрыт тестами.** Любая запись в `entity_tags`/`entity_credits` меняет
      копию в `entities` и сдвигает `updated_at`. Контрольный запрос возвращает 0:
      ```sql
      SELECT count(*) FROM entities e
      WHERE e.genres IS DISTINCT FROM (
          SELECT coalesce(array_agg(t.slug ORDER BY t.slug), '{}') FROM entity_tags et
          JOIN tags t ON t.id = et.tag_id WHERE et.entity_id = e.id AND t.kind = 'genre');
      ```
- [ ] **4. Удаления видны.** `DELETE FROM entities` в коде остался только у уборки надгробий и
      переноса в архив. Остальное — `deleted_at`. `grep -rn "DELETE FROM entities" modules`.
- [ ] **5. Служебные поля на месте:** `is_archived`, `deleted_at`, индекс
      `(updated_at, id)` у `entities` и `people`; `updated_at` сдвигается у 100% изменённых строк
      (тест на каждый путь записи: админка, импорт, архив).
- [ ] **6. Прототип ночной синхронизации на dev:** `nexus index sync` над копией `movies`
      укладывается в 5 минут на суточный инкремент и в 15 минут на полную пересборку
      горячего слоя (~120 тыс.). Сквозной поиск movies + movies_archive — p95 < 50 мс.
- [ ] **7. Откат за минуту:** переключатель чтения каталога (`CATALOG_READ=meili|tantivy`).
      Meilisearch не удаляется, пока Tantivy не проработает в проде 2 недели.

---

## Приложение A. Ревью-чек для каждого PR в модуль `catalog`

```
[ ] Новое поле сущности — колонка/ключ в entities, не новая таблица
[ ] Список — text[] или jsonb в строке; в элементах есть slug для ссылок
[ ] Изменение данных каталога сдвигает entities.updated_at
[ ] Нет жёсткого DELETE (кроме архива/уборки надгробий)
[ ] В документ поиска не попадают форум, комментарии, тексты рецензий
[ ] Внешний сервис — ключ в external_ids, а не таблица <vendor>_*
```

## Приложение B. Связанные документы

- `.docs/backend/tantivy-roadmap.md` — краткая версия для агентов (LOD 2).
- `.docs/backend/catalog.md` → `<tables>` (текущая схема), «Холодный архив» (Tantivy сегодня).
- `migrations/20261027100000_external_ids.sql` — удаление таблиц под поставщика.
