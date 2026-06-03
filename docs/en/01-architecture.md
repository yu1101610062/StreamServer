# 01. Architecture

StreamServer is split into a Core control plane and one or more Agent nodes.

```text
Web Console / Desktop Client / External API
          |
          | HTTP API
          v
    media-core
          |
          | Bidirectional gRPC stream
          v
    media-agent
          |
          v
 FFmpeg / ZLMediaKit / Local Media Runtime
```

## media-core

`media-core` is the only northbound service. It owns:

- HTTP API and web UI.
- Task creation, validation, and state transitions.
- Scheduling and dispatch.
- Idempotency and operation requests.
- Attempt and lease records.
- Node registry and live load.
- Task events, logs, progress, audit, and callbacks.
- ZLMediaKit hook ingestion.
- PostgreSQL migrations and repositories.

## media-agent

`media-agent` runs on media nodes. It owns:

- Registration and heartbeat.
- Capability probing for FFmpeg, ffprobe, ZLM, and GPU devices.
- FFmpeg execution plans and process supervision.
- ZLM proxy, RTP server, and recording control.
- Runtime registry, adoption, recovery, logs, progress, and artifacts.
- Upload and local media file handling.

## Control Plane Stream

The Agent opens a long-lived bidirectional gRPC stream. Agent-to-Core messages include registration, heartbeat, capability snapshots, task events, logs, progress, and runtime snapshots. Core-to-Agent commands include start task, stop task, capability probe, orphan adoption, and recording control.

Task messages carry `task_id`, `attempt_no`, and `lease_token`. Core uses those fields to reject stale Agent events after retries, reclaim, or new dispatches.
