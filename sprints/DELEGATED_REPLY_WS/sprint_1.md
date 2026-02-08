# Sprint 1: Delegated Send MVP (Writer Result + Pending Table + Timeouts)

Source spec: `spec/DELEGATED_REPLY_WS.md`

## Context (Spec Excerpts For This Sprint)

- Add `WsDelegatedRequest` that callers `ask()` and receive one of: `NotDelivered | Unconfirmed | Confirmed`.
- Dedup: same `request_id` + same `fingerprint` must not double-send; attach waiters instead.
- Pending request state must be bounded and GC’d by deadline to prevent leaks.
- `NotDelivered` requires the send path to observe transport write failure (not just mailbox enqueue).

## Goal

Land the delegated request API and the core pending/timeout mechanics, including deterministic `NotDelivered`, without yet requiring endpoint-specific confirmation matching.

By the end of this sprint, `shared-ws` can:
- Detect write failure (`NotDelivered`)
- Time out awaiting confirmation (`Unconfirmed`)

## Scope

### In

- New public request API:
  - `WsDelegatedRequest`
  - `WsConfirmMode` (`Sent` vs `Confirmed`)
  - `WsDelegatedOk` / `WsDelegatedError`
- Pending table implementation (dedup + multi-waiter + bounded + GC).
- Delegated writer send completion surfaced back to the websocket actor (no actor-thread IO blocking).
- shared-ws unit + integration tests covering:
  - Case 3 (NotDelivered)
  - Case 4 (Unconfirmed)

### Out

- Endpoint-defined confirmation matcher completing `Confirmed` on inbound frames (Sprint 2).
- Removing shared-ws legacy limiter modules (Sprint 3).
- `examples-ws` end-to-end tests (Sprint 4).

## Flow Analysis (Impacted Areas)

### Impact Map

- `shared-ws/src/ws/actor.rs`
  - Add `WsDelegatedRequest` handler returning `DelegatedReply<...>`.
  - Add pending table state + GC scheduling.
  - Add internal event(s) to receive completion from async writer task (e.g. `WebSocketEvent::DelegatedWriteDone { .. }`).
  - Note: current `send_with_writer()` uses `tell()` and cannot be used for delegated requests.
- `shared-ws/src/ws/writer.rs`
  - No functional change required, but delegated requests must use `writer.ask(WriterWrite { .. }).await` to observe `WebSocketResult<()>`.
- `shared-ws/src/core/types.rs`
  - Add new error/ok types if they belong in `core` (or keep them in `ws/actor.rs` and re-export).
- Tests
  - New unit tests for pending table (new module) under `shared-ws/src/ws/...` or `shared-ws/src/core/...`.
  - New integration tests under `shared-ws/tests/` (reuse server harness patterns in `shared-ws/tests/state_recovery.rs`).

### As-Is Critical Path

- Outbound drain: `shared-ws/src/ws/actor.rs::drain_pending_outbound()`
- (Legacy) Permit gate: `shared-ws/src/ws/actor.rs::acquire_outbound_permit()` (removed in Sprint 3; shared-ws becomes rate limiter agnostic)
- Send: `shared-ws/src/ws/actor.rs::send_with_writer()` uses `writer.tell(...).send().await` (cannot observe real send errors)

### To-Be (Delegated Path Only, Sprint 1)

- `WsDelegatedRequest` handler:
  1. Insert/lookup pending entry by `request_id` + `fingerprint` (dedup)
  2. Spawn detached task that performs `writer.ask(WriterWrite { message }).await`
  3. Deliver result back to actor via an internal message/event
  4. Complete waiters:
     - writer error: `NotDelivered`
     - writer ok + `confirm_mode=Sent`: reply `confirmed=false` (sent)
     - writer ok + `confirm_mode=Confirmed`: keep pending until timeout (no matcher yet) then reply `Unconfirmed`

### Key Risks + Mitigations

- Risk: actor blocks on IO if we `await writer.ask` inline.
  - Mitigation: always spawn detached task (`ctx.spawn` or `tokio::spawn`) and message the actor back with the result.
