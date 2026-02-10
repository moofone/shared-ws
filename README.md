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

## Documentation

- Architecture: [`docs/architecture/architecture.md`](docs/architecture/architecture.md)

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
