//! Kameo-based WebSocket infrastructure.

pub mod client;
pub mod core;
pub mod supervision;
pub mod tls;
pub mod transport;
pub mod ws;

pub use ws::{
    WriterWrite, WriterWriteBatch, WsMessage, WsWriterActor, spawn_writer_supervised_with,
    spawn_writer_supervisor,
};
