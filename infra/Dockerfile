# Production-образ: сборка в полном тулчейне, запуск в slim-образе без компилятора.
# Собирается из корня репозитория: docker build -f infra/Dockerfile .

# cargo-chef отделяет сборку зависимостей от сборки нашего кода: пока Cargo.lock не менялся,
# слой с зависимостями берётся из кеша и пересобирается только workspace.
FROM rust:1.99-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked -p nexus

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home nexus
COPY --from=builder /app/target/release/nexus /usr/local/bin/nexus
USER nexus
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
CMD ["nexus"]
