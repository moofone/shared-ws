# DELEGATED_REPLY_WS Sprint Plan

**Spec (source of truth):** `spec/DELEGATED_REPLY_WS.md`

## Global Goal

Add a delegated-reply request API to `shared-ws` so callers can `ask()` the websocket actor to:

1. Send an outbound request/frame.
2. Block until one terminal outcome occurs: `Confirmed | NotDelivered | Unconfirmed`.

Important: `shared-ws` is **rate limiter agnostic**. “Denied” is produced by an external coordinator (e.g. `shared-rate_limiter`) that runs immediately before calling `ws.ask(...)`.

## Sprint Breakdown

- `sprints/DELEGATED_REPLY_WS/sprint_1.md`: Delegated request plumbing + pending table (dedup/GC) + writer send result (NotDelivered) + Unconfirmed timeout path.
- `sprints/DELEGATED_REPLY_WS/sprint_2.md`: Pluggable confirmation matching hook + `Confirmed` completion path + rate-limit feedback hook shape.
- `sprints/DELEGATED_REPLY_WS/sprint_3.md`: Remove legacy shared-ws rate limiting so the crate is rate limiter agnostic.
- `sprints/DELEGATED_REPLY_WS/sprint_4.md`: `examples-ws` Deribit delegated subscribe example + end-to-end tests (all 4 cases) + shared-ws integration tests hardening.

## Impact Map (High Level)

**As-Is (today):**
- Outbound gating: `shared-ws/src/ws/actor.rs::acquire_outbound_permit()` uses:
  - `shared-ws/src/core/global_rate_limit.rs` (actor-based batch permits), and/or
  - `shared-ws/src/core/rate_limit.rs` (fixed window).
- Outbound send: `shared-ws/src/ws/actor.rs::send_with_writer()` uses `writer.tell(WriterWrite { .. }).send().await` which only confirms mailbox enqueue, not transport write completion.
- Inbound dispatch: `shared-ws/src/ws/actor.rs::dispatch_endpoint()` runs subscription-ACK parsing via `WsSubscriptionManager`, then endpoint parsing via `WsEndpointHandler::parse_frame`.

**To-Be (end state):**
- New ask-able message: `WsDelegatedRequest` returning `DelegatedReply<Result<WsDelegatedOk, WsDelegatedError>>`.
- Pending request table (bounded + GC) inside the websocket actor for dedup + multi-waiter completion.
- Writer send result observable for delegated requests (needed for deterministic `NotDelivered`).
- Endpoint-defined confirmation matching hook to complete `Confirmed` without tying semantics into the limiter core.
- shared-ws internal rate-limiter modules removed; rate limiting is handled externally.

## Global Risks / Design Constraints

- **Send failure observability:** current `tell()` to writer cannot surface transport send errors. Delegated reply requires deterministic `NotDelivered`.
- **Hot-path performance:** avoid per-message allocations and payload copies; use `Bytes`/`WsFrame` as-is.
- **Pending table leaks:** must GC by deadline and apply a hard cap (`max_pending_requests`).
- **Ambiguity (`Unconfirmed`):** must be first-class, with enough context for callers to probe/decide.
- **Breaking API removal:** removing `WsRateLimiter`/`WsGlobalRateLimiterActor` exports is an intentional break; must be explicit in sprint 3.

## Testing / Quality Gates (Project DoD)

- 100% unit test coverage (line + branch) for all new/changed logic on the delegated request path and pending table.
- Symmetric integration tests (WireMock-style boundaries):
  - WebSocketActor with writer mocked (send ok / send error).
  - WebSocketActor with inbound confirmation frames injected (matcher hook).
- End-to-end acceptance tests in `examples-ws` covering all 4 terminal outcomes.
- No legacy rate limiter modules remain in `shared-ws` at end of sprint 4.

## Final Project Definition of Done

- `shared-ws` outcomes implemented and tested:
  1. `Confirmed`
  2. `NotDelivered`
  3. `Unconfirmed`
- “Denied” is covered by `examples-ws` tests via an external rate limiter coordinator.
- `shared-ws` no longer contains send-side limiter implementations.
- Pending request table is bounded and cannot grow unbounded (timeouts + cap).
- `examples-ws` includes a Deribit delegated subscribe example and 4 end-to-end style tests.
