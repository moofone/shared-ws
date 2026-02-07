use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Future, Sink, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async as tungstenite_connect,
    connect_async_tls_with_config as tungstenite_connect_tls,
    tungstenite::{
        Message as TungsteniteMessage, Utf8Bytes,
        protocol::{CloseFrame as TungCloseFrame, WebSocketConfig},
    },
};

use crate::core::{WebSocketBufferConfig, WebSocketError, WsCloseFrame, WsFrame, WsTlsConfig};
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
        reason: AsRef::<Bytes>::as_ref(&f.reason).clone(),
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

fn msg_to_frame(msg: TungsteniteMessage) -> WsFrame {
    match msg {
        TungsteniteMessage::Text(text) => WsFrame::Text(AsRef::<Bytes>::as_ref(&text).clone()),
        TungsteniteMessage::Binary(bytes) => WsFrame::Binary(bytes),
        TungsteniteMessage::Ping(bytes) => WsFrame::Ping(bytes),
        TungsteniteMessage::Pong(bytes) => WsFrame::Pong(bytes),
        TungsteniteMessage::Close(frame) => WsFrame::Close(close_to_core(frame)),
        TungsteniteMessage::Frame(_) => WsFrame::Binary(Bytes::new()),
    }
}

fn frame_to_msg(frame: WsFrame) -> TungsteniteMessage {
    match frame {
        WsFrame::Text(bytes) => match std::str::from_utf8(bytes.as_ref()) {
            Ok(_) => {
                let text = unsafe { Utf8Bytes::from_bytes_unchecked(bytes) };
                TungsteniteMessage::Text(text)
            }
            Err(_) => TungsteniteMessage::Binary(bytes),
        },
        WsFrame::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        WsFrame::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        WsFrame::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        WsFrame::Close(frame) => TungsteniteMessage::Close(frame.map(core_to_close)),
    }
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
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(Some(Ok(msg_to_frame(msg)))),
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
        tls: WsTlsConfig,
    ) -> Pin<Box<dyn Future<Output = Result<(Self::Reader, Self::Writer), WebSocketError>> + Send>>
    {
        let connector = self.connector.clone();
        Box::pin(async move {
            install_rustls_crypto_provider();

            let mut config = WebSocketConfig::default();
            let max_message = buffers.max_message_bytes.max(buffers.read_buffer_bytes);
            let max_frame = buffers.max_frame_bytes.max(buffers.read_buffer_bytes);
            config.max_message_size = Some(max_message);
            config.max_frame_size = Some(max_frame);
            config.write_buffer_size = buffers.write_buffer_bytes;
            config.max_write_buffer_size = buffers.max_write_buffer_bytes;

            let disable_tls_validation = !tls.validate_certs;

            let (stream, _) = match connector {
                Some(connector) => tungstenite_connect_tls(
                    url.clone(),
                    Some(config),
                    disable_tls_validation,
                    Some(connector),
                )
                .await
                .map_err(|e| map_ws_error("connect", e))?,
                None => {
                    // For ws://, use connect_async; for wss://, connect_async_tls_with_config works too.
                    match tungstenite_connect(url.clone()).await {
                        Ok(ok) => ok,
                        Err(_) => {
                            tungstenite_connect_tls(url, Some(config), disable_tls_validation, None)
                                .await
                                .map_err(|e| map_ws_error("connect", e))?
                        }
                    }
                }
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
