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
  описание/постер, включить.

ПРОВЕРИТЬ РУКАМИ: скрытый фильм — 404 гостю и открывается в админке; включили — появился в
поиске и списке; сортировки поиска меняют порядок; sitemap без скрытых; футер с атрибуцией TMDB.
```
