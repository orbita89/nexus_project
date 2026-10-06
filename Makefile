# Команды разработки. `make ci` повторяет проверки GitHub Actions локально.
# Тестам нужен Postgres: по умолчанию — из infra/docker-compose.yml (make up).

DATABASE_URL ?= postgres://nexus_user:nexus_password@localhost:5432/nexus_db
export DATABASE_URL
# Тесты поиска ходят в Meilisearch из make up.
MEILI_URL ?= http://localhost:7700
MEILI_MASTER_KEY ?= nexus_dev_master_key
export MEILI_URL MEILI_MASTER_KEY

COMPOSE = docker compose -f infra/docker-compose.yml
# Контейнер с тулчейном (сервис tools) от пользователя хоста: Rust на машине не нужен.
TOOLS = $(COMPOSE) run --rm --user $(shell id -u):$(shell id -g) tools

.PHONY: up down logs run seed fmt lint boundaries test deny ci image http ci-docker test-docker shell

up:          ## Поднять dev-окружение
	$(COMPOSE) up -d --build

down:        ## Остановить dev-окружение
	$(COMPOSE) down

logs:        ## Логи приложения
	$(COMPOSE) logs -f app

run:         ## Запустить приложение локально (без Docker)
	cargo run -p nexus

seed:        ## Загрузить тестовых пользователей, каталог и social (seeds/*.sql) в dev-базу, перестроить поиск
	cat seeds/dev.sql seeds/catalog.sql seeds/social.sql | $(COMPOSE) exec -T postgres sh -c 'psql -v ON_ERROR_STOP=1 -q -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"'
	scripts/reindex-search.sh

fmt:         ## Отформатировать код
	cargo fmt --all

lint:        ## Формат + clippy
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

boundaries:  ## Модули не зависят друг от друга
	scripts/check-module-boundaries.sh

test:        ## Все тесты (нужен Postgres)
	cargo test --workspace

deny:        ## Уязвимости и лицензии зависимостей
	cargo deny check

image:       ## Собрать production-образ
	docker build -f infra/Dockerfile -t nexus:local .

# Нужна сеть хоста, чтобы контейнер видел localhost:80 (nginx из make up).
http:        ## HTTP-проверки из http/*.http против поднятого окружения (нужен make seed)
	docker run --rm --network host -v $(CURDIR)/http:/workdir jetbrains/intellij-http-client \
		--env-file http-client.env.json --env dev health.http auth.http catalog.http social.http

ci: lint boundaries test deny  ## Всё, что проверяет CI (кроме сборки образа)

# Те же цели в Docker (сервис tools): нужен только make up, Rust ставить не надо.
ci-docker:   ## make ci в контейнере
	$(TOOLS) make ci

test-docker: ## Тесты в контейнере
	$(TOOLS) make test

shell:       ## Shell в контейнере с тулчейном (cargo, clippy, cargo deny)
	$(TOOLS) bash
