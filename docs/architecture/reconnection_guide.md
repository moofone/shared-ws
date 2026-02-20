# Reconnection Guide

This guide covers deterministic reconnect behavior for both public and delegated-auth websocket endpoints.

## Modes

`shared-ws` now supports two endpoint session modes:

- `WsSessionMode::Public` (default):
  - behavior: replay subscriptions immediately on socket open/reconnect
- `WsSessionMode::AuthGated`:
  - behavior: do not replay subscriptions until the owner confirms auth

Configure this in your endpoint handler:

```rust
fn session_mode(&self) -> WsSessionMode {
    WsSessionMode::AuthGated
}
```

## Open/Reconnect Signal

Use `on_connection_opened(is_reconnect)` to notify your owner actor/service when a socket opens:

```rust
fn on_connection_opened(&mut self, is_reconnect: bool) -> Option<Self::Message> {
    Some(MyEndpointMessage::ConnectionOpened { is_reconnect })
}
```

The returned message is routed through `handle_message`.

## Deterministic Auth-Gated Flow

For delegated auth (for example JSON-RPC `public/auth`), use this sequence:

1. Socket opens/reopens.
2. Endpoint receives `on_connection_opened(is_reconnect)`.
3. Owner performs app-level auth (for example via `WsDelegatedRequest`).
4. Owner sends `WsSetAuthenticated { authenticated: true }`.
5. Framework replays desired subscriptions exactly once for that connection.

If auth fails, keep the session unauthenticated and retry auth or force reconnect from owner logic.

## Control Messages

- `WsSetAuthenticated { authenticated: bool }`
  - `true`: unlocks replay/sends and triggers initial replay for auth-gated sessions
  - `false`: marks the session unauthenticated
- `WsReplaySubscriptions`
  - forces replay of desired subscriptions on demand (for example operator-triggered reconciliation)

## Public Endpoints

Public endpoints should stay on `WsSessionMode::Public` and continue to rely on automatic replay.

## Testing Recommendations

Use the mock testing utilities (`shared_ws::testing`) for deterministic reconnect tests:

- force socket drops (`MockServer::drop_socket()`)
- verify auth-gated replay ordering
- verify reconnect notifications (`is_reconnect=true`)
- verify replay only occurs after auth confirmation
