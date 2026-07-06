# bsl-indexer в контейнере: демон-писатель + read-only MCP HTTP serve.
#
# Контекст сборки — корень репозитория (см. deploy/docker/docker-compose.yml,
# поле build.context). Исходники копируются из контекста, .dockerignore рядом с
# ним отсекает target/ и .git, чтобы контекст не раздувался.
#
#   docker build -f deploy/docker/Dockerfile -t bsl-indexer:local .

# ---- build ----
# slim-база вдвое легче полного rust:bookworm (важно на медленной сети);
# gcc нужен для bundled rusqlite/zstd (компиляция C). reqwest — rustls (без openssl).
FROM rust:1-slim-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# bsl-indexer без feature enrichment (LLM-обогащение не нужно для индекса/serve).
RUN cargo build --release -p bsl-indexer

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/bsl-indexer /usr/local/bin/bsl-indexer
COPY deploy/docker/entrypoint.sh /usr/local/bin/bsl-indexer-entrypoint.sh
ENV CODE_INDEX_HOME=/data/code-index-home
WORKDIR /data
ENTRYPOINT ["bash", "/usr/local/bin/bsl-indexer-entrypoint.sh"]
