# defi-ws Architecture

This crate provides a high-performance WebSocket connection manager designed for:

- Fast IO loops outside the actor runtime.
- Deterministic state transitions inside a single kameo actor.
- Self-healing reconnects with preserved adapter state.
- Transport-neutral frames (`WsFrame`).
- A swappable transport boundary (tokio-tungstenite today; fastwebsockets later).

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
