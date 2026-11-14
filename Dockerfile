# syntax=docker/dockerfile:1.7

FROM rust:1.95-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Cache the dependency build separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src tests \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > tests/placeholder.rs \
    && cargo build --release --locked \
    && rm -rf src tests target/release/deps/serum_proxy*

# Real build.
COPY src ./src
COPY tests ./tests
RUN touch src/main.rs && cargo build --release --locked

# Dev toolchain: rustfmt + clippy (omitted by the slim image) and pkg-config
# baked in, so fmt/lint work and no apt runs per call.
FROM rust:1.95-slim-bookworm AS toolchain
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy
WORKDIR /app

FROM rust:1.95-slim-bookworm AS dev
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENV RUST_LOG=debug
ENV SERUM_ADMIN_BIND=0.0.0.0:8088
CMD ["cargo", "run", "--", "--config", "/app/config.yaml"]

FROM debian:bookworm-slim AS runtime
LABEL org.opencontainers.image.source="https://github.com/serum-sh/serum-proxy" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --user-group serum
USER serum
WORKDIR /app
COPY --from=builder /build/target/release/serum-proxy /usr/local/bin/serum-proxy

ENV SERUM_ADMIN_BIND=0.0.0.0:8088
EXPOSE 8088
ENTRYPOINT ["/usr/local/bin/serum-proxy"]
CMD ["--config", "/app/config.yaml"]
