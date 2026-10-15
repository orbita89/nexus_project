# Задача фронтенду: страницы студий

Бэкенд готов (коммит «Studios: studio pages…»). Нужно: клик по студии в карточке фильма ведёт
на страницу студии с краткой информацией и списком её фильмов.

## API (все запросы — `/api/v1/catalog`, без авторизации)

1. **Карточка фильма** `GET /entities/{slug}` — новое поле
   `studios: [{ slug, name, logo_url | null }]` (порядок как раньше в `metadata.studios`).
   Выводить студии из него (а не из `metadata.studios`) ссылками на `/studios/{slug}`; логотип —
   маленькой иконкой, если есть.

2. **Студия** `GET /studios/{slug}` →
   ```json
   {
     "slug": "legendary-pictures",
     "name": "Legendary Pictures",
     "logo_url": "https://image.tmdb.org/t/p/w300/....png",
     "origin_country": "US",
     "headquarters": "Burbank, California, United States",
     "homepage": "https://www.legendary.com",
     "description": "American film production company.",
     "parent": { "slug": "warner-bros-pictures", "name": "Warner Bros. Pictures", "logo_url": "..." },
     "entities_count": 2
   }
   ```
   Все поля, кроме `slug`, `name`, `entities_count`, могут быть `null`. 404 — студии нет.
   Первое открытие страницы студии бэкенд дополняет данными из TMDB (до ~1–2 с), дальше быстро.

3. **Фильмы студии** `GET /entities?studio={slug}&sort=&limit=&offset=` — тот же список, что
   каталог: те же карточки (`EntitySummary`), сортировки (`recommended` по умолчанию, `new`,
   `top_rated`, `trending`, `title`, …) и `{items, total, limit, offset}`.

## Страница `/studios/{slug}`

- Шапка: логотип (PNG с прозрачностью — на светлой и тёмной теме нужен фон-подложка), название,
  страна (флаг/название по ISO-коду), штаб-квартира, ссылка на сайт (`target="_blank"`,
  `rel="noopener"`), материнская компания ссылкой на её страницу, «N фильмов» из `entities_count`.
- Описание — если есть (обычно по-английски; многие студии без него — блок не показывать).
- Ниже — сетка фильмов студии с выбором сортировки и пагинацией (переиспользовать компоненты
  каталога).
- 404 → «Студия не найдена».
- Названия студий приходят латиницей (так их отдаёт TMDB: «Mosfilm», «Soyuzmultfilm») —
  показывать как есть.

## Статика

Страницы можно делать SSG/ISR: бэкенд присылает `/studios/{slug}` в вебхук пересборки
(`POST /_isr/revalidate`), когда меняются фильмы студии. Новый маршрут нужно поддержать в
обработчике ревалидации.
