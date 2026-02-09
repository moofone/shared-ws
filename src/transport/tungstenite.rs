use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{Future, Sink, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream,
    connect_async_tls_with_config as tungstenite_connect_tls,
    connect_async_with_config as tungstenite_connect_with_config,
    tungstenite::{
        Message as TungsteniteMessage, Utf8Bytes,
        protocol::{CloseFrame as TungCloseFrame, WebSocketConfig},
    },
};

use crate::core::{WebSocketBufferConfig, WebSocketError, WsCloseFrame, WsFrame, WsText};
use crate::tls::install_rustls_crypto_provider;
use crate::transport::WsTransport;

fn map_ws_error(context: &'static str, err: impl ToString) -> WebSocketError {
    WebSocketError::TransportError {
        context,
        error: err.to_string(),
    }
}

fn close_to_core(frame: Option<TungCloseFrame>) -> Option<WsCloseFrame> {
    frame.map(|f| WsCloseFrame {
        code: u16::from(f.code),
        // Avoid a refcount bump / clone: we already own the close frame.
        reason: f.reason.into(),
    })
}

fn core_to_close(frame: WsCloseFrame) -> TungCloseFrame {
    let reason = match std::str::from_utf8(frame.reason.as_ref()) {
        Ok(_) => unsafe { Utf8Bytes::from_bytes_unchecked(frame.reason) },
        Err(_) => Utf8Bytes::from_static(""),
    };
    TungCloseFrame {
        code: frame.code.into(),
        reason,
    }
}

fn msg_to_frame(msg: TungsteniteMessage) -> Result<WsFrame, WebSocketError> {
    match msg {
        // Avoid a refcount bump / clone: we already own the message.
        TungsteniteMessage::Text(text) => {
            // SAFETY: tungstenite `Text` payloads are validated UTF-8.
            Ok(WsFrame::Text(unsafe {
                WsText::from_bytes_unchecked(text.into())
            }))
        }
        TungsteniteMessage::Binary(bytes) => Ok(WsFrame::Binary(bytes)),
        TungsteniteMessage::Ping(bytes) => Ok(WsFrame::Ping(bytes)),
        TungsteniteMessage::Pong(bytes) => Ok(WsFrame::Pong(bytes)),
        TungsteniteMessage::Close(frame) => Ok(WsFrame::Close(close_to_core(frame))),
        TungsteniteMessage::Frame(_) => Err(map_ws_error("read", "unexpected raw frame")),
    }
}

fn frame_to_msg(frame: WsFrame) -> TungsteniteMessage {
    match frame {
        WsFrame::Text(text) => {
            let bytes = text.into_bytes();
            // SAFETY: `WsText` is UTF-8 validated by construction.
            TungsteniteMessage::Text(unsafe { Utf8Bytes::from_bytes_unchecked(bytes) })
        }
        WsFrame::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        WsFrame::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        WsFrame::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        WsFrame::Close(frame) => TungsteniteMessage::Close(frame.map(core_to_close)),
    }
}

fn build_ws_config(buffers: WebSocketBufferConfig) -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    // Transport-level limits must reflect the configured transport limits; this is part of the
    // public safety surface (DoS caps) and must not be accidentally skipped.
    config.max_message_size = Some(buffers.max_message_bytes);
    config.max_frame_size = Some(buffers.max_frame_bytes);
    config.write_buffer_size = buffers.write_buffer_bytes;
    config.max_write_buffer_size = buffers.max_write_buffer_bytes;
    config
}

#[derive(Clone, Default)]
pub struct TungsteniteTransport {
    connector: Option<Connector>,
}

impl TungsteniteTransport {
    pub fn with_connector(connector: Connector) -> Self {
        Self {
            connector: Some(connector),
        }
    }

    pub fn rustls(config: Arc<rustls::ClientConfig>) -> Self {
        Self::with_connector(Connector::Rustls(config))
    }
}

pub struct TungsteniteReader {
    inner: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl Stream for TungsteniteReader {
    type Item = Result<WsFrame, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(Some(msg_to_frame(msg))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(map_ws_error("read", err)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct TungsteniteWriter {
    inner: futures_util::stream::SplitSink<
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        TungsteniteMessage,
    >,
}

impl Sink<WsFrame> for TungsteniteWriter {
    type Error = WebSocketError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_ready(cx)
            .map_err(|e| map_ws_error("write", e))
    }

    fn start_send(mut self: Pin<&mut Self>, item: WsFrame) -> Result<(), Self::Error> {
        Pin::new(&mut self.inner)
            .start_send(frame_to_msg(item))
            .map_err(|e| map_ws_error("write", e))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_flush(cx)
            .map_err(|e| map_ws_error("write", e))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_close(cx)
            .map_err(|e| map_ws_error("write", e))
    }
}

impl WsTransport for TungsteniteTransport {
    type Reader = TungsteniteReader;
    type Writer = TungsteniteWriter;

    fn connect(
        &self,
        url: String,
        buffers: WebSocketBufferConfig,
    ) -> Pin<Box<dyn Future<Output = Result<(Self::Reader, Self::Writer), WebSocketError>> + Send>>
    {
        let connector = self.connector.clone();
        Box::pin(async move {
            install_rustls_crypto_provider();

            let config = build_ws_config(buffers);

            let (stream, _) = match connector {
                Some(connector) => {
                    tungstenite_connect_tls(url.clone(), Some(config), false, Some(connector))
                        .await
                        .map_err(|e| map_ws_error("connect", e))?
                }
                None => tungstenite_connect_with_config(url, Some(config), false)
                    .await
                    .map_err(|e| map_ws_error("connect", e))?,
            };

            let (write, read) = stream.split();
            Ok((
                TungsteniteReader { inner: read },
                TungsteniteWriter { inner: write },
            ))
        })
    }
}

/// Accept an incoming websocket connection and return a client wrapper used by tests/examples.
pub async fn accept_async(stream: TcpStream) -> Result<crate::client::WsClient, WebSocketError> {
    crate::client::accept_async(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tungstenite_limits_reflect_buffer_config() {
        let buffers = WebSocketBufferConfig {
            max_message_bytes: 2 << 20,
            max_frame_bytes: 3 << 20,
            write_buffer_bytes: 111,
            max_write_buffer_bytes: 222,
        };

        let cfg = build_ws_config(buffers);
        assert_eq!(cfg.max_message_size, Some(2 << 20));
        assert_eq!(cfg.max_frame_size, Some(3 << 20));
        assert_eq!(cfg.write_buffer_size, 111);
        assert_eq!(cfg.max_write_buffer_size, 222);
    }
}
