# shared-ws Integration Guide

This doc describes how to integrate `shared-ws` into an application: which traits to implement,
how to wire subscriptions/auth/parse, how to do health checks, and where retries/rate limiting
belong.

For design/background, see `docs/architecture/architecture.md`.

For more complete, runnable integrations, see `examples/e2e/`.

## Mental Model (What You Provide vs What shared-ws Provides)

`shared-ws` provides:

- `WebSocketActor<E, R, P, I, T>`: a kameo actor that owns connection lifecycle + policies.
- A reader task (outside kameo) that runs ingress decode and forwards minimal events into the actor.
- A supervised `WsWriterActor` that owns the transport writer.
- Health stats via `WsHealthMonitor` (exposed through ask-able messages).
- Delegated requests (`ask(WsDelegatedRequest)`) for “send + await sent/confirmed/rejected/timeout”.

You provide (pluggable traits):

- `E: WsEndpointHandler`: endpoint-specific protocol logic and state.
- `E::Subscription: WsSubscriptionManager`: how to build/track subscriptions.
- `I: WsIngress`: optional tight-loop ingress decode/filter to reduce actor mailbox load.
- `P: WsPingPongStrategy`: protocol ping/pong or application-level heartbeat.
- `R: WsReconnectStrategy`: backoff policy after disconnects.
- `T: WsTransport`: IO implementation (default is tokio-tungstenite).

## Quickstart (Minimal Skeleton)

```rust
use std::time::Duration;

use shared_ws::transport::tungstenite::TungsteniteTransport;
use shared_ws::ws::{
    ExponentialBackoffReconnect, ForwardAllIngress, ProtocolPingPong, WebSocketActor,
    WebSocketActorArgs, WebSocketBufferConfig, WebSocketEvent,
};

// 1) Implement WsSubscriptionManager + WsEndpointHandler (see below).
// 2) Pick ingress/ping/reconnect/transport.
let actor = WebSocketActor::spawn(WebSocketActorArgs {
    url: "wss://example".to_string(),
    transport: TungsteniteTransport::default(),
    reconnect_strategy: ExponentialBackoffReconnect::default(),
    handler: /* your handler */ todo!(),
    ping_strategy: ProtocolPingPong::new(Duration::from_secs(15), Duration::from_secs(10)),
    ingress: ForwardAllIngress::default(),
    enable_ping: true,
    stale_threshold: Duration::from_secs(30),
    ws_buffers: WebSocketBufferConfig::default(),
    outbound_capacity: 1024,
    circuit_breaker: None,
    latency_policy: None,
    payload_latency_sampling: None,
    registration: None,
    metrics: None,
});

// 3) Start the connection loop.
actor.tell(WebSocketEvent::Connect).send().await?;
```

Notes:

- `spawn(...)` constructs the actor but does not auto-connect; you must `tell(WebSocketEvent::Connect)`.
- If you want other actors to discover this websocket actor, set `registration` (see “Actor naming”).

## Required Traits

### `WsSubscriptionManager`

This trait defines how the actor computes (and re-computes) desired subscription frames.

Required methods:

- `initial_subscriptions() -> Vec<SubscriptionMessage>`
- `update_subscriptions(action: WsSubscriptionAction<SubscriptionMessage>) -> Result<Vec<SubscriptionMessage>, String>`
  - Return the concrete subscription messages that should be sent immediately (may be empty).
- `serialize_subscription(&SubscriptionMessage) -> Vec<u8>`
- `handle_subscription_response(&[u8]) -> WsSubscriptionStatus`

Optional fast-path:

- `maybe_subscription_response(&[u8]) -> bool` (default `true`)
  - Use this to skip expensive “is this an ACK?” parsing on high-volume feeds.

### `WsEndpointHandler`

This trait is the endpoint adapter. It owns auth state, protocol decode, error classification, and
the subscription manager instance.

Required methods (most integrations start here):

- `subscription_manager(&mut self) -> &mut Self::Subscription`
- `generate_auth(&self) -> Option<Vec<u8>>`
- `parse(&mut self, data: &[u8]) -> Result<WsParseOutcome<Message>, Error>`
  - You can also override `parse_text(&str)` or `parse_frame(&WsFrame)` for better perf/ergonomics.
- `handle_message(&mut self, msg: Message) -> Result<(), Error>`
- `handle_server_error(code, message, data) -> WsErrorAction`
  - Decide whether server error frames are fatal/reconnect/continue.
- `reset_state(&mut self)`
  - Called when the actor resets after a disconnect.
- `classify_disconnect(&self, cause: &WsDisconnectCause) -> WsDisconnectAction`
  - Decide: `ImmediateReconnect` | `BackoffReconnect` | `Abort`.

Optional hooks:

- `on_open(&mut self)` called after a successful connection is established.
- `session_mode(&self) -> WsSessionMode`
  - `Public` (default): replay on open/reconnect.
  - `AuthGated`: defer replay until `WsSetAuthenticated { authenticated: true }`.
- `on_connection_opened(&mut self, is_reconnect: bool) -> Option<Message>`
  - one-shot open/reconnect signal back to your owner.
- `sample_payload_timestamps_us(&mut self, data, emit)` for low-frequency lag metrics (see “Metrics”).

## Pluggable Ingress (Mailbox Load Control)

`WsIngress` runs in the reader task (outside kameo) and can:

