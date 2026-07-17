//! Generic managed WebSocket **server**.
//!
//! This module owns every server-side transport primitive so applications never
//! touch one: `SO_REUSEADDR`, bind and backlog, the accept/rebind loop, the
//! handshake timeout, WebSocket limits, the connection cap, connection-task
//! reaping, ping/pong, close, malformed handshakes, command fanout, and
//! shutdown.
//!
//! # Shape
//!
//! [`spawn_managed_ws_server_task`] takes a [`WsServerConfig`], a
//! [`WsServerEndpointFactory`], and a [`WsServerRetryPolicy`], and returns a
//! [`ManagedWsServerHandle`]. The factory mints one concrete
//! [`WsServerEndpointHandler`] per accepted connection. Dispatch is static
//! throughout: no `dyn`, no `async_trait`, no boxed handler on the event path.
//!
//! ```text
//! application ──dispatch(command)──▶ handle ──fanout──▶ endpoint handlers ──▶ clients
//!                                                  ▲
//!                          factory.create_endpoint()│ (one per connection)
//! ```
//!
//! # Lifecycle
//!
//! [`ManagedWsServerStatus`] moves `Starting` → (`Retrying` ⇄) `Listening` →
//! `ShuttingDown` → `Stopped`, or to `Terminal` on unrecoverable failure.
//! `Stopped` and `Terminal` are mutually exclusive: **a normal shutdown can
//! never present as a terminal failure**, so a supervisor can treat `Terminal`
//! as an unambiguous signal to replace the owner. An occupied address is
//! `Retrying`, not a failure — the server rebinds when the address frees up.
//!
//! Terminal causes are retained on the handle
//! ([`ManagedWsServerHandle::terminal_error`]) for its whole life, so an
//! observer that looks late still learns why.
//!
//! # Overload
//!
//! `max_connections` is a **total budget for the server instance**, not a
//! per-IP limit; a connection beyond it is closed immediately rather than
//! queued. `retained_commands_per_client` bounds fanout: a client that falls
//! further behind is closed and must reconnect for an authoritative snapshot.
//! Stale frames are never queued on a slow client's behalf. Commands are cloned
//! once per connection, so payloads should be reference-counted ([`bytes::Bytes`])
//! — fanout then copies refcounts, not bytes, and a payload is serialized once
//! regardless of connection count.
//!
//! # Cleanup
//!
//! [`ManagedWsServerHandle::shutdown_with_timeout`] closes the listener and
//! drains clients within a budget. Dropping the handle aborts the server task,
//! which releases the listener and every connection task — the final guarantee
//! that no transport outlives its owner.
//!
//! # Test support
//!
//! The `test-support` feature adds
//! [`spawn_managed_ws_server_task_with_faults`] for deterministic accept-loop
//! terminal-fault injection. It is strictly additive and off by default: the
//! production API carries no fault hook.

mod config;
mod handle;
mod runtime;
#[cfg(test)]
mod tests;
mod types;

pub use config::{WsServerConfig, WsServerRetryPolicy};
pub use handle::ManagedWsServerHandle;
pub use runtime::spawn_managed_ws_server_task;
pub use types::{
    ManagedWsServerDispatchError, ManagedWsServerShutdownError, ManagedWsServerStartError,
    ManagedWsServerStatus, ManagedWsServerStatusError, ManagedWsServerTerminalError,
    WS_SERVER_FRAME_BATCH_CAPACITY, WsServerCloseCause, WsServerCommandOf,
    WsServerConnectionContext, WsServerEndpointAction, WsServerEndpointEvent,
    WsServerEndpointFactory, WsServerEndpointHandler, WsServerFrameBatch,
};

#[cfg(feature = "test-support")]
pub use runtime::{WsServerTestFaults, spawn_managed_ws_server_task_with_faults};
