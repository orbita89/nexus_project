# Промт для фронтенда: активные и скрытые фильмы, сортировки по TMDB

Бэкенд загрузил каталог ~1,2 млн фильмов (TMDB + Wikidata) и ввёл флаг «активен»: на сайте —
~80 тыс., остальные хранятся в БД и включаются в админке. Ниже — промт для новой сессии Claude Code,
открытой в `/home/dev/nexus_web`.

```
Проект Nexus, фронтенд: SvelteKit 3 (Svelte 5), TypeScript, Tailwind + DaisyUI, клиент API —
openapi-fetch с типами из pnpm api:types. Бэкенд (/home/dev/nexus_project) добавил флаг
«активен» у сущностей каталога и сортировки поиска по данным TMDB. Код бэкенда НЕ меняй: если
чего-то не хватает — допиши в docs/backend-questions.md. Контракт — nexus_project/documents/api/
catalog.json, описание — documents/modules/catalog.md (разделы «Активные и скрытые», «Поиск»,
«Эндпоинты», «Каталог фильмов из TMDB + Wikidata»).

Порядок: pnpm api:types (бэкенд поднят: make up в nexus_project), затем пункты по одному, после
каждого — pnpm verify зелёный и коммит. Не пушить.

ЧТО ИЗМЕНИЛОСЬ В API:
- Entity / EntityDetail: поле active (bool). Скрытая (active=false) на сайте не видна нигде:
  GET /catalog/entities, /search, /similar, related в карточке, фильмографии людей, entities_count
  тегов — только активные. GET /catalog/entities/{slug} скрытой — 404 для всех, кроме админа
  (с токеном admin — 200 и скрытые related). Человек, у которого все работы скрыты, — 404.
- metadata фильма: popularity (число, TMDB, «смотрят сейчас») и vote_count (целое, TMDB,
  «известность за всё время»).
- GET /catalog/search?sort=popularity|votes — сортировка по этим полям (без sort — релевантность).
- Админка: GET /catalog/admin/entities — все сущности, включая скрытые, фильтры как у /entities
  плюс active=true|false, у каждой поле active. POST/PATCH /admin/entities принимают active.
  PATCH active=false: бэкенд удаляет документ из поиска и шлёт ISR — сервер фронтенда получает
  404 и удаляет статику (уже реализовано в server/isr.js); active=true — страница соберётся.

=== 1. Статика и SEO ===
- entries() пререндера листает GET /catalog/entities — теперь это ~80 тыс. активных. Пререндерить
  заранее только верх: например первые N (PRERENDER_ENTITIES=N, по умолчанию 2000) из
  GET /catalog/search?sort=votes, остальные активные рисуются при первом заходе (ISR, как сейчас).
- sitemap.xml: только активные (GET /catalog/entities постранично), sitemap index по 50 000 URL.
- Страница 404 для скрытых — обычная 404 SvelteKit (без «фильм скрыт»).
- Атрибуция TMDB в футере: логотип TMDB и «This product uses the TMDB API but is not endorsed or
  certified by TMDB» (условие бесплатного использования API; постеры и данные — из TMDB).

=== 2. Поиск ===
- Переключатель сортировки: «По релевантности» (по умолчанию), «Популярные сейчас»
  (sort=popularity), «Самые известные» (sort=votes). В URL — ?sort=…

=== 3. Админка ===
- Список сущностей админки — GET /admin/entities (вместо публичного), фильтр
  «Все / На сайте / Скрытые» (active), бейдж «Скрыт» у неактивных, поиск q.
- В форме сущности — переключатель «Показывать на сайте» (PATCH active). Карточку скрытой админ
  открывает с токеном (GET /entities/{slug} с Authorization).
- Включение — главный сценарий «наполнения контентом»: найти фильм в «Скрытых», дописать
  описание/постер, включить. Переключённые вручную помечены active_manual (в списке админки) —
  показать значок «вручную».
- Раздел «Данные TMDB»: две кнопки, ход — тем же логом шагов, что у перестройки поиска (поток
  text/event-stream: plan, step, progress, done; читать через fetch, EventSource не умеет POST):
  · «Обновить popularity» — POST /admin/tmdb/popularity/stream (дамп TMDB, ~15–25 мин), итог
    done.popularity {export, films, updated};
  · «Пересобрать активные» — POST /admin/tmdb/active/stream?by_votes=40000&by_popularity=40000
    (поля ввода для двух чисел, подтверждение перед запуском), итог done.active
    {activated, deactivated, active}. Ручные переключения не трогаются.
  409 — уже идёт перестройка поиска или другая такая операция: показать сообщение.

ПРОВЕРИТЬ РУКАМИ: скрытый фильм — 404 гостю и открывается в админке; включили — появился в
поиске и списке; сортировки поиска меняют порядок; sitemap без скрытых; футер с атрибуцией TMDB.
```

## Дополнение: честная сортировка (2026-10-13)

```
Бэкенд ввёл статус выхода, балл «Рекомендуемые» и сортировки. Контракт — documents/api/catalog.json,
описание — documents/modules/catalog.md («Сортировка и активные»).

- Главная и каталог по умолчанию — sort=recommended (балл: рейтинг IMDb + популярность + свежесть,
  настраивается в админке). Переключатель: «Рекомендуемые», «Смотрят сейчас» (trending), «Высокий
  рейтинг» (top_rated), «Новинки» (new — только вышедшие), «Скоро» (soon). Поиск — те же
  recommended/trending/top_rated.
- release_status в элементах и карточке: soon — бейдж «Скоро премьера» и дата выхода; отдельный
  блок «Скоро премьера» — GET /catalog/entities?sort=soon. Невышедшие фильмы из TMDB (кроме «скоро»)
  бэкенд не отдаёт (404).
- imdb_rating / imdb_votes в карточке и списках: «IMDb 8.0» (атрибуция IMDb рядом с TMDB в футере;
  данные IMDb — только некоммерческое использование).
- Удалить сортировки popularity/votes поиска и metadata.popularity/vote_count из форм (их больше нет).
- Админка «Сортировка»: форма настроек (GET /admin/ranking) — веса рейтинга/популярности/свежести,
  «доверительные голоса», полураспад свежести (лет), минимум голосов, окно «скоро» (дней);
  кнопка «Предпросмотр» — POST /admin/ranking/preview (таблица топ-50: название, год, IMDb, голоса,
  popularity, вклад q/p/r, балл, активен); «Сохранить» — PUT /admin/ranking/stream (лог шагов).
- Админка «Данные»: «Обновить рейтинги» — POST /admin/tmdb/stats/stream; «Пересобрать активные» —
  POST /admin/tmdb/active/stream?count=80000 (одно поле «сколько фильмов на сайте»). В списке
  админки — поле visible («на сайте») вдобавок к active.
```
