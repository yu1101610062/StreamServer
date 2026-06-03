# ADR-0002: Use a Core-Agent Control Plane

## Status

Accepted

## Context

Media tasks run on nodes that own local FFmpeg, ZLMediaKit, disks, network interfaces, and optional GPU devices. A single API service cannot directly assume that all runtime resources are local.

## Decision

StreamServer separates `media-core` and `media-agent`.

- Core owns APIs, scheduling, state machines, persistence, idempotency, audit, callbacks, and web UI.
- Agent owns node registration, capability probing, FFmpeg/ZLM runtime execution, progress, logs, local artifacts, and recovery.
- Core and Agent communicate through a bidirectional gRPC stream.

## Consequences

Pros:

- Clear boundary between control plane and node-local execution.
- Supports multi-node and offline edge-node deployment.
- Enables runtime capability reporting and targeted dispatch.

Cons:

- Requires stream lifecycle, heartbeat, reconnect, and stale-message handling.
- Dispatch and recovery logic are more complex than single-process execution.
