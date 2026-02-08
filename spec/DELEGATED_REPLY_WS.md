# Delegated Reply WS Requests (shared-ws PRD)

Status: Draft
Last updated: 2026-02-08

## Summary

Add a delegated-reply request API to `shared-ws` so callers can `ask()` the websocket actor to:

1. Send an outbound request/frame.
2. Block until one of the following terminal outcomes occurs:
   - **Confirmed** (endpoint-specific success confirmation observed).
   - **NotDelivered** (send failed; definitely not sent).
   - **Unconfirmed** (send likely occurred but confirmation was not observed before deadline; ambiguous).

This PRD also defines the debouncing/dedup + timeout/GC rules required to prevent unbounded growth of pending-request state.

Important: `shared-ws` is **rate limiter agnostic**. “Denied” is an outcome of an *external* rate limiter/coordinator (e.g. `shared-rate_limiter`) that runs **before** calling `WebSocketActor::ask(...)`.

## Motivation

`shared-ws` already provides a high-performance websocket actor (`src/ws/actor.rs`) and a send-side rate limiter (`src/core/rate_limit.rs`, `src/core/global_rate_limit.rs`), but the current public message surfaces are “fire and forget” at the application level:

- `WsSubscriptionUpdate` returns once messages are enqueued/drained, not once an endpoint confirms success.
- For endpoints like Deribit (JSON-RPC over WS), users often need "send subscribe + wait for confirmation" to safely build higher-level state machines.

We also need to debounce duplicate requests (same logical operation arriving twice within microseconds) while the first request is still pending confirmation.

## Goals

1. Provide a **single** “send + await confirmation” API usable by any websocket endpoint handler.
2. Keep the hot path fast:
   - No payload copies beyond what shared-ws already does (`WsFrame` / `Bytes`).
   - No heap allocation per request in the common case (0 or 1 waiter).
3. Keep `shared-ws` **rate limiter agnostic**:
   - No dependency on `shared-rate_limiter`.
   - No rate limiter-specific fields on `WsDelegatedRequest`.
4. Prevent memory leaks:
   - Pending request map must be bounded and must expire entries with deadlines.
5. Make ambiguous outcomes first-class:
   - If “sent” but “not confirmed”, surface `Unconfirmed` with enough context for callers to decide a follow-up probe or retry strategy.

## Non-Goals

1. “Exactly once” to the remote endpoint. Without endpoint-provided idempotency/state, we can only provide:
   - local debouncing (don’t double-send while in-flight)
   - explicit ambiguity handling (Unconfirmed)
2. A mandatory “probe” hook inside shared-ws. Probing is endpoint-specific and may require extra IO; we surface the information needed so callers can probe using their own logic.

## Terminology (Precise Semantics)

- **Denied**: the (external) rate limiter rejected the attempt; the frame was never sent to `shared-ws` for IO.
- **NotDelivered**: we attempted to send, but the send definitely did not occur (writer error before the frame could be written). Safe for an external coordinator to refund quota.
- **Sent**: the writer accepted the frame for write (local success). This does **not** imply remote application.
- **Confirmed**: an endpoint-specific confirmation was observed (e.g., Deribit JSON-RPC response with matching id and `success=true`).
- **Unconfirmed**: the frame was sent (or assumed sent) but no confirmation was observed before a deadline. Ambiguous.

Note: rate limiting is about **send** volume; it is not inherently tied to endpoint confirmation. This API still supports “wait for confirmation” for user-level correctness.

## Current Architecture Touchpoints

- Websocket actor: `shared-ws/src/ws/actor.rs`
- Writer actor: `shared-ws/src/ws/writer.rs`
- Endpoint interface: `shared-ws/src/core/types.rs` (`WsEndpointHandler`, `WsSubscriptionManager`)
- Existing internal rate limiting:
  - `shared-ws/src/core/rate_limit.rs`
  - `shared-ws/src/core/global_rate_limit.rs`

## Proposed Design

### 1) New Delegated Request Message

Add a new message handled by `WebSocketActor<...>`:

`WsDelegatedRequest` (name bikeshed) fields:

- `request_id: u64`
  - Caller-provided correlation id. For Deribit JSON-RPC, this is the JSON-RPC `id`.
- `fingerprint: u64`
  - Stable hash of the semantic request payload (used for dedup integrity).
- `frame: WsFrame`
  - Outbound bytes, zero-copy (`Bytes`) via `WsFrame`.
- `confirm_deadline: Instant`
  - Absolute deadline for confirmation.
- `confirm_mode: WsConfirmMode`
  - `Sent` (reply once writer accepts) OR
  - `Confirmed` (reply only once inbound confirmation is matched)

Reply type:

- `DelegatedReply<Result<WsDelegatedOk, WsDelegatedError>>`

Where `WsDelegatedOk` minimally includes:

- `request_id`
- `confirmed: bool`
- optional `meta` (endpoint-defined small metadata; not required for MVP)

### 2) Pluggable Confirmation Matching (Protocol-Agnostic)

We need a protocol-agnostic way for shared-ws to decide when an inbound frame confirms a pending request.

Add a new optional trait used by endpoints:

`trait WsPendingRequestMatcher` (exact name bikeshed):

- `fn maybe_request_response(&self, data: &[u8]) -> bool`
  - fast precheck; default `false` so high-volume feeds don’t pay extra parsing cost.
- `fn match_request_response(&mut self, data: &[u8]) -> Option<WsRequestMatch>`
  - returns a match with:
    - `request_id: u64`
    - `result: Result<(), String>` (success/failure message)
    - optional `rate_limit_feedback` (retry-after, bucket hint) when the server signals throttling via payload

