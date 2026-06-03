# ADR-0005: Use PostgreSQL as the Source of Truth

## Status

Accepted

## Context

Task dispatch, idempotency, leases, events, audits, callbacks, nodes, recordings, and stream metadata need consistent transactional behavior. Runtime events may arrive concurrently from API calls, scheduler loops, Agent streams, and ZLM hooks.

## Decision

PostgreSQL is the source of truth for StreamServer control-plane state.

The project uses SQLx migrations and repository-layer transactions for task creation, dispatch, state transitions, events, callback outbox, node records, recording catalogs, and audit records.

## Consequences

Pros:

- Transactional consistency for dispatch and state transitions.
- Row locks and unique constraints can enforce idempotency and fencing.
- Event and audit history remain queryable.

Cons:

- Core requires PostgreSQL availability.
- Tests and local development need a reachable database for repository/integration coverage.
- Native packaging must either bundle PostgreSQL runtime or support an external PostgreSQL role.