- Drop frames (`WsIngressAction::Ignore`)
- Emit compact application events (`Emit`/`EmitBatch`)
- Forward raw frames into the actor (`Forward`)
- Request reconnect/shutdown (`Reconnect`/`Shutdown`)

If you do not need this, use `ForwardAllIngress` to forward every frame.

## Pluggable Ping/Pong

Provide `P: WsPingPongStrategy`:

- `ProtocolPingPong`: uses websocket control frames (`Ping`/`Pong`).
- `WsApplicationPingPong`: tracks application-level ping requests by key.

The actor will:

- periodically request a ping (`SendPing`) when `enable_ping = true`
- disconnect if `ping.is_stale()` or if no inbound activity occurs for `stale_threshold`

## Retries and Reconnect (Where They Live)

Connection retries:

- Endpoint decides what to do after a disconnect via `WsEndpointHandler::classify_disconnect(...)`.
- Backoff timing comes from `R: WsReconnectStrategy` (use `ExponentialBackoffReconnect` or your own).
- Optional `WsCircuitBreaker` can suppress connection attempts after repeated failures.
- For delegated-auth endpoints, use `WsSessionMode::AuthGated` + `WsSetAuthenticated` to keep
  replay ordering deterministic (`open -> auth -> replay`).

Request retries:

- `shared-ws` does not implement endpoint-specific request retries.
- For “send + await outcome” calls, build retries at the call site around `ws.ask(WsDelegatedRequest)`,
  and ensure your endpoint supports idempotency (typically: stable `request_id` + stable payload).

## Delegated Requests (Send + Await Sent/Confirmed)

Use `ask(WsDelegatedRequest { ... })` when you need an application-visible outcome.

You must implement confirmation matching in your handler:

- `maybe_request_response(&[u8]) -> bool` (fast precheck; default `false`)
- `match_request_response(&[u8]) -> Option<WsRequestMatch>`
  - Return `{ request_id, result: Ok(()) | Err(String), rate_limit_feedback }`

Key fields:

- `request_id`: correlation id (endpoint-defined; often JSON-RPC `id`).
- `fingerprint`: cheap hash of the payload used to dedupe concurrent calls.
- `confirm_deadline`: if `confirm_mode = Confirmed`, the actor returns `Unconfirmed` after this.
- `confirm_mode`:
  - `Sent`: completes when the writer accepts the frame for writing.
  - `Confirmed`: completes only after a match is observed (or times out).

Outcomes:

- `WsDelegatedOk { confirmed, rate_limit_feedback }`
- `WsDelegatedError::{NotDelivered, Unconfirmed, PayloadMismatch, TooManyPending, EndpointRejected{..}}`

`rate_limit_feedback` is informational only; `shared-ws` does not apply rate limiting policy.

## Rate Limiting (External Coordinator)

Important: `shared-ws` is rate-limiter agnostic.

If you need outbound throttling:

- put an external coordinator in front of `ws.ask(WsDelegatedRequest { .. })`
- acquire quota immediately before sending, then commit/refund based on the outcome

See `examples/e2e/src/coordinator.rs` for a full pattern using `shared-rate_limiter`.

### “Rate limiter actor name”

There is no rate limiter actor inside this crate.

If you implement your coordinator as a kameo actor and want discovery-by-name:

- register the websocket actor via `WebSocketActorArgs.registration = Some(WsActorRegistration::new("..."))`
- choose a stable application-level name for your coordinator (for example `"rate_limiter"`), and
  keep it consistent across deployments

## Health Checks (Pluggable at the Application Boundary)

`shared-ws` exposes health primitives; you decide what “healthy” means for your service.

Ask-able messages on `WebSocketActor`:

- `GetConnectionStatus -> WsConnectionStatus` (`Connecting | Connected | Disconnected`)
- `GetConnectionStats -> WsConnectionStats` (uptime, last message age, reconnect count, error buffers, RTT percentiles)
- `WaitForWriter { timeout } -> ()` (readiness gate: actor is ready for application writes; may wait across reconnects)
- `WsSetAuthenticated { authenticated } -> ()` (auth-gated sessions only; unlock replay/sends when true)
- `WsReplaySubscriptions -> ()` (explicit on-demand replay of desired subscriptions)

Typical patterns:

- **Readiness**: `GetConnectionStatus == Connected` (or `WaitForWriter`)
- **Liveness**: process is up; optionally require “not permanently disconnected”
- **Degraded**: high `last_message_age`, frequent reconnects, or recent server/internal errors

See also: [`docs/reconnection_guide.md`](reconnection_guide.md) for deterministic reconnect patterns.

## Metrics Hook (Optional)

If you want metrics without coupling `shared-ws` to your telemetry stack:

- implement `WsMetricsReporter`
- pass `metrics: Some(Arc::new(MyReporter))` in `WebSocketActorArgs`

Optional distributed lag metrics:

- set `payload_latency_sampling: Some(WsPayloadLatencySamplingConfig { interval: ... })`
- implement `WsEndpointHandler::sample_payload_timestamps_us` to emit Unix epoch microseconds
  extracted from payloads

## Transport

Default transport:

- `TungsteniteTransport` (`crate::transport::tungstenite`)

Custom transports:

- implement `WsTransport` and provide `connect(url, buffers) -> Future<Reader, Writer>`
- `Reader` yields `WsFrame` values; `Writer` is a `Sink<WsFrame>`
