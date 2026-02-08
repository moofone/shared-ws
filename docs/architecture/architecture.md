# shared-ws Architecture

This crate provides a high-performance WebSocket connection manager designed for:

- Fast IO loops outside the actor runtime.
- Deterministic state transitions inside a single kameo actor.
- Self-healing reconnects with preserved adapter state.
- Transport-neutral frames (`WsFrame`).
- A swappable transport boundary (tokio-tungstenite today; fastwebsockets later).

## Related Docs

- Self-healing + state preservation: [`docs/architecture/self_healing.md`](self_healing.md)
- Pluggable ping/pong: [`docs/architecture/pluggable_pingpong.md`](pluggable_pingpong.md)
- Latency policy hooks: [`docs/architecture/latency_policy_hooks.md`](latency_policy_hooks.md)
- JSON parse modes: [`docs/architecture/json_parse_modes.md`](json_parse_modes.md)
- Delegated reply requests (design + semantics): [`spec/DELEGATED_REPLY_WS.md`](../../spec/DELEGATED_REPLY_WS.md)

## Key Components

### `WebSocketActor<E, R, P, I, T>`

`E: WsEndpointHandler` owns endpoint-specific state (auth, subscriptions, protocol decode pipeline).

`R: WsReconnectStrategy` decides backoff behavior.

`P: WsPingPongStrategy` handles heartbeats and staleness detection.

`I: WsIngress` is the tight-loop ingress decoder/aggregator/filter that runs outside kameo.

The actor:

- Spawns a `reader_task` that blocks on socket reads.
- Spawns a `WsWriterActor` to serialize outbound frames.
- Runs subscription/auth orchestration and disconnect classification inside the actor.
- Optionally supports "send + await outcome" flows via `ask(WsDelegatedRequest)` (see below).

### Reader Task (Outside Kameo)

The reader runs in a dedicated Tokio task and does:

- `read.next()` on the socket stream.
- For each frame: `ingress.on_frame(&frame)`.
- Depending on the ingress decision:
  - ignore the frame,
  - forward the raw `WsFrame` into the actor (`WebSocketEvent::Inbound`), or
  - emit a compact decoded event (`IngressEmit<E::Message>`).
- On close/error: `actor_ref.tell(WebSocketEvent::Disconnect { ... })`.

Ingress state continuity:

- The actor owns `I` and moves it into the reader task on connect.
- When the reader task terminates, it returns `I` via the task `JoinHandle`, and the actor stores it
  back so state is preserved across reconnects.

Stale detection continuity:

- Even if ingress ignores all frames, the reader task periodically emits `WebSocketEvent::InboundActivity`
  so `WsHealthMonitor` does not incorrectly mark the connection stale.

### Writer Actor (Kameo)

`WsWriterActor` owns the transport writer (`T::Writer`) and provides:

- `WriterWrite { message }` for single-frame sends.
- `WriterWriteBatch { messages }` for batched feed+flush sends.

### Delegated Reply Requests (Send + Await Confirmation)

For request/response style websocket APIs (for example JSON-RPC over websocket), `shared-ws`
exposes an ask-able message:

- `WsDelegatedRequest { request_id, fingerprint, frame, confirm_deadline, confirm_mode }`

Semantics:

- `confirm_mode = Sent`: reply once the writer accepts the frame for writing.
- `confirm_mode = Confirmed`: reply only once an endpoint-specific confirmation is observed, or
  `Unconfirmed` if the deadline elapses.
- Duplicate in-flight requests for the same `request_id` are deduped:
  - same `fingerprint`: join the existing pending entry without re-sending.
  - different `fingerprint`: fail fast with `PayloadMismatch`.

Confirmation matching is endpoint-defined and protocol-agnostic:

- `WsEndpointHandler::maybe_request_response(&[u8]) -> bool` is a fast precheck (default `false`).
- `WsEndpointHandler::match_request_response(&[u8]) -> Option<WsRequestMatch>` returns a correlated
  match (request id + success/failure + optional retry-after hints).

Pending requests are bounded and garbage-collected by deadline inside the actor.

Important: `shared-ws` is rate-limiter agnostic. If you need outbound throttling, apply an external
rate limiter/coordinator before calling `ask(WsDelegatedRequest { ... })`.

### Health + Stale Detection

`WsHealthMonitor` tracks:

- Message counts, last receive time, reconnect count.
- Recent error buffers (bounded).
- RTT histogram for policy-driven disconnects.

Stale detection uses:

- `WsPingPongStrategy::is_stale()` for heartbeat timeouts.
- `WsHealthMonitor::is_stale()` for "no inbound data" windows.

### TLS Policy

`WsTlsConfig { validate_certs: bool }` is safe-by-default (`true`).

## Transport Boundary

`WsFrame` is the transport-neutral frame surface. The `WsTransport` trait returns:

- `Reader: Stream<Item = Result<WsFrame, WebSocketError>>`
- `Writer: Sink<WsFrame, Error = WebSocketError>`

The only dynamic dispatch is on `connect()` (boxed future). The inner read/write
loops are monomorphized.

## Known Gaps

- Subscription/auth encoding still returns owned bytes (`Vec<u8>`). For true zero-allocation
  outbound hot paths, evolve the traits to "encode into caller-provided buffer" APIs.

## Test Coverage Notes

See:

- `tests/zero_copy.rs`: validates zero-copy inbound parse pointer behavior.
- `tests/state_recovery.rs`: validates reconnect + subscription state invariants across disconnects.
