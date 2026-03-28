FROM rust:1.85-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
COPY migrations ./migrations
COPY config ./config

RUN cargo build --locked --release -p media-core -p media-agent

FROM debian:bookworm-slim AS media-core-runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/media-core /usr/local/bin/media-core
COPY config ./config

CMD ["media-core"]

FROM debian:bookworm-slim AS media-agent-runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg iproute2 procps \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/media-agent /usr/local/bin/media-agent
COPY config ./config
COPY docker/entrypoints/media-agent-supervisor.sh /usr/local/bin/media-agent-supervisor

RUN chmod +x /usr/local/bin/media-agent-supervisor \
    && mkdir -p /data/media/work /data/media/logs

CMD ["media-agent-supervisor"]
