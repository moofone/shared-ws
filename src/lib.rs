//! Kameo-based WebSocket infrastructure.

pub mod client;
// Internal canonical definitions. Public API is exposed via `crate::ws` (and select root re-exports).
mod core;
pub mod supervision;
pub mod testing;
pub mod tls;
pub mod transport;
pub mod ws;

pub use ws::{
    WriterWrite, WriterWriteBatch, WsMessage, WsWriterActor, spawn_writer_supervised_with,
    spawn_writer_supervisor,
};
