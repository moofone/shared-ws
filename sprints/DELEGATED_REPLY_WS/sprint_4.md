# Sprint 4: Deribit Example + End-to-End Tests (All 4 Cases)

Source spec: `spec/DELEGATED_REPLY_WS.md`

## Context (Spec Excerpts For This Sprint)

- `examples-ws` must demonstrate Deribit subscribe with delegated reply + rate limiter.
- Tests required:
  - shared-ws: unit + integration (all 4 cases)
  - examples-ws: end-to-end style tests (all 4 cases)

## Goal

Deliver a concrete Deribit delegated-subscribe example and prove the full 4-outcome matrix with end-to-end style tests.

## Scope

### In

- `examples-ws` Deribit delegated subscribe flow using `WsDelegatedRequest`.
- Deribit confirmation matcher implementation (JSON-RPC id correlation).
- 4 end-to-end style tests in `examples-ws/tests/` mirroring:
  1. Confirmed
  2. Denied
  3. NotDelivered
  4. Unconfirmed
- shared-ws integration tests hardened so the same 4 cases are covered within shared-ws itself (in addition to examples-ws).

### Out

- Production “probe” behavior in shared-ws. For `Unconfirmed`, the example should show caller-driven probing strategies, but shared-ws core should only surface ambiguity.

## Flow Analysis (Impacted Areas)

### Impact Map

- `examples-ws/src/endpoints/deribit.rs`
  - Implement matcher hooks introduced in Sprint 2 on `DeribitPublicHandler` (or a wrapper handler).
  - Implement delegated subscribe helper that:
    - builds JSON-RPC `public/subscribe` frame with `id=request_id`
    - acquires a permit from `shared-rate_limiter` (external coordinator) immediately before calling `ws.ask(...)`
    - calls `ws.ask(WsDelegatedRequest { confirm_mode: Confirmed, .. })`
    - handles `Denied | NotDelivered | Unconfirmed | Confirmed`
- `examples-ws/tests/deribit_public_flow.rs`
  - Add new tests for delegated reply outcomes
  - Extend local server harness to:
    - send JSON-RPC success/error responses with matching id
    - optionally close connection to force write failure
    - optionally stay silent to force timeout
- `shared-ws/tests/`
  - Add/extend integration tests covering shared-ws-owned outcomes (`Confirmed`, `NotDelivered`, `Unconfirmed`)
  - `Denied` is covered in `examples-ws` via an external coordinator wrapper

## TODO (TDD Checklist)

- [x] Implement Deribit matcher (fast-path + match):
  - [x] `maybe_request_response`: cheap check for `"id"` + either `"result"`/`"error"`
  - [x] `match_request_response`: extract `id` as `u64` and success/failure
  - [x] No allocation on hot path (purpose-built scanner)
- [x] Add delegated subscribe helper in examples-ws:
  - [x] `request_id` generation (monotonic counter)
  - [x] `fingerprint` from semantic payload (stable hash of method + channels)
  - [x] external rate limiter coordinator acquires/commits/refunds around `ws.ask(...)`
- [x] Add 4 `examples-ws` tests (end-to-end style):
  - [x] `delegated_subscribe_happy_path_confirmed`
  - [x] `delegated_subscribe_denied_by_rate_limiter`
  - [x] `delegated_subscribe_not_delivered_refunds`
  - [x] `delegated_subscribe_unconfirmed_times_out`
- [x] Shared-ws integration tests for shared-ws-owned outcomes are present:
  - [x] Confirmed (`shared-ws/tests/delegated_reply_sprint2.rs`)
  - [x] NotDelivered + Unconfirmed (`shared-ws/tests/delegated_reply_sprint1.rs`)
  - [x] (Denied is intentionally external; covered by `examples-ws`)
- [x] Document recommended caller decision tree for `Unconfirmed` in `examples-ws/README.md`
- [x] Test gate:
  - [x] `cargo test --workspace` passes (shared-ws + examples-ws)

## Implementation Notes (What Landed)

- Workspace setup:
  - `shared-ws/Cargo.toml` now defines a workspace with `examples-ws` as a member.
- New examples crate:
  - `shared-ws/examples-ws/src/endpoints/deribit.rs`: Deribit matcher + subscribe request builder + fingerprint
  - `shared-ws/examples-ws/src/coordinator.rs`: external rate-limiter coordinator around delegated sends
  - `shared-ws/examples-ws/tests/deribit_public_flow.rs`: 4 outcome tests (Confirmed, Denied, NotDelivered, Unconfirmed)
  - `shared-ws/examples-ws/README.md`: Unconfirmed decision tree guidance

## Acceptance Criteria

- [x] AC1: Deribit delegated subscribe example compiles and runs using public shared-ws API.
- [x] AC2: `examples-ws` has 4 end-to-end style tests that pass reliably.
- [x] AC3: shared-ws has unit/integration coverage for shared-ws-owned outcomes (`Confirmed`, `NotDelivered`, `Unconfirmed`).
- [x] AC4: No legacy limiter code remains in shared-ws; rate limiting is external (e.g. `shared-rate_limiter` in a coordinator).

## Test Plan

- Unit:
  - matcher extraction logic (if factored)
- Integration:
  - shared-ws local-server based tests (4 cases)
- End-to-end:
  - examples-ws tests (4 cases)

## Definition of Done (Sprint)

- All TODOs complete
- All acceptance criteria met
- No “TODO later” scaffolding in production code
- Documentation updated for `Unconfirmed` decision-making
