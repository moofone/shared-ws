# Changelog

## 0.1.4 - 2026-02-20

- Add deterministic auth-gated session control for reconnect flows:
  - `WsSessionMode::{Public, AuthGated}` on `WsEndpointHandler`
  - `on_connection_opened(is_reconnect)` endpoint callback
  - `WsSetAuthenticated` message to unlock replay after app-level auth
  - `WsReplaySubscriptions` message for explicit on-demand replay
- Gate auto replay and incremental subscription sends while unauthenticated in auth-gated mode.
- Add mock E2E coverage for:
  - no replay before auth
  - reconnect requiring re-auth before replay
  - explicit replay after auth
- Add a dedicated reconnection guide (`docs/reconnection_guide.md`).

## 0.1.3 - 2026-02-17

- Add reusable mock testing utilities under `shared_ws::testing`:
  - `MockTransport` + `MockServer`
  - `JsonRpcDelegatedEndpoint`
  - `NoReconnect` + `NoSubscriptions`
- Add explicit server-side socket-drop simulation via `MockServer::drop_socket()`.
- Add integration coverage for delegated confirmation and socket-drop behavior using the new mock utilities.
- Document the new mock testing surface in `README.md`.

## 0.1.1 - 2026-02-09

- Make the crate package/publish-ready (metadata + dual-license files).
- Use a crates.io `kameo` dependency with an explicit version requirement.
- Remove async locking from `examples/e2e` coordinator code (lock-free example code).

## 0.1.0

- Initial release.
