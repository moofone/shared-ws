# defi-ws Architecture

This crate provides a high-performance WebSocket connection manager designed for:

- Fast IO loops outside the actor runtime.
- Deterministic state transitions inside a single kameo actor.
- Self-healing reconnects with preserved adapter state.
- Zero-copy inbound parsing (borrow transport frame bytes).
- A swappable transport boundary (tokio-tungstenite today; fastwebsockets later).

## Key Components

### `WebSocketActor<E, R, P, T>`

`E: WsExchangeHandler` owns all adapter-specific state (auth, subscriptions, decode pipeline).

`R: WsReconnectStrategy` decides backoff behavior.

`P: WsPingPongStrategy` handles heartbeats and staleness detection.

The actor:

- Spawns a `reader_task` that blocks on socket reads and forwards each frame into the actor mailbox.
- Spawns a `WsWriterActor` to serialize outbound frames.
- Runs subscription/auth orchestration and disconnect classification inside the actor.

### Reader Task (Outside Kameo)

The reader runs in a dedicated Tokio task and does:

- `read.next()` on the socket stream.
- On each frame: `actor_ref.tell(WebSocketEvent::Inbound(frame))`.
- On close/error: `actor_ref.tell(WebSocketEvent::Disconnect { ... })`.

This isolates IO from actor scheduling while keeping state mutations within the actor.

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
loops are monomorphized and do not allocate per frame.

## Known Gaps

- Subscription/auth encoding still returns owned bytes (`Vec<u8>`). For true zero-allocation
  outbound hot paths, evolve the traits to "encode into caller-provided buffer" APIs.

## Test Coverage Notes

See:

- `tests/zero_copy.rs`: validates zero-copy inbound parse pointer behavior.
- `tests/state_recovery.rs`: validates reconnect + subscription state invariants across disconnects.
