# Команды разработки. `make ci` повторяет проверки GitHub Actions локально.
# Тестам нужен Postgres: по умолчанию — из infra/docker-compose.yml (make up).

DATABASE_URL ?= postgres://nexus_user:nexus_password@localhost:5432/nexus_db
export DATABASE_URL

COMPOSE = docker compose -f infra/docker-compose.yml
# Контейнер с тулчейном (сервис tools) от пользователя хоста: Rust на машине не нужен.
TOOLS = $(COMPOSE) run --rm --user $(shell id -u):$(shell id -g) tools

.PHONY: up down logs run seed media fmt lint boundaries test test-fast test-one deny ci image http ci-docker test-docker shell bench-books books-llm ai openapi

up:          ## Поднять dev-окружение
	$(COMPOSE) up -d --build

down:        ## Остановить dev-окружение
	$(COMPOSE) down

logs:        ## Логи приложения
	$(COMPOSE) logs -f app

run:         ## Запустить приложение локально (без Docker)
	cargo run -p nexus

seed:        ## Загрузить тестовых пользователей (seeds/dev.sql) в dev-базу, перестроить поиск. Каталог — из TMDB (scripts/catalog-import)
	cat seeds/dev.sql | $(COMPOSE) exec -T postgres sh -c 'psql -v ON_ERROR_STOP=1 -q -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"'
	scripts/reindex-search.sh

# Ключи источников берутся из infra/.env (только эти три строки — файл не исполняется как shell).
media:       ## Постеры и трейлеры каталога: make media [MODE=check] [ARGS="--dry-run --slug dune-2021"]
	env $$(grep -sE '^(TMDB_API_KEY|TWITCH_CLIENT_ID|TWITCH_CLIENT_SECRET)=' infra/.env) \
		cargo run -q -p nexus -- media $(or $(MODE),fill) $(ARGS)

fmt:         ## Отформатировать код
	cargo fmt --all

lint:        ## Формат + clippy
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

boundaries:  ## Модули не зависят друг от друга
	scripts/check-module-boundaries.sh

# cargo-nextest и линкер mold — если установлены (быстрее прогон и линковка), иначе обычный cargo test.
# Установка: cargo install cargo-nextest --locked; mold — пакет mold (apt) или релиз с GitHub.
NEXTEST := $(shell cargo nextest --version >/dev/null 2>&1 && echo 1)
MOLD := $(shell command -v mold 2>/dev/null)
ifneq ($(MOLD),)
export RUSTFLAGS += -C link-arg=-fuse-ld=mold
endif
ifeq ($(NEXTEST),1)
CARGO_TEST = cargo nextest run --no-fail-fast
else
CARGO_TEST = cargo test --no-fail-fast
endif
# Тяжёлые наборы (индексы Tantivy, минуты): только в полном make test.
HEAVY_TESTS = catalog archive auth

openapi:     ## Контракт API documents/api/*.json — из собранного dev-бинарника, без тестов (секунды)
	docker exec -w /app nexus-app sh -c 'cargo build -q -p nexus && /target/debug/nexus openapi documents/api'

test:        ## Все тесты (нужен Postgres) — один раз перед закрытием задачи
	$(CARGO_TEST) --workspace

test-fast:   ## Быстро во время работы: unit-тесты и HTTP-тесты без тяжёлых (catalog, archive, auth)
	$(CARGO_TEST) --workspace --lib
	$(CARGO_TEST) -p nexus $(foreach t,$(filter-out $(HEAVY_TESTS),$(basename $(notdir $(wildcard app/tests/*.rs)))),--test $(t))

test-one:    ## Один набор/тест: make test-one T=studios [F=имя_теста]
	$(CARGO_TEST) -p nexus --test $(T) $(F)

deny:        ## Уязвимости и лицензии зависимостей
	cargo deny check

image:       ## Собрать production-образ
	docker build -f infra/Dockerfile -t nexus:local .

# Нужна сеть хоста, чтобы контейнер видел localhost:80 (nginx из make up).
http:        ## HTTP-проверки из http/*.http против поднятого окружения (нужен make seed; social/forum ждут своих данных)
	docker run --rm --network host -v $(CURDIR)/http:/workdir jetbrains/intellij-http-client \
		--env-file http-client.env.json --env dev health.http auth.http catalog.http social.http forum.http

ci: lint boundaries test deny  ## Всё, что проверяет CI (кроме сборки образа)

# Те же цели в Docker (сервис tools): нужен только make up, Rust ставить не надо.
ci-docker:   ## make ci в контейнере
	$(TOOLS) make ci

test-docker: ## Тесты в контейнере
	$(TOOLS) make test

shell:       ## Shell в контейнере с тулчейном (cargo, clippy, cargo deny)
	$(TOOLS) bash

bench-books: ## Нагрузка на поиск книг (Tantivy), отчёт в консоль. Нужен make up. [SECONDS=5] [THREADS=1,8,32] [LIMIT=24] [PROFILE=release — точные цифры]
	@$(COMPOSE) ps --status running --services | grep -qx app || { echo "Окружение не запущено — сначала: make up"; exit 1; }
	@$(COMPOSE) exec -e RUST_LOG=error app cargo run -q $(if $(filter release,$(PROFILE)),--release) -p nexus -- books load \
		$(if $(SECONDS),--seconds $(SECONDS)) $(if $(THREADS),--threads $(THREADS)) $(if $(LIMIT),--limit $(LIMIT)) $(ARGS)

# Секрет ключей ИИ — из infra/.env (только эта строка), иначе dev-значение как в docker-compose.
AI_ENV = env $$(grep -sE '^AI_SECRET_KEY=' $(CURDIR)/infra/.env)
NEXUS = $(COMPOSE) exec -e RUST_LOG=error app cargo run -q -p nexus --

books-llm:   ## Чистка книг ИИ одной командой: make books-llm [LIMIT=5000] [ORDER=newest] [AI=slug] [YES=1] [REINDEX=1]
	@$(COMPOSE) ps --status running --services | grep -qx app || { echo "Окружение не запущено — сначала: make up"; exit 1; }
	@cd scripts/catalog-import && $(AI_ENV) .venv/bin/python books_llm.py run \
		$(if $(LIMIT),--limit $(LIMIT)) $(if $(ORDER),--order $(ORDER)) $(if $(AI),--ai $(AI)) $(if $(YES),--yes) $(ARGS); \
	code=$$?; cd ../..; \
	if [ $$code -eq 3 ]; then exit 0; fi; [ $$code -eq 0 ] || exit $$code; \
	if [ -z "$(REINDEX)" ]; then echo "поиск не перестроен (REINDEX=1 — перестроить)"; exit 0; fi; \
	echo "== 5. книжный индекс и ручной порядок"; \
	$(NEXUS) books index && \
	$(NEXUS) books ranks knigi_10_000_progress.txt --unmatched knigi_ne_naideny.txt

ai:          ## Провайдеры ИИ: make ai CMD="list" | CMD="add deepseek --kind openai --url https://api.deepseek.com --model deepseek-chat --keys sk-.." | CMD="test deepseek"
	$(NEXUS) ai $(or $(CMD),list)
