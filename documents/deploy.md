# Окружения и деплой

**Статус:** ✅ dev в Docker, CI публикует образ в GHCR, конфигурация staging/прода готова и
проверена локально. 🕓 сервера ещё нет: подключение — по чек-листу внизу.

## Окружения

| | dev | staging | прод |
|---|---|---|---|
| Где | ваша машина | сервер (VPS) | сервер (VPS) |
| Compose | `infra/docker-compose.yml` | `infra/docker-compose.prod.yml` | `infra/docker-compose.prod.yml` |
| Приложение | исходники + `cargo watch` (hot-reload) | готовый образ из GHCR | тот же образ, что прошёл staging |
| Настройки | дефолты в compose | `infra/.env.prod` на сервере | `infra/.env.prod` на сервере |
| Письма | Mailpit (http://localhost:8025) | настоящий SMTP | настоящий SMTP |
| Dev login | включён | выключен всегда | выключен всегда |
| Swagger `/docs` | да | да (`API_DOCS=true`) | по желанию |
| Данные | `make seed` | свои, без продовых персональных данных | боевые, бэкапы |

Принцип: **один образ, разные настройки.** Код не знает, где он запущен: всё различие — в
переменных окружения (`Config::from_env`). Миграции приложение применяет само при старте.

## Dev: всё в Docker

`make up` поднимает Postgres, Redis, Meilisearch, Mailpit, nginx и приложение с hot-reload.
Для проверок Rust на машине не обязателен — есть сервис `tools` с тем же тулчейном:

| Команда | Что делает |
|---|---|
| `make ci-docker` | `make ci` в контейнере: fmt, clippy, границы модулей, тесты, cargo deny |
| `make test-docker` | только тесты |
| `make shell` | bash в контейнере: `cargo ...`, `cargo deny ...` |

`tools` запускается от вашего пользователя, поэтому файлы, которые он меняет (`cargo fmt`,
`UPDATE_OPENAPI=1 ...`), остаются вашими. У него свои тома для сборки и кеша крейтов, так что
он не мешает `cargo watch` в `app`, но и не делит с ним кеш: первая сборка — несколько минут.
На машине с ~6 ГБ RAM не стоит гонять тесты в `tools` во время тяжёлой пересборки `app`.

Если Rust стоит локально, `make ci` работает и напрямую. Сборка приложения в Docker идёт в
том `/target` (не в папку проекта), поэтому локальный `target/` — только ваш.

## Образ

`infra/Dockerfile`: сборка в `rust:1.98`, запуск в `debian:bookworm-slim` без компилятора, от
пользователя `nexus`, ~90 МБ. `make image` собирает его локально (`nexus:local`).

CI (`.github/workflows/ci.yml`, job `docker`): в PR образ только собирается; после зелёных
lint, test и deny в `main` он публикуется в GitHub Container Registry:

```
ghcr.io/orbita89/nexus_project:<sha коммита>   # это и деплоим
ghcr.io/orbita89/nexus_project:main            # последний из main
```

Пакет в GHCR после первой публикации приватный: сервер логинится токеном
(`docker login ghcr.io`, токен с правом `read:packages`) или пакет делается публичным в
настройках репозитория.

## Staging и прод

`infra/docker-compose.prod.yml`: Postgres, Redis, Meilisearch (`MEILI_ENV=production`),
приложение из образа и nginx — единственный открытый наружу порт. Все секреты обязательны
(`${VAR:?...}`): без заполненного `infra/.env.prod` compose не стартует.

```bash
cp infra/.env.prod.example infra/.env.prod     # заполнить, не коммитить
docker compose -f infra/docker-compose.prod.yml --env-file infra/.env.prod up -d
```

`NEXUS_ENV` (`staging` / `production`) — имя проекта compose: тома и контейнеры двух окружений
не пересекаются, даже если они на одной машине.

**Обновление:** поменять `NEXUS_IMAGE` на новый sha и
`docker compose ... pull app && docker compose ... up -d app`. Откат — вернуть прошлый sha.

Ограничение отката: если новый образ применил миграцию, старый при старте упадёт — sqlx не
запускается, когда в БД есть миграция, которой нет в бинарнике. До первого прода решить: либо
откатываться только вперёд (исправление новым коммитом), либо запускать миграции с
`ignore_missing` (схема при этом должна оставаться совместимой со старым кодом — миграции только
добавляют, как и требует правило проекта).

### Проверка локально (без сервера)

Собрать образ и поднять прод-конфигурацию рядом с dev, на другом порту:

```bash
make image
cat > /tmp/nexus-local.env <<'ENV'
NEXUS_ENV=localprod
NEXUS_IMAGE=nexus:local
HTTP_PORT=8088
POSTGRES_PASSWORD=local-check
MEILI_MASTER_KEY=local-check-meili-key
JWT_SECRET=local-check-jwt-secret-at-least-32-chars
SMTP_URL=smtp://localhost:1025
MAIL_FROM=Nexus <no-reply@nexus.local>
APP_BASE_URL=http://localhost:8088
PUBLIC_URL=http://localhost:8088
API_DOCS=true
ENV
docker compose -f infra/docker-compose.prod.yml --env-file /tmp/nexus-local.env up -d
curl http://localhost:8088/health
docker compose -f infra/docker-compose.prod.yml --env-file /tmp/nexus-local.env down -v
```

## HTTPS 🕓

Нужны сервер и домен. Варианты: certbot рядом с nginx (сертификат в том, `listen 443 ssl`), или
домен за Cloudflare (TLS на их стороне). Решить при подключении сервера.

## Чек-лист: когда появится сервер

1. VPS (2 vCPU, 4 ГБ RAM хватит на старт), Docker и compose plugin, пользователь для деплоя,
   SSH по ключу, firewall: открыты 22, 80, 443.
2. Домены: `staging.<домен>` и `<домен>` → IP сервера (или два сервера).
3. На сервере: клон репозитория (нужны только `infra/`), `infra/.env.prod` со своими секретами,
   `docker login ghcr.io`.
4. HTTPS (см. выше).
5. OAuth: отдельные приложения у провайдеров с redirect_uri этого окружения.
6. Бэкапы Postgres: ежедневный `pg_dump` в хранилище вне сервера + проверка восстановления.
7. Деплой из CI: job после `docker`, по SSH выполняет обновление (см. выше). Staging —
   автоматически после main, прод — вручную (GitHub Environments с подтверждением), тем же sha.
8. После деплоя на staging — `http/*.http` с окружением `staging` в `http/http-client.env.json`
   (dev login там выключен, проверки, которые на него опираются, нужно будет разделить).
