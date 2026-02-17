# shared-ws

High-performance WebSocket infrastructure built around an actor-managed connection lifecycle, with
transport and protocol logic cleanly separated.

## Features

- Actor-managed WebSocket connection lifecycle (connect, disconnect, reconnect, shutdown).
- Transport abstraction (`WsTransport`) with a transport-neutral wire surface (`WsFrame`).
- High-throughput IO: reader/writer tasks run outside the actor runtime; the actor owns policies and
  state.
- Self-healing reconnects with handler/subscription state preserved across reconnects.
- Pluggable ping/pong strategies (protocol-level or application-level).
- Delegated request API: `ask(WsDelegatedRequest)` for "send + await sent/confirmed/rejected/timeout"
  outcomes (endpoint supplies confirmation matching).
- Outbound backpressure controls (bounded queueing) and an optional circuit breaker for connection
  attempts.
- Latency policy hooks (e.g. disconnect on sustained RTT percentile breaches).
- Opt-in instrumentation hooks:
  - `WsMetricsReporter` for forwarding metrics to any backend.
  - Low-frequency payload timestamp lag sampling for distributed “event-time vs now” monitoring.
- Built-in test utilities in `shared_ws::testing`:
  - `MockTransport` + `MockServer` for deterministic in-memory websocket tests
  - `JsonRpcDelegatedEndpoint`, `NoSubscriptions`, and `NoReconnect` helpers
  - explicit server-side socket-drop simulation via `MockServer::drop_socket()`

## Testing With Mock Transport

```rust,no_run
use std::time::{Duration, Instant};
use shared_ws::testing::{JsonRpcDelegatedEndpoint, MockTransport, NoReconnect};
use shared_ws::ws::{
    ForwardAllIngress, ProtocolPingPong, WebSocketActor, WebSocketActorArgs, WebSocketEvent,
    WsDelegatedRequest,
};

let (transport, mut server) = MockTransport::channel_pair();
let actor = WebSocketActor::spawn(WebSocketActorArgs {
    url: "ws://mock".to_string(),
    transport,
    reconnect_strategy: NoReconnect,
    handler: JsonRpcDelegatedEndpoint::default(),
    ingress: ForwardAllIngress::default(),
    ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
    enable_ping: false,
    stale_threshold: Duration::from_secs(60),
    ws_buffers: Default::default(),
    outbound_capacity: 32,
    circuit_breaker: None,
    latency_policy: None,
    payload_latency_sampling: None,
    registration: None,
    metrics: None,
});

// Simulate server-side socket close.
server.drop_socket();

let req = WsDelegatedRequest::confirmed(
    1,
    1,
    shared_ws::ws::into_ws_message("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}"),
    Instant::now() + Duration::from_millis(50),
);
let _ = actor.ask(req);
let _ = actor.tell(WebSocketEvent::Connect);
```

## Documentation

- Architecture: [`docs/architecture/architecture.md`](docs/architecture/architecture.md)

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
