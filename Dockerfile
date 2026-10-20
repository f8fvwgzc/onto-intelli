# syntax=docker/dockerfile:1
# One binary + PostgreSQL. Build and run it with the database:
#   ONTO_TOKEN=$(openssl rand -hex 24) docker compose --profile server up --build
# or on its own, against any PostgreSQL 16+ with pgvector:
#   docker build -t onto-intelli .
#   docker run -p 50051:50051 -e ONTO_DATABASE_URL=postgres://… -e ONTO_TOKEN -v onto-config:/data onto-intelli

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p onto-intelli-cli \
 && cp target/release/onto /usr/local/bin/onto

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /data onto
COPY --from=build /usr/local/bin/onto /usr/local/bin/onto
USER onto
WORKDIR /data
# The volume holds only configuration (and your protos and schema files);
# all data lives in PostgreSQL.
ENV ONTO_CONFIG=/data/onto-intelli.toml
EXPOSE 50051
# First start writes a starter config into the volume; edit it and restart.
ENTRYPOINT ["/bin/sh", "-c", "[ -f \"$ONTO_CONFIG\" ] || onto init /data >/dev/null; exec onto serve --listen 0.0.0.0:50051"]
