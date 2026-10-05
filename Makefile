# Команды разработки. `make ci` повторяет проверки GitHub Actions локально.
# Тестам нужен Postgres: по умолчанию — из infra/docker-compose.yml (make up).

DATABASE_URL ?= postgres://nexus_user:nexus_password@localhost:5432/nexus_db
export DATABASE_URL

COMPOSE = docker compose -f infra/docker-compose.yml

.PHONY: up down logs run fmt lint boundaries test deny ci image http

up:          ## Поднять dev-окружение
	$(COMPOSE) up -d --build

down:        ## Остановить dev-окружение
	$(COMPOSE) down

logs:        ## Логи приложения
	$(COMPOSE) logs -f app

run:         ## Запустить приложение локально (без Docker)
	cargo run -p nexus

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
http:        ## HTTP smoke-проверки из http/*.http против поднятого окружения
	docker run --rm --network host -v $(CURDIR)/http:/workdir jetbrains/intellij-http-client \
		--env-file http-client.env.json --env dev health.http

ci: lint boundaries test deny  ## Всё, что проверяет CI (кроме сборки образа)
