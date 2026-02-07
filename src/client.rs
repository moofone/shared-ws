use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, accept_async as tungstenite_accept,
    connect_async as tungstenite_connect, connect_async_tls_with_config as tungstenite_connect_tls,
    tungstenite::protocol::WebSocketConfig,
    tungstenite::{Message as TungsteniteMessage, Utf8Bytes, protocol::CloseFrame as TungCloseFrame},
};

use crate::tls::install_rustls_crypto_provider;
use crate::core::{WebSocketError, WsCloseFrame, WsFrame};

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
        WsFrame::Text(bytes) => {
            // Bulletproof: avoid UB even if callers misuse WsFrame::Text.
            match std::str::from_utf8(bytes.as_ref()) {
                Ok(_) => TungsteniteMessage::Text(unsafe { Utf8Bytes::from_bytes_unchecked(bytes) }),
                Err(_) => TungsteniteMessage::Binary(bytes),
            }
        }
        WsFrame::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        WsFrame::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        WsFrame::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        WsFrame::Close(frame) => TungsteniteMessage::Close(frame.map(core_to_close)),
    }
}

/// Connector wrapper to avoid exposing tokio-tungstenite types outside this crate.
#[derive(Clone)]
pub struct WsConnector {
    inner: Connector,
}

impl WsConnector {
    pub fn rustls(config: Arc<rustls::ClientConfig>) -> Self {
        Self {
            inner: Connector::Rustls(config),
        }
    }
}

/// Connection configuration for websocket clients.
#[derive(Clone, Copy, Debug)]
pub struct WsConnectConfig {
    pub max_message_size: Option<usize>,
    pub max_frame_size: Option<usize>,
    pub write_buffer_size: usize,
    pub max_write_buffer_size: usize,
}

impl Default for WsConnectConfig {
    fn default() -> Self {
        Self {
            max_message_size: Some(16 * 1024 * 1024),
            max_frame_size: Some(16 * 1024 * 1024),
            write_buffer_size: 128 << 10,
            max_write_buffer_size: 256 << 10,
        }
    }
}

impl From<WsConnectConfig> for WebSocketConfig {
    fn from(cfg: WsConnectConfig) -> Self {
        WebSocketConfig::default()
            .max_message_size(cfg.max_message_size)
            .max_frame_size(cfg.max_frame_size)
            .write_buffer_size(cfg.write_buffer_size)
            .max_write_buffer_size(cfg.max_write_buffer_size)
    }
}

/// Thin wrapper around a websocket stream that hides tungstenite types.
pub struct WsClient {
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsClient {
    pub async fn send(&mut self, msg: WsFrame) -> Result<(), WebSocketError> {
        self.inner
            .send(frame_to_msg(msg))
            .await
            .map_err(|e| map_ws_error("write", e))
    }

    pub async fn next(&mut self) -> Option<Result<WsFrame, WebSocketError>> {
        self.inner
            .next()
            .await
            .map(|res| res.map(msg_to_frame).map_err(|e| map_ws_error("read", e)))
    }
}

impl Stream for WsClient {
    type Item = Result<WsFrame, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        match inner.poll_next(cx) {
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(Some(Ok(msg_to_frame(msg)))),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(map_ws_error("read", err)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<WsFrame> for WsClient {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll_ready(cx).map_err(|e| map_ws_error("write", e))
    }

    fn start_send(self: Pin<&mut Self>, item: WsFrame) -> Result<(), Self::Error> {
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner
            .start_send(frame_to_msg(item))
            .map_err(|e| map_ws_error("write", e))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll_flush(cx).map_err(|e| map_ws_error("write", e))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll_close(cx).map_err(|e| map_ws_error("write", e))
    }
}

/// Connect to a websocket URL using default configuration.
pub async fn connect_async(url: impl AsRef<str>) -> Result<WsClient, WebSocketError> {
    install_rustls_crypto_provider();
    let (stream, _) = tungstenite_connect(url.as_ref())
        .await
        .map_err(|err| WebSocketError::ConnectionFailed(err.to_string()))?;
    Ok(WsClient { inner: stream })
}

/// Connect using a pre-built HTTP request (useful for custom headers).
pub async fn connect_request(request: http::Request<()>) -> Result<WsClient, WebSocketError> {
    install_rustls_crypto_provider();
    let (stream, _) = tungstenite_connect(request)
        .await
        .map_err(|err| WebSocketError::ConnectionFailed(err.to_string()))?;
    Ok(WsClient { inner: stream })
}

/// Connect using TLS configuration and optional connector.
pub async fn connect_async_tls_with_config(
    url: impl AsRef<str>,
    config: Option<WsConnectConfig>,
    disable_tls_validation: bool,
    connector: Option<WsConnector>,
) -> Result<WsClient, WebSocketError> {
    install_rustls_crypto_provider();
    let config = config.map(WebSocketConfig::from);
    let connector = connector.map(|c| c.inner);
    let (stream, _) =
        tungstenite_connect_tls(url.as_ref(), config, disable_tls_validation, connector)
            .await
            .map_err(|err| WebSocketError::ConnectionFailed(err.to_string()))?;
    Ok(WsClient { inner: stream })
}

/// Accept an incoming websocket connection.
pub async fn accept_async(stream: TcpStream) -> Result<WsClient, WebSocketError> {
    let ws = tungstenite_accept(MaybeTlsStream::Plain(stream))
        .await
        .map_err(|err| WebSocketError::ConnectionFailed(err.to_string()))?;
    Ok(WsClient { inner: ws })
}