Integrate this into `dispatch_endpoint` (`shared-ws/src/ws/actor.rs`) before or alongside `handler.parse_frame`.

Notes:

- This avoids forcing a universal “success response” definition into the rate limiter or shared-ws core.
- The matcher can do a partial parse (e.g., Deribit: look for `"id": <n>` + `"error"`/`"result"`).

### 3) Pending Map + Debounce/Dedup

Inside `WebSocketActor`, maintain:

- `pending_requests: HashMap<u64, PendingRequest>`

`PendingRequest` stores:

- `fingerprint: u64`
- `deadline: Instant`
- waiters:
  - `waiter0: Option<ReplySender<Result<WsDelegatedOk, WsDelegatedError>>>`
  - `waiters: Vec<ReplySender<...>>` (allocate only if duplicates occur)

Behavior:

1. On new request:
   - If `(request_id)` not present: create entry and send once.
   - If present with same fingerprint: attach waiter; **do not send again**.
   - If present with different fingerprint: reply immediately with `PayloadMismatch`.

### 4) Timeouts / GC (Prevent Unbounded Growth)

Requirements:

- Every pending entry has a hard deadline (`confirm_deadline`).
- The actor must periodically expire entries even if no inbound frames arrive.

Implementation options:

1. Min-heap of `(deadline, request_id)` (preferred for predictable cost).
2. Periodic sweep capped to `N` expirations per tick.

On expiration:

- Remove the pending entry.
- Reply all waiters with `WsDelegatedError::Unconfirmed { request_id, hint }`.

Also enforce:

- `max_pending_requests` hard cap; reject new requests with `TooManyPending` and log.

### 5) Rate Limiting Is External (shared-ws Knows Nothing)

`shared-ws` must not depend on or embed any rate limiting policy.

Recommended composition:

1. A caller (or coordinator actor) uses `shared-rate_limiter` to reserve quota **immediately before** calling `ws.ask(WsDelegatedRequest { .. })`.
2. The coordinator forwards the request to `shared-ws` and awaits the delegated outcome.
3. The coordinator commits/refunds the external permit based on the observed terminal outcome:
   - `NotDelivered` => refund
   - `Unconfirmed` => commit as “sent but unconfirmed” (no refund by default)
   - `Confirmed` => commit as confirmed
   - `Denied` => no permit was acquired; nothing to do

This keeps the limiter protocol-agnostic and keeps `shared-ws` focused on IO + correlation only.

### 6) Errors

Define a dedicated error surface for delegated requests (do not overload `WebSocketError`):

`enum WsDelegatedError`:

- `NotDelivered { request_id: u64, message: String }`
- `Unconfirmed { request_id: u64, message: String }`
- `PayloadMismatch { request_id: u64 }`
- `TooManyPending { max: usize }`
- `EndpointRejected { request_id: u64, message: String }`

### 7) Logging

`shared-ws` does not log “rate limit exceeded” because it does not make rate limit decisions.

Recommended logging inside `shared-ws`:

1. On too many pending: `warn!(pending_len, max, "pending request table full")`
2. On `NotDelivered`: `warn!(request_id, error, "delegated send not delivered")`
3. On `Unconfirmed` expiry: `info!(request_id, "delegated send unconfirmed (deadline elapsed)")`

## Example (examples-ws): Deribit Subscribe With Delegated Reply + Rate Limiter

Implement in `examples-ws` a Deribit-style JSON-RPC subscribe call that:

1. Builds a JSON-RPC `subscribe` frame with `id = request_id`.
2. Acquires a permit from `shared-rate_limiter` (external) immediately before sending.
3. Calls `ws.ask(WsDelegatedRequest { ... confirm_mode: Confirmed ... })`.
4. Handles outcomes:
   - Confirmed: proceed.
   - Denied: backoff using retry_after and retry with same request_id (optional).
   - NotDelivered: safe retry with same request_id.
   - Unconfirmed: choose one:
     - probe via a follow-up “list subscriptions” request (if supported)
     - wait for first notification and treat as implied confirmation
     - retry (risking duplicates)

## Tests

### shared-ws (unit + integration)

Add unit tests for pending map behavior:

1. Dedup joins waiters without re-send when same request_id+fingerprint arrives.
2. PayloadMismatch for same request_id with different fingerprint.
3. Expiration completes all waiters and removes entry.

Add integration tests using a stub `WsTransport`:

Case 1. Happy path:
- writer accepts send
- reader emits matching response
- expect `Confirmed`

Case 2. External rate limiter denies:
- out of scope for `shared-ws` unit/integration tests
- covered in `examples-ws` end-to-end style tests via a coordinator wrapper

Case 3. Allowed but message not delivered:
- writer returns error on send
- expect `NotDelivered`

Case 4. Allowed + sent but no confirmation:
- writer accepts send
- no matching response before deadline
- expect `Unconfirmed`
- pending entry cleaned up

### examples-ws (end-to-end style)

Add 4 tests mirroring the above outcomes using the public `shared-ws` API surface and a Deribit matcher:

1. `delegated_subscribe_happy_path_confirmed`
2. `delegated_subscribe_denied_by_rate_limiter`
3. `delegated_subscribe_not_delivered_refunds`
4. `delegated_subscribe_unconfirmed_times_out`

## Definition of Done

1. `shared-ws/spec/DELEGATED_REPLY_WS.md` exists and matches implementation.
2. shared-ws provides delegated request semantics without embedding rate limiting policy.
3. Coverage exists for:
   - shared-ws unit+integration tests: `Confirmed`, `NotDelivered`, `Unconfirmed`
   - examples-ws end-to-end style tests: all 4 terminal outcomes (including external `Denied`)
4. Pending map is bounded and cannot grow forever (timeouts/GC and max cap).
