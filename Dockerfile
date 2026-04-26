ARG DEBIAN_MIRROR=
ARG UBUNTU_MIRROR=
ARG CARGO_REGISTRY_MIRROR=
ARG NPM_REGISTRY_MIRROR=
ARG FRONTEND_BUILDER_IMAGE=node:22-bookworm
ARG RUST_BUILDER_IMAGE=rust:1.85-bookworm
ARG MEDIA_CORE_RUNTIME_BASE_IMAGE=debian:bookworm-slim
ARG MEDIA_AGENT_RUNTIME_BASE_IMAGE=jrottenberg/ffmpeg:7.1-ubuntu2404
ARG MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE=jrottenberg/ffmpeg:7.1-nvidia2204

FROM ${FRONTEND_BUILDER_IMAGE} AS frontend-builder

ARG NPM_REGISTRY_MIRROR

WORKDIR /app/crates/media-core/frontend

COPY crates/media-core/frontend/package.json ./
COPY crates/media-core/frontend/package-lock.json ./

RUN if [ -n "${NPM_REGISTRY_MIRROR:-}" ]; then npm config set registry "${NPM_REGISTRY_MIRROR}"; fi \
    && npm ci

COPY crates/media-core/frontend ./

RUN npm run build

FROM ${RUST_BUILDER_IMAGE} AS rust-builder-base

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

FROM rust-builder-base AS media-core-builder

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --locked --release -p media-core

FROM rust-builder-base AS media-agent-builder

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --locked --release -p media-agent

FROM rust-builder-base AS streamserver-config-builder

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    cargo build --locked --release -p streamserver-config

FROM scratch AS media-core-bin-export

COPY --from=media-core-builder /app/target/release/media-core /media-core

FROM scratch AS media-agent-bin-export

COPY --from=media-agent-builder /app/target/release/media-agent /media-agent

FROM scratch AS streamserver-config-bin-export

COPY --from=streamserver-config-builder /app/target/release/streamserver-config /streamserver-config

FROM scratch AS media-ui-export

COPY --from=frontend-builder /app/crates/media-core/ui /ui

FROM scratch AS media-bin-export

COPY --from=media-core-bin-export /media-core /media-core
COPY --from=media-agent-bin-export /media-agent /media-agent
COPY --from=streamserver-config-bin-export /streamserver-config /streamserver-config

FROM scratch AS media-host-assets-export

COPY --from=media-core-bin-export /media-core /media-core
COPY --from=media-agent-bin-export /media-agent /media-agent
COPY --from=streamserver-config-bin-export /streamserver-config /streamserver-config
COPY --from=media-ui-export /ui /ui

FROM ${MEDIA_CORE_RUNTIME_BASE_IMAGE} AS media-core-runtime

ARG DEBIAN_MIRROR
ENV STREAMSERVER_UI_DIR=/opt/streamserver/ui \
    STREAMSERVER_BINARY_NAME=media-core \
    STREAMSERVER_BINARY_PATH=/opt/streamserver/bin/media-core

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

COPY packaging/docker/exec-streamserver-binary.sh /usr/local/bin/exec-streamserver-binary
COPY config ./config

RUN chmod +x /usr/local/bin/exec-streamserver-binary \
    && mkdir -p /opt/streamserver/bin /opt/streamserver/ui

ENTRYPOINT ["/usr/local/bin/exec-streamserver-binary"]
CMD []

FROM ${MEDIA_AGENT_RUNTIME_BASE_IMAGE} AS media-agent-runtime

ARG UBUNTU_MIRROR
ENV STREAMSERVER_BINARY_NAME=media-agent \
    STREAMSERVER_BINARY_PATH=/opt/streamserver/bin/media-agent

RUN set -eux; \
    if [ -n "${UBUNTU_MIRROR:-}" ]; then \
      find /etc/apt -type f \( -name '*.sources' -o -name 'sources.list' \) -print0 \
        | xargs -0 -r sed -i \
          -e "s|http://archive.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
          -e "s|https://archive.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
          -e "s|http://security.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
          -e "s|https://security.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g"; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY packaging/docker/exec-streamserver-binary.sh /usr/local/bin/exec-streamserver-binary
COPY config ./config

RUN chmod +x /usr/local/bin/exec-streamserver-binary \
    && mkdir -p /opt/streamserver/bin /data/media/work /data/media/logs

ENTRYPOINT ["/usr/local/bin/exec-streamserver-binary"]
CMD []

FROM ${MEDIA_AGENT_GPU_RUNTIME_BASE_IMAGE} AS media-agent-gpu-runtime

ARG UBUNTU_MIRROR
ENV STREAMSERVER_BINARY_NAME=media-agent \
    STREAMSERVER_BINARY_PATH=/opt/streamserver/bin/media-agent

RUN set -eux; \
    if [ -n "${UBUNTU_MIRROR:-}" ]; then \
      find /etc/apt -type f \( -name '*.sources' -o -name 'sources.list' \) -print0 \
        | xargs -0 -r sed -i \
          -e "s|http://archive.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
          -e "s|https://archive.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
          -e "s|http://security.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g" \
          -e "s|https://security.ubuntu.com/ubuntu|${UBUNTU_MIRROR}/ubuntu|g"; \
    fi; \
    apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY packaging/docker/exec-streamserver-binary.sh /usr/local/bin/exec-streamserver-binary
COPY config ./config

RUN chmod +x /usr/local/bin/exec-streamserver-binary \
    && mkdir -p /opt/streamserver/bin /data/media/work /data/media/logs

ENTRYPOINT ["/usr/local/bin/exec-streamserver-binary"]
CMD []
