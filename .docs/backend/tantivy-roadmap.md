# Tantivy roadmap — LOD 2 (план на будущее, не задача спринта)

Полная версия для людей: `documents/tantivy-migration-plan.md` (только для записи; агенту контекст — этот файл).

<rules>
Фаза 1 (сейчас, окт–ноя 2026) — ограничения на КАЖДЫЙ PR в `catalog`:
- Flat First: НЕ создавать M2M-таблицы для описательных данных каталога (`film_genres`, `book_authors`, `entity_countries`, ...). Списки — `text[]` или `jsonb` в строке `entities`; в элементах — slug для ссылок. Лимиты — в `validate.rs`.
- Отдельная таблица допустима только для: данных пользователей (`reviews`, `entity_ratings`, ...), самостоятельных сущностей со своей страницей (`people`, `studios`), часто пишущихся счётчиков (`entity_stats`, `person_credit_counts`).
- Внешний сервис — ключ в `entities.external_ids`, никогда не таблица `<vendor>_*`.
- Наследие `entity_tags`, `entity_credits`, `entity_relations` не мигрируем; при появлении копий (`genres text[]`, `credits jsonb`) — dual-write в одной транзакции.
- Любое изменение данных каталога обязано сдвигать `entities.updated_at` (триггер `set_updated_at()` — только через `UPDATE entities`).
- PK остаётся `uuid` (`uuidv7()`), BIGINT не вводим: на `entities.id` ссылаются FK всех модулей. В Tantivy id — STRING-терм.
- Запланированные служебные поля (ещё НЕ созданы): `entities.is_archived boolean`, `entities.deleted_at timestamptz` (мягкое удаление — надгробие для инкремента), индекс `(updated_at, id)`. Добавлять новой миграцией при старте работ над sync.
</rules>

<target>
Фаза 2 (≈ дек 2026, 1 день): PostgreSQL — источник правды (всё, включая social/auth); Tantivy — read-only денормализованный каталог (поиск, фасеты, списки, карточка без PG), заменяет Meilisearch для каталога (переключатель `CATALOG_READ=meili|tantivy`, Meili не удаляется 2 недели).
- В Tantivy НЕ попадают: форум, комментарии, тексты рецензий, коллекции, лента. Допустимы агрегаты (`rating_avg`, `reviews_count`).
- Индексы раздельно: `/var/lib/nexus/tantivy/{movies,movies_archive,series,series_archive,books,books_archive,people}`; индекс = `<entity_kind>[_archive]` по `is_archived`. Сегодняшний `ARCHIVE_DIR` = будущий `movies_archive`.
- Сквозной поиск: в Tantivy нет MultiSearcher — параллельный `TopDocs(offset+limit)` по индексам, k-way merge по score, архив с весом < 1, `total` = сумма `Count`, `offset` ≤ 1000.
- Ночной sync (`nexus index sync`, 03:30): курсор в payload коммита (`synced_until`), окно `(since − 5 мин, now]` по `updated_at`; для каждой строки `delete_term(id)` во всех индексах kind → `add_document` (или ничего, если `deleted_at`); числа `entity_stats` — отдельным проходом; ОДИН `commit()` на прогон; полная пересборка — в `<name>.v<ts>/` + замена.
</target>

<checklist>
Готовность к Фазе 2: (1) нет новых M2M каталога; (2) документ = одна строка `entities` без JOIN (совпадает с Meili-документом); (3) dual-write покрыт тестами, контрольный запрос расхождений = 0; (4) нет жёстких `DELETE FROM entities` кроме архива/уборки; (5) `is_archived`, `deleted_at`, индекс `(updated_at, id)` есть, `updated_at` сдвигается на всех путях записи; (6) прототип sync: инкремент < 5 мин, полная горячего слоя < 15 мин, сквозной поиск p95 < 50 мс; (7) откат переключателем за минуту.
</checklist>
