# shared-ws (crate: defi-ws)

High-performance, transport-swappable WebSocket library built on Kameo actors.

## Design

- IO loop runs outside Kameo for throughput; frames are injected into the actor.
- Transport-neutral frame type: `WsFrame`.
- Transport boundary: `WsTransport` (currently `TungsteniteTransport`).
- Self-healing reconnect logic with subscription + handler state preserved across reconnects.

## Crate Layout

- `src/core/`: transport-neutral types/policies (`WsFrame`, health, ping/pong, rate-limit).
- `src/transport/`: transport boundary + implementations.
- `src/ws/`: Kameo actor + writer actor.

## Quick Start

See `examples/basic_connect.rs`.

## Tests

- `tests/state_recovery.rs`: reconnect + subscription-state invariants.
- `tests/zero_copy.rs`: zero-copy inbound parsing.