- Risk: pending map grows forever.
  - Mitigation: min-heap (deadline, request_id) + scheduled expiry event; plus `max_pending_requests` hard cap.

## TODO (TDD Checklist)

- [x] Create a new internal module: `shared-ws/src/ws/delegated_pending.rs`
  - [x] Implement `PendingTable` (dedup + multi-waiter + cap + deadline min-heap + expiry):
    - `insert_or_join(request_id, fingerprint, deadline, confirm_mode, waiter) -> (PendingInsertOutcome, waiter)`
    - `mark_sent_ok(request_id)` (completes `Sent` mode immediately)
    - `complete_not_delivered(request_id)`
    - `expire_due(now) -> Vec<PendingExpired<_>>`
  - [x] Unit tests for:
    - dedup joiners (same id+fingerprint)
    - mismatch (same id, different fingerprint)
    - expiry removes entry and completes all waiters
    - cap rejects with `TooManyPending`
- [x] Add new public API types in `shared-ws/src/ws/delegated.rs` (re-exported by `shared-ws/src/ws/mod.rs`):
  - [x] `WsConfirmMode`
  - [x] `WsDelegatedRequest`
  - [x] `WsDelegatedOk`
  - [x] `WsDelegatedError` (`NotDelivered`, `Unconfirmed`, `PayloadMismatch`, `TooManyPending`)
- [x] Implement `KameoMessage<WsDelegatedRequest>` for `WebSocketActor<...>` returning `DelegatedReply<Result<WsDelegatedOk, WsDelegatedError>>`
  - [x] Use `ctx.reply_sender()` to get `ReplySender` and store it in pending table
  - [x] Ensure `tell()` callers (no reply_sender) work (dedup applies, but no waiters)
- [x] Add writer completion plumbing:
  - [x] Add internal events: `WebSocketEvent::DelegatedWriteDone { .. }` + `WebSocketEvent::DelegatedExpire`
  - [x] Spawn detached task that `await`s `writer.ask(WriterWrite{..})` and tells the actor the outcome
- [x] Add GC scheduling for pending entries:
  - [x] Deadline min-heap inside `PendingTable`
  - [x] Next-expiry scheduling via `schedule_delegated_expiry()` and `WebSocketEvent::DelegatedExpire`
- [x] Integration tests in `shared-ws/tests/`:
  - [x] Case 3: NotDelivered (writer returns error); expect `NotDelivered`
  - [x] Case 4: Unconfirmed (writer ok + `confirm_mode=Confirmed` + no matcher yet); expect `Unconfirmed`
  - [x] Dedup validation: 2 concurrent asks produce 1 send
- [x] Test gate: `cargo test --workspace` passes

## Implementation Notes (What Landed)

- Files added:
  - `shared-ws/src/ws/delegated.rs`
  - `shared-ws/src/ws/delegated_pending.rs`
  - `shared-ws/tests/delegated_reply_sprint1.rs`
- Wiring:
  - `shared-ws/src/ws/actor.rs` now handles `WsDelegatedRequest` and uses detached `writer.ask(...)` to observe real send errors.
  - Pending entries are GC’d by deadline via a min-heap + internal timer event.

## Acceptance Criteria

- [x] AC1: `WsDelegatedRequest` exists and can be `ask()`’d; caller blocks until completion.
- [x] AC2: Duplicate `(request_id, fingerprint)` joins the existing in-flight request without a second send.
- [x] AC3: `NotDelivered` and `Unconfirmed` outcomes are observable and tested.
- [x] AC4: Pending table is bounded and GC’d; no unbounded growth under repeated timeouts.

## Test Plan

- Unit:
  - pending table (dedup/mismatch/expiry/cap)
- Integration (A mocked / B mocked):
  - WebSocketActor with local ws server and controlled behaviors (close/no-ack)
- End-to-end:
  - deferred to Sprint 4 (`examples-ws`)

## Definition of Done (Sprint)

- All TODOs complete
- All acceptance criteria met
- New/changed code has 100% unit coverage (line + branch) for delegated pending table + accounting
- Integration tests for cases 2/3/4 pass
- No temporary scaffolding left behind (beyond explicitly planned legacy limiter removal in sprint 3)
