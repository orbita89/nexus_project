# Промт для фронтенда: карточка v2 на настоящем API

Бэкенд сделал всё из `nexus_web/docs/backend-prompt.md` (коммиты `a0a7d06..83e2cfb` в `main`).
Ниже — промт для новой сессии Claude Code, открытой в `/home/dev/nexus_web`: снять заглушки и
`localStorage`, подключить новые эндпоинты.

```
Проект Nexus, фронтенд: SvelteKit 3 (Svelte 5), TypeScript, Tailwind + DaisyUI, клиент API —
openapi-fetch с типами из pnpm api:types. Бэкенд (/home/dev/nexus_project) доделал API для
карточки v2 и старые долги из docs/backend-questions.md. Код бэкенда НЕ меняй: если чего-то не
хватает — допиши в docs/backend-questions.md. Контракт — nexus_project/documents/api/*.json,
описание — documents/modules/{catalog,social,auth}.md, коды ошибок — таблица в
documents/for-other-modules.md («Авторизация в хендлере»).

Перед работой прочитай: docs/architecture.md, docs/design.md, docs/backend-questions.md,
docs/backend-prompt.md (что просили и что фронтенд обещал включить — таблица в конце),
src/lib/catalog/card.ts, src/lib/social/*.ts, src/lib/components/card/*, src/lib/api/errors.ts.

Порядок работы: сначала pnpm api:types (бэкенд поднят: make up в nexus_project), затем пункты
ниже по одному, после каждого — pnpm verify зелёный и коммит. Не пушить.

ОБЩИЕ ПРАВИЛА:
- Публичное (рецензии, сводка оценок, комментарии, похожие, оценки пачкой) грузится БЕЗ токена,
  как сейчас (browserApi / publicApi), — оно одинаково для всех и кэшируется. Личное («мой голос»,
  «моя реакция», «моя отметка») — отдельными запросами с токеном, только у вошедшего. Не
  встраивать личное в публичные запросы.
- Карточка — статика (пререндер): то, что в EntityDetail (related, tags.kind, metadata.studios/
  awards/trivia), рисуется в HTML; живые блоки — через liveQuery и CardLive (card-live.svelte.ts).
- Оптимистичные обновления голосов, реакций и отметок с откатом при ошибке; сообщения ошибок —
  через errorMessage().
- localStorage-хранилища (review-votes, marks, Reactions) удалить вместе с ключами: старые
  значения не переносим.
- Дизайн не менять без нужды: заглушки заменяются данными в тех же местах и стилях.

=== 1. Коды ошибок (src/lib/api/errors.ts) ===
Тело ошибки теперь {"error", "code", "retry_after"?}. ApiError получает поле code; translate()
выбирает сообщение по code (словарь), сопоставление текста оставить запасным путём. Коды:
validation, unauthorized, invalid_credentials, forbidden, email_not_verified, own_review,
not_found, conflict, username_taken, email_taken, not_released, review_without_text,
self_relation, password_incorrect, invalid_token, username_change_too_soon, rate_limited,
unavailable, internal. Тексты — из текущего словаря KNOWN, недостающие придумай в том же тоне
(not_released — «Оценки откроются после премьеры», review_without_text — «Комментировать можно
только рецензию с текстом», own_review — «За свою рецензию голосовать нельзя»). Тесты в
errors.spec.ts.

=== 2. Год в EntityRef ===
EntityRef.release_date: в плашках сущностей тем форума, рецензий профиля, коллекций и ленты —
«Дюна (2021)» через yearOf().

=== 3. Спойлеры ===
- Форма рецензии (MyReview / ReviewComposer): галочка «Есть спойлеры» → PutReview.is_spoiler;
  при редактировании — из своей рецензии.
- isSpoiler() в reviews.ts — по полю контракта (review.is_spoiler), без проверки 'in'.

=== 4. До премьеры ===
isReleased() на карточке остаётся (кнопки неактивны), плюс ответ 409 code=not_released — понятное
сообщение (пункт 1). Касается оценки, рецензии и реакций.

=== 5. Голоса за рецензии (src/lib/social/review-votes.svelte.ts) ===
- PUT /api/v1/social/reviews/{id}/vote {value: 1|-1} → {likes_count, dislikes_count, value};
  DELETE того же пути → то же с value: null. Повторное нажатие того же голоса — DELETE.
- Свои голоса — GET /api/v1/social/entities/{slug}/votes/mine → {"<review_id>": 1|-1}: ОДИН
  запрос на карточку (общий store на страницу, не на каждую рецензию), только для вошедшего,
  перечитывать при входе/выходе.
- Счётчики — Review.likes_count / dislikes_count (теперь в контракте всегда); после голоса
  показывать числа из ответа PUT/DELETE. reviewCount() заменить чтением полей.
- За свою рецензию кнопки неактивны (author.id === session.user.id); 403 own_review — на всякий
  случай через errorMessage.

=== 6. Комментарии к рецензиям (ReviewComments.svelte, REVIEW_COMMENTS_API = true) ===
- GET /api/v1/social/reviews/{id}/comments?limit&offset → Page<ReviewComment {id, review_id,
  author: UserRef, body, created_at, updated_at}>, старые сверху, без токена. «Показать ещё» по
  offset. Счётчик на кнопке — Review.comments_count.
- POST тот же путь {body} (1–2000 символов, проверка и на клиенте) → 201 ReviewComment, добавить
  в конец. Гостю — приглашение войти.
- DELETE /api/v1/social/review-comments/{id} — своя кнопка «Удалить»; admin — DELETE
  /api/v1/social/admin/review-comments/{id} (как ModerateButton для рецензий).
- У оценки без текста (body === null) блока комментариев нет (бэкенд ответит 409).
- Реалтайм: события review.commented и review.comment_deleted в канале сущности (data:
  review_id, comment_id, entity_id, author_id) — перечитать комментарии этой рецензии и
  comments_count (через CardLive или точечно). Протокол — nexus_project/documents/modules/realtime.md.

=== 7. Реакции (src/lib/components/card/Reactions.svelte) ===
- Числа — RatingSummary.reactions {fire, sad, think, bored} из GET …/entities/{slug}/rating
  (уже грузится для оценок — переиспользуй тот же запрос).
- Своя — GET /api/v1/social/entities/{slug}/reaction → {kind} или 404 (нет реакции); PUT {kind}
  → 200; DELETE → 204. Одна на зрителя: другая заменяет, повторная снимает. Счётчики
  обновлять оптимистично (±1 и перенос между видами).
- До выхода — неактивно (isReleased), 409 not_released — сообщение.

=== 8. Отметки «Посмотреть позже» / «Просмотрено» (marks.svelte.ts, MarkButtons.svelte) ===
- GET /api/v1/social/entities/{slug}/mark → {kind: later|done} или 404; PUT {kind}; DELETE → 204.
  Взаимоисключающие, повторная — снять. markLabels() по типу оставить.
- Список для будущего кабинета: GET /api/v1/social/users/me/marks?kind=later|done&limit&offset
  → Page<{entity: EntityRef, kind, created_at}>. Сделать страницы /settings… или /me/marks НЕ
  нужно, если их нет в docs/design.md — только функцию в marks.svelte.ts и тест; если в design.md
  кабинет описан — сделай вкладки «Позже» / «Просмотрено» сеткой EntityCard.

=== 9. Связанные (Related.svelte) — одной каруселью ===
Приквелы, сиквелы и ремейки — НЕ отдельными строками-группами, а ОДНОЙ горизонтальной каруселью
«Связанные» (как полоса «Похожие»: прокрутка, на мобильном — свайп).
- API: EntityDetail.related — ОДИН массив для карусели: [{...EntitySummary, relation:
  'prequel' | 'sequel' | 'remake'}], уже хронологически (release_date, без даты — в конце);
  пусто — []. Склеивать и сортировать на фронтенде не нужно.
- relatedGroups() в card.ts заменить на relatedWorks(entity) → entity.related как есть (или
  с подписью). Тесты в card.spec.ts.
- У карточки в карусели — подпись типа связи: «Приквел», «Сиквел», «Ремейк» (у ремейка,
  который старше текущей, — «Оригинал»), бейджем DaisyUI над или под постером.
- Пусто — раздела нет. Проверить на dune-2021 (в одной карусели: «Дюна» 1984 — «Оригинал»,
  «Часть вторая» — «Сиквел») и lotr-fellowship-2001 («Хоббит» 2012 — «Приквел», «Две крепости» —
  «Сиквел»).
Админка карточки: блок «Связанные»: список текущих связей с удалением
(DELETE /api/v1/catalog/admin/entities/{id}/relations/{to_id}?kind=sequel|prequel|remake) и
добавление (PUT /api/v1/catalog/admin/entities/{id}/relations {kind, to_id} — выбор сущности
через существующий EntityPicker). 409 self_relation / conflict — сообщения.

=== 10. Похожие (Recommendations.svelte, RECOMMENDATIONS_API = true) ===
GET /api/v1/catalog/entities/{slug}/similar?limit=12 → Page<EntitySummary>, без токена, в
браузере (liveQuery). Полоса постеров EntityCard; пусто (total 0) — блок не показывать.

=== 11. Жанры отдельно от тегов ===
Tag.kind: genre | tag. В шапке карточки (Hero) — жанры, над «О фильме» — остальные теги
(TagChips по kind). Бэкенд уже отдаёт жанры первыми, но фильтровать по kind.
- /tags: две группы «Жанры» и «Теги» (GET /catalog/tags?kind=genre / ?kind=tag или один список
  с разбиением).
- /tags/[slug]: название из GET /api/v1/catalog/tags/{slug} → {id, slug, name, kind,
  entities_count} вместо всего списка; 404 — страница ошибки.
- Админка тегов: поле «Жанр / тег» (kind) при создании и правке.

=== 12. Производство, награды, факты (Production.svelte, форма сущности в админке) ===
metadata.studios: string[] (movie, series), awards: [{title, year: number|null, won: bool}] и
trivia: string[] (все типы). Показ: «Производство» — студии (у игр developer/publisher, у книг
publisher — как сейчас); «Награды» — победы и номинации раздельно или с пометкой, год; «Факты» —
список. Нет данных — блока нет (заглушки убрать). Пример данных — dune-2021 (make seed).
Форма админки: редактирование списков; лимиты: studios до 20 × 1–120, awards до 50 (title
1–200, year 1880–2100), trivia до 30 × 1–1000; ошибки бэкенда — code validation.

=== 13. Долги ===
a) Админка пользователей: GET /api/v1/auth/admin/users/page?q=&limit=&offset= →
   Page<UserView> с total — поиск по username/email (поле с debounce), пагинация по total.
   Обход типов (as never) убрать.
b) Сортировки каталога: GET /catalog/entities?sort=new|popular|rating|title — переключатель
   в разделах каталога (SortTabs), sort в адресе (?sort=, new — по умолчанию, в адрес не
   писать). На главной — «Популярное» и «Высокий рейтинг» полосами, если это есть в design.md.
c) Оценки в сетках: GET /api/v1/social/ratings?slugs=a,b,c (до 100) → {"slug": {average,
   count}}, без токена, один запрос на страницу сетки; RatingBadge на EntityCard. На SSR и в
   браузере; ошибка запроса — сетка без оценок, не ошибка страницы.
d) «В коллекцию» (AddToCollection.svelte): GET /api/v1/social/users/{username}/collections?
   contains={slug} → у каждой коллекции contains: bool — одним запросом вместо N.
e) Превью коллекций: Collection.preview — первые 4 EntityRef: обложки в CollectionCard
   (коллаж из постеров, пустая — заглушка).
f) Страница тега — пункт 11.

В конце: docs/backend-questions.md — всё сделанное перенести в «Решено» (со ссылкой на
коммиты бэкенда a0a7d06..83e2cfb), что осталось или нашлось — открытыми пунктами;
docs/architecture.md обновить; pnpm verify зелёный. Итог — список того, что подключено, и что
проверено руками на make seed (dune-2021: голоса, 2 комментария, реакции, ремейк и сиквел,
студии/награды/факты, похожие; user: отметки «позже»).
```

## Что проверить руками после

| Где | Что должно быть (данные `make seed`) |
|---|---|
| `/films/dune-2021` | жанры в шапке, теги над «О фильме»; одна карусель «Связанные»: «Дюна» 1984 (оригинал), «Часть вторая» (сиквел); студии, 7 «Оскаров», 2 факта; похожие — «Прибытие», «Интерстеллар»…; у рецензии `user` 2 лайка, 1 дизлайк, 2 комментария; реакции 🔥2 🤔1 |
| `/films/dune-part-two-2024` | рецензия `author` размыта (спойлер) |
| вход `user` | «Дюна» 2021 — «Просмотрено»; «В коллекцию» — галочки одним запросом |
| `/films`, `?sort=rating` | порядок по оценкам, оценки на постерах |
| `/admin/users` | поиск по имени и email, `total` |
