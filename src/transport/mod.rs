use std::future::Future;
use std::pin::Pin;

use futures_util::{Sink, Stream};

use crate::core::{WebSocketBufferConfig, WebSocketError, WsFrame, WsTlsConfig};

pub mod tungstenite;

/// Transport boundary for websocket IO.
///
/// The IO loop is expected to live outside kameo; the actor owns state and policies.
///
/// This trait is intentionally minimal so different websocket implementations can be swapped
/// (tokio-tungstenite vs fastwebsockets) while keeping protocol/state logic unchanged.
pub trait WsTransport: Clone + Send + Sync + 'static {
    type Reader: Stream<Item = Result<WsFrame, WebSocketError>> + Send + Unpin + 'static;
    type Writer: Sink<WsFrame, Error = WebSocketError> + Send + Sync + Unpin + 'static;

    fn connect(
        &self,
        url: String,
        buffers: WebSocketBufferConfig,
        tls: WsTlsConfig,
    ) -> Pin<Box<dyn Future<Output = Result<(Self::Reader, Self::Writer), WebSocketError>> + Send>>;
}
