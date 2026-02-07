//! Kameo-based WebSocket infrastructure.

pub mod core;
pub mod transport;
pub mod client;
pub mod supervision;
pub mod tls;
pub mod ws;

pub use ws::{
    WriterWrite, WriterWriteBatch, WsMessage, WsWriterActor, spawn_writer_supervisor,
    spawn_writer_supervised_with,
};
