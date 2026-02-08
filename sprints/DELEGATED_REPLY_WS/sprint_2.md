# Sprint 2: Confirmation Matching (Inbound Correlation -> `Confirmed`)

Source spec: `spec/DELEGATED_REPLY_WS.md`

## Context (Spec Excerpts For This Sprint)

- `Confirmed` requires endpoint-specific semantics: shared-ws must be protocol-agnostic and let endpoints define “success”.
- The matcher must be fast and optional; high-volume feeds should not pay parsing cost if they don’t use delegated confirm.
- For Deribit JSON-RPC, confirmation is a response frame with matching `id` and `result` (or `error`).

## Goal

Add a pluggable confirmation matcher hook and complete delegated requests as `Confirmed` when a matching inbound response is observed.

## Scope

### In

- Define a protocol-agnostic “request match” surface (either a new trait or defaulted methods on `WsEndpointHandler`).
- Wire matcher into `dispatch_endpoint` so it can complete pending delegated requests.
- Add `Confirmed` happy-path integration tests (Case 1).
- Add error completion when matcher indicates request rejection (`EndpointRejected`).
Note: rate limiting is external. If an endpoint wants to surface server throttling signals, this should be exposed as optional metadata on the match result for callers/coordinators to interpret (not applied inside `shared-ws`).

### Out

- Full removal of legacy shared-ws limiter modules (Sprint 3).
- `examples-ws` Deribit e2e tests (Sprint 4).

## Flow Analysis (Impacted Areas)

### Impact Map

- `shared-ws/src/core/types.rs`
  - Option A (preferred for performance): extend `WsEndpointHandler` with defaulted methods:
    - `fn maybe_request_response(&self, data: &[u8]) -> bool { false }`
    - `fn match_request_response(&mut self, data: &[u8]) -> Option<WsRequestMatch> { None }`
  - Define `WsRequestMatch` (request_id + ok/err + optional feedback).
- `shared-ws/src/ws/actor.rs`
  - In `dispatch_endpoint`, before `parse_frame`, call matcher hooks and complete pending request if matched.
  - Ensure completion wakes all waiters and removes the pending entry.

### As-Is

- `dispatch_endpoint` currently handles subscription ACKs via `WsSubscriptionManager`, but has no request-id correlation mechanism for arbitrary requests.

### To-Be

1. On inbound frame:
   - If `handler.maybe_request_response(bytes)` returns `true`, call `handler.match_request_response(bytes)`.
2. If a match is returned:
   - Lookup pending entry by `request_id`.
   - If found and the entry is awaiting confirmation:
     - success => `WsDelegatedOk { confirmed: true }`
     - failure => `WsDelegatedError::EndpointRejected { .. }`
   - If match contains rate-limit feedback:
     - apply feedback to limiter (or enqueue for application in Sprint 3 if limiter not yet global).
3. Continue normal parse flow so existing handlers can still observe frames (unless explicitly configured to short-circuit).

## TODO (TDD Checklist)

- [x] Define `WsRequestMatch` type (location: `shared-ws/src/core/types.rs`)
  - fields:
    - `request_id: u64`
    - `result: Result<(), String>` (or a small error enum)
    - optional `rate_limit_feedback` (protocol-agnostic; informational only)
- [x] Extend `WsEndpointHandler` with matcher hooks (default no-op) in `shared-ws/src/core/types.rs`:
  - [x] `maybe_request_response(&self, data: &[u8]) -> bool`
  - [x] `match_request_response(&mut self, data: &[u8]) -> Option<WsRequestMatch>`
- [x] Implement completion logic in `shared-ws/src/ws/actor.rs::dispatch_endpoint()`:
  - [x] On match success: complete waiters with `Confirmed` and remove pending entry
  - [x] On match failure: complete waiters with `EndpointRejected` and remove pending entry
  - [x] Ensure confirm-mode correctness:
    - `Sent` mode requests must not be “re-confirmed”
    - `Confirmed` mode requests must only complete on match or deadline
- [x] Add unit tests for completion behavior:
  - [x] `PendingTable::complete_confirmed` only completes `confirm_mode=Confirmed`
- [x] Add shared-ws integration test (Case 1):
  - [x] server receives outbound subscribe frame with id
  - [x] server sends a matching response frame (id + success)
  - [x] `ask(WsDelegatedRequest { confirm_mode: Confirmed })` returns `Ok(confirmed=true)`
- [x] Add integration test for `EndpointRejected`:
  - [x] server sends matching id + error response
  - [x] request completes with `WsDelegatedError::EndpointRejected`
- [x] Test gate: `cargo test --workspace` passes

## Implementation Notes (What Landed)

- Types and hooks:
  - `shared-ws/src/core/types.rs`:
    - `WsRequestMatch`
    - `WsRateLimitFeedback`
    - defaulted `WsEndpointHandler::{maybe_request_response, match_request_response}`
- Delegated completion wiring:
  - `shared-ws/src/ws/actor.rs`:
    - calls matcher hooks in `dispatch_endpoint` before normal parse
    - completes pending requests via `complete_delegated_from_match`
- Pending table additions:
  - `shared-ws/src/ws/delegated_pending.rs`: `complete_confirmed` / `complete_rejected`
- Tests:
  - `shared-ws/tests/delegated_reply_sprint2.rs`: Case 1 Confirmed + EndpointRejected

## Acceptance Criteria

- [x] AC1: Endpoint handlers can opt in to confirmation matching without affecting endpoints that don’t care.
- [x] AC2: Case 1 (happy path confirmed) is implemented and tested.
- [x] AC3: Rejection responses are surfaced as `EndpointRejected`.
- [x] AC4: Performance guardrail: endpoints that don’t opt in pay only a single branch (default `maybe_request_response` false).

## Test Plan

- Unit:
  - completion helper + matcher hook defaulting behavior
- Integration (A mocked / B mocked):
  - WebSocketActor + local server (response correlation)
- End-to-end:
  - deferred to Sprint 4 (`examples-ws`)

## Definition of Done (Sprint)

- All TODOs complete
- `Confirmed` path works end-to-end in shared-ws integration tests
- 100% unit coverage for new/changed modules on this sprint’s critical path
- No temporary adapters left behind beyond planned legacy limiter removal in Sprint 3
