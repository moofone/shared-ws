# Sprint 3: Remove Rate Limiting From shared-ws (Rate Limiter Agnostic)

Source spec: `spec/DELEGATED_REPLY_WS.md`

## Context (Spec Excerpts For This Sprint)

- `shared-ws` must not embed outbound rate limiting policy.
- Existing internal limiter modules are legacy and should be removed:
  - `shared-ws/src/core/rate_limit.rs`
  - `shared-ws/src/core/global_rate_limit.rs`
- External composition is the intended model:
  - callers/coordinators run `shared-rate_limiter` immediately before `ws.ask(WsDelegatedRequest { .. })`
  - `shared-ws` only does IO + correlation + timeouts

## Goal

Make `shared-ws` **rate limiter agnostic** by deleting its internal rate limiter implementations and exports, without leaving compatibility bridges or duplicated “retry-after scheduling” logic that assumes a limiter exists inside the crate.

## Scope

### In

- Remove legacy rate limiter modules + all public exports of those types.
- Remove rate-limiter-specific fields and logic from `WebSocketActorArgs` / `WebSocketActor`:
  - `global_rate_limit`, `global_permits`
  - `rate_limiter`
  - `acquire_outbound_permit()` and rate-limit-driven `schedule_outbound_drain()` behavior
- Preserve bounded memory/backpressure and send semantics:
  - keep `outbound_capacity` and `OutboundQueueFull`
  - keep writer readiness, disconnect handling, reconnect logic, and subscription behavior (except for rate-limit-specific retries)

### Out

- Integrating `shared-rate_limiter` into shared-ws (explicitly *not* desired).
- Distributed/global multi-process coordination.

## Flow Analysis (Impacted Areas)

### Impact Map

- `shared-ws/src/ws/actor.rs`
  - delete rate limiter state and gating calls on outbound drain
  - ensure outbound queue drain still functions (best-effort send, error on writer failure)
- `shared-ws/src/ws/mod.rs`
  - stop re-exporting legacy limiter types
- `shared-ws/src/core/mod.rs`
  - remove `global_rate_limit` and `rate_limit` modules
- `shared-ws/src/ws/rate_limit.rs`
  - remove backwards-compat re-export module

### As-Is

- Outbound drain path calls `acquire_outbound_permit()` which can return `WebSocketError::RateLimited { retry_after }` and schedules a retry (`schedule_outbound_drain`).

### To-Be

- Outbound drain path does not gate on rate limiting.
- If callers need rate limiting, they must place an external coordinator in front of `shared-ws` (e.g., `shared-rate_limiter` + a small actor that calls `ws.ask(WsDelegatedRequest { .. })`).

## TODO (TDD Checklist)

- [x] Tests-first: add/adjust integration tests in `shared-ws/tests/` that prove:
  - [x] outbound drain sends queued messages promptly (no internal retry scheduling)
  - [x] writer failure path still triggers disconnect handling (existing semantics preserved; exercised via existing drain error path)
- [x] Remove rate-limiter-specific fields and logic from `WebSocketActorArgs` and `WebSocketActor`:
  - [x] remove `global_rate_limit`
  - [x] remove `rate_limiter`
  - [x] remove `acquire_outbound_permit()` and internal retry scheduling (`schedule_outbound_drain()`)
- [x] Remove legacy limiter modules + exports:
  - [x] delete `shared-ws/src/core/rate_limit.rs`
    - moved `WsCircuitBreaker` / `jitter_delay` to `shared-ws/src/core/connection_policy.rs`
  - [x] delete `shared-ws/src/core/global_rate_limit.rs`
  - [x] delete `shared-ws/src/ws/rate_limit.rs`
  - [x] update `shared-ws/src/ws/mod.rs` public surface
- [x] Migrate tests/examples that referenced legacy limiter fields/types.
- [x] Test gate: `cargo test --workspace` passes

## Implementation Notes (What Landed)

- Legacy limiter code removed:
  - deleted `shared-ws/src/core/global_rate_limit.rs`
  - deleted `shared-ws/src/core/rate_limit.rs`
  - deleted `shared-ws/src/ws/rate_limit.rs`
  - removed `shared-rate_limiter` dependency from `shared-ws/Cargo.toml`
- Kept reconnect/circuit-breaker utilities in a non-limiter module:
  - `shared-ws/src/core/connection_policy.rs` now contains `WsCircuitBreaker` and `jitter_delay`
- Websocket actor no longer gates outbound drain on permits and no longer schedules limiter-driven retries:
  - `shared-ws/src/ws/actor.rs` drains best-effort and relies on external coordination for rate limiting
- New integration test:
  - `shared-ws/tests/outbound_drain_no_limiter.rs`

## Acceptance Criteria

- [x] AC1: `shared-ws` compiles with no legacy limiter modules present.
- [x] AC2: `shared-ws` contains no outbound rate limiting policy (no internal “RateLimited” generation purely from a built-in limiter).
- [x] AC3: Existing reconnect/subscription behavior is preserved (except for intentionally removed limiter-specific retry scheduling and public exports).

## Test Plan

- Unit:
  - any extracted helper modules (if created)
- Integration:
  - outbound drain behavior without internal rate limiting

## Definition of Done (Sprint)

- Legacy limiter code deleted
- All tests passing
- No compatibility bridges remain (explicitly removed exports documented in README/release notes if applicable)
