ARG DEBIAN_MIRROR=http://mirrors.tuna.tsinghua.edu.cn
ARG UBUNTU_MIRROR=
ARG CARGO_REGISTRY_MIRROR=sparse+https://rsproxy.cn/index/

FROM rust:1.85-bookworm AS builder

ARG DEBIAN_MIRROR
ARG CARGO_REGISTRY_MIRROR

RUN set -eux; \
    if [ -n "${DEBIAN_MIRROR:-}" ]; then \
      find /etc/apt -type f \( -name '*.sources' -o -name 'sources.list' \) -print0 \
        | xargs -0 -r sed -i \
          -e "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}/debian|g" \
          -e "s|https://deb.debian.org/debian|${DEBIAN_MIRROR}/debian|g" \
          -e "s|http://deb.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|https://deb.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|http://security.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|https://security.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g"; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    mkdir -p /usr/local/cargo; \
    if [ -n "${CARGO_REGISTRY_MIRROR:-}" ]; then \
      printf '%s\n' \
        '[source.crates-io]' \
        'replace-with = "mirror"' \
        '' \
        '[source.mirror]' \
        "registry = \"${CARGO_REGISTRY_MIRROR}\"" \
        '' \
        '[net]' \
        'git-fetch-with-cli = true' \
        > /usr/local/cargo/config.toml; \
    else \
      printf '%s\n' \
        '[net]' \
        'git-fetch-with-cli = true' \
        > /usr/local/cargo/config.toml; \
    fi

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
COPY migrations ./migrations
COPY config ./config

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --locked --release -p media-core -p media-agent

FROM debian:bookworm-slim AS media-core-runtime

ARG DEBIAN_MIRROR

RUN set -eux; \
    if [ -n "${DEBIAN_MIRROR:-}" ]; then \
      find /etc/apt -type f \( -name '*.sources' -o -name 'sources.list' \) -print0 \
        | xargs -0 -r sed -i \
          -e "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}/debian|g" \
          -e "s|https://deb.debian.org/debian|${DEBIAN_MIRROR}/debian|g" \
          -e "s|http://deb.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|https://deb.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|http://security.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|https://security.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g"; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/media-core /usr/local/bin/media-core
COPY config ./config

CMD ["media-core"]

FROM debian:bookworm-slim AS media-agent-runtime

ARG DEBIAN_MIRROR

RUN set -eux; \
    if [ -n "${DEBIAN_MIRROR:-}" ]; then \
      find /etc/apt -type f \( -name '*.sources' -o -name 'sources.list' \) -print0 \
        | xargs -0 -r sed -i \
          -e "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}/debian|g" \
          -e "s|https://deb.debian.org/debian|${DEBIAN_MIRROR}/debian|g" \
          -e "s|http://deb.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|https://deb.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|http://security.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g" \
          -e "s|https://security.debian.org/debian-security|${DEBIAN_MIRROR}/debian-security|g"; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg iproute2 procps \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/media-agent /usr/local/bin/media-agent
COPY config ./config
COPY docker/entrypoints/media-agent-supervisor.sh /usr/local/bin/media-agent-supervisor

RUN chmod +x /usr/local/bin/media-agent-supervisor \
    && mkdir -p /data/media/work /data/media/logs

CMD ["media-agent-supervisor"]

FROM nvidia/cuda:12.6.3-runtime-ubuntu22.04 AS media-agent-gpu-runtime

ARG UBUNTU_MIRROR

RUN set -eux; \
    if [ -n "${UBUNTU_MIRROR:-}" ]; then \
      sed -i \
        -e "s|http://archive.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
        -e "s|http://security.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
        /etc/apt/sources.list; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl ffmpeg iproute2 procps \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/media-agent /usr/local/bin/media-agent
COPY config ./config
COPY docker/entrypoints/media-agent-supervisor.sh /usr/local/bin/media-agent-supervisor

RUN chmod +x /usr/local/bin/media-agent-supervisor \
    && mkdir -p /data/media/work /data/media/logs

CMD ["media-agent-supervisor"]
