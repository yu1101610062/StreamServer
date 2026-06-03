# 00. Overview

StreamServer is a native Rust media control-plane and edge-agent system for orchestrating FFmpeg and ZLMediaKit workloads.

It is not a single media server implementation. It is a control plane plus worker-agent system:

- `media-core` exposes the API and web console, owns task state, scheduling, idempotency, persistence, audit, and control-plane coordination.
- `media-agent` runs on media nodes, connects to Core through a bidirectional gRPC stream, probes local capabilities, and manages FFmpeg/ZLM runtimes.
- PostgreSQL is the source of truth for tasks, nodes, events, audits, recordings, hook events, and callback delivery.
- FFmpeg handles complex media processing.
- ZLMediaKit handles realtime media serving, proxying, distribution, recording, APIs, and hooks.

## Why Native Runtime

Target environments may be offline, restricted, or unsuitable for Docker runtime deployment. Media workloads also need direct interaction with host networking, process management, GPU devices, multicast interfaces, and local media directories.

StreamServer therefore uses Docker only during the build phase. The runtime artifact is a Linux AMD64 offline bundle installed through systemd.

## Current Scope

Implemented areas include:

- Core API and web console.
- Agent runtime and local process management.
- Bidirectional Core-Agent gRPC stream.
- PostgreSQL persistence and migrations.
- Task state machine, attempt tracking, and lease fencing.
- FFmpeg execution planning.
- ZLMediaKit integration.
- Native Linux AMD64 packaging.
- Automated Rust and frontend tests.

Areas still being hardened include GPU scheduling closure, production observability, upgrade/rollback, and broader FFmpeg smoke coverage.
