# ADR-0001: Use Native Runtime Instead of Docker Runtime

## Status

Accepted

## Context

Target environments may be offline, restricted, or unable to run Docker. Media workloads also need direct access to host networking, process management, GPU devices, multicast routes, and local media directories.

## Decision

StreamServer uses Docker only during the build phase. Runtime deployment is a native Linux AMD64 offline bundle installed through systemd.

## Consequences

Pros:

- No Docker runtime dependency on target hosts.
- Better fit for edge nodes, offline environments, host networking, and multicast.
- Easier integration with systemd and native process supervision.

Cons:

- Packaging complexity increases.
- Runtime dependency verification becomes more important.
- Upgrade and rollback workflows must be implemented explicitly.
