# ADR-0003: Fence Task State with Attempt and Lease

## Status

Accepted

## Context

Media tasks can be retried, reclaimed, stopped, restarted, or adopted after process/Core/Agent failures. Old Agent messages may arrive after a newer task attempt has already started.

## Decision

Task execution messages carry `task_id`, `attempt_no`, and `lease_token`.

Core stores task attempts and leases in PostgreSQL and validates Agent events against the active attempt and lease before applying state changes.

## Consequences

Pros:

- Stale Agent messages cannot overwrite current task state.
- Retries and reclaim can be reasoned about explicitly.
- The model is testable at both domain and repository layers.

Cons:

- Every runtime event must carry fencing metadata.
- Repository transitions must consistently validate attempt and lease fields.
