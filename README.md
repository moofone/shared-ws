# shared-ws

High-performance WebSocket infrastructure built around an actor-managed connection lifecycle, with
transport and protocol logic cleanly separated.

## Features

- Actor-managed WebSocket connection lifecycle (connect, disconnect, reconnect, shutdown).
- Transport abstraction (`WsTransport`) with a transport-neutral wire surface (`WsFrame`).
- High-throughput IO: reader/writer tasks run outside the actor runtime; the actor owns policies and
  state.
- Self-healing reconnects with handler/subscription state preserved across reconnects.
- Deterministic delegated-auth reconnect support via auth-gated sessions:
  - `WsSessionMode::AuthGated`
  - `on_connection_opened(is_reconnect)`
  - `WsSetAuthenticated` and `WsReplaySubscriptions`
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

## Boundary Fixture Rule

When `shared-ws` is used behind an exchange or external-contract boundary, the owning boundary
actor should keep raw websocket contract fixtures under its own `test/fixtures/` tree, split by
message type.

Recommended pattern:

- raw WS fixtures live on the boundary actor only
- split fixture groups by contract/message family, for example:
  - `session/`
  - `orders/`
  - `balances/`
  - `positions/`
  - `fills/`
  - `market_data/`
- downstream business actors do not duplicate raw exchange websocket payloads
- every registered live WS contract has a required fixture entry and a replay test using
  `MockTransport` / `ReconnectableMockTransport`
- compliant fixtures must carry live provenance metadata:
  - `source = "live_capture"`
  - `captured_at_ms`
  - `capture_command`
  - `exchange_env`
- synthesized or malformed fixtures belong in a separate robustness bucket and do not satisfy
  live contract compliance
- registered WS fixture contracts validate that live websocket startup is allowed; they do not
  replay fixture frames into the runtime
- normal mode still opens the real websocket only after fixture existence/provenance validation
  passes
- explicit fixture-capture mode bypasses the gate so capture workflows can refresh fixtures, and
  still opens the real websocket
- live WS startup should be blocked unless the required fixture inventory exists and those
  fixtures are compliant live captures, except for explicit fixture-capture mode

`shared-ws::testing` gives the transport surface for deterministic replay; callers should add a
small fixture catalog/scenario layer on top so contract coverage is enforced rather than remembered.

## Managed Server (`ws::server`)

A statically dispatched, supervised WebSocket **server** that owns every
server-side transport primitive so applications never hold one directly. It is
the mirror of the managed client: an application supplies per-connection
application logic and a config, and gets back a handle it can dispatch to,
observe, and shut down.

```rust,no_run
use std::time::Duration;
use bytes::Bytes;
use shared_ws::ws::server::{
    spawn_managed_ws_server_task, ManagedWsServerStatus, WsServerConfig,
    WsServerConnectionContext, WsServerEndpointAction, WsServerEndpointEvent,
    WsServerEndpointFactory, WsServerEndpointHandler, WsServerRetryPolicy,
};
use shared_ws::ws::WsFrame;

#[derive(Clone)]
struct Feed(Bytes); // reference-counted payload: fanout clones a refcount, not bytes

#[derive(Debug, thiserror::Error)]
#[error("endpoint error")]
struct EndpointError;

struct Endpoint;
impl WsServerEndpointHandler for Endpoint {
    type Command = Feed;
    type Error = EndpointError;
    async fn on_event(
        &mut self,
        event: WsServerEndpointEvent<'_, Self::Command>,
    ) -> Result<WsServerEndpointAction, Self::Error> {
        Ok(match event {
            WsServerEndpointEvent::Command(feed) => {
                WsServerEndpointAction::Send(WsFrame::Binary(feed.0.clone()))
            }
            _ => WsServerEndpointAction::None,
        })
    }
}

struct Factory;
impl WsServerEndpointFactory for Factory {
    type Handler = Endpoint;
    fn create_endpoint(&self, _ctx: &WsServerConnectionContext) -> Self::Handler {
        Endpoint
    }
}

# async fn run() {
let config = WsServerConfig::new("127.0.0.1:8788".parse().unwrap());
let handle = spawn_managed_ws_server_task(config, Factory, WsServerRetryPolicy::default())
    .expect("spawn");
handle
    .wait_for_status(ManagedWsServerStatus::Listening, Duration::from_secs(5))
    .await
    .expect("listening");

// One serialization, fanned out to every matching connection.
let delivered = handle.dispatch(Feed(Bytes::from_static(b"tick"))).unwrap();
println!("delivered to {delivered} clients");

handle.shutdown_with_timeout(Duration::from_secs(5)).await.unwrap();
# }
```

### Lifecycle and status

`ManagedWsServerStatus` progresses `Starting → (Retrying ⇄) Listening →
ShuttingDown → Stopped`, or to `Terminal` on unrecoverable failure. **`Stopped`
and `Terminal` are disjoint**: a normal shutdown can never present as a terminal
failure, so a supervisor may treat `Terminal` as an unambiguous replace signal
(`terminal_error()` retains the typed cause for the life of the handle). An
occupied address is `Retrying`, not a failure — the server rebinds with capped
exponential backoff (100 ms → 5 s) when the address frees up.

### Overload behaviour

- `max_connections` is a **total budget for the server instance**, not per-IP; a
  connection beyond it is closed immediately rather than queued.
- `retained_commands_per_client` bounds fanout. A client that falls further
  behind is closed and must reconnect for an authoritative snapshot — stale
  frames are never queued on a slow client's behalf.
- Commands are cloned once per connection, so payloads should be
  reference-counted (`bytes::Bytes`): a payload is serialized once regardless of
  connection count, and fanout copies refcounts, not bytes. The
  `managed_server_fanout` Criterion benchmark measures this at 1/64/256 clients.

### Cleanup

`shutdown_with_timeout` closes the listener and drains clients within a budget,
aborting and reporting a timeout if a connection cannot drain in time. Dropping
the handle aborts the server task, which releases the listener and every
connection task — the final guarantee that no transport outlives its owner.

### Test-support boundary

The `test-support` feature adds `spawn_managed_ws_server_task_with_faults` for
deterministic accept-loop terminal-fault injection. It is strictly additive and
off by default: **the production API carries no fault hook.**

## Documentation

- Architecture: [`docs/architecture/architecture.md`](docs/architecture/architecture.md)
- Reconnection guide:
  [`docs/architecture/reconnection_guide.md`](docs/architecture/reconnection_guide.md)

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
