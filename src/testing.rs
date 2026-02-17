//! Reusable test utilities for exercising the websocket actor without a real socket.
//!
//! This module is intended for integration tests in downstream crates that need to drive
//! `WebSocketActor` deterministically (including server-side socket drops).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Sink;
use tokio::sync::{Mutex, mpsc};

use crate::transport::{WsTransport, WsTransportConnectFuture};
use crate::ws::{
    WebSocketBufferConfig, WebSocketError, WsDisconnectAction, WsDisconnectCause,
    WsEndpointHandler, WsErrorAction, WsFrame, WsMessageAction, WsParseOutcome,
    WsReconnectStrategy, WsRequestMatch, WsSubscriptionAction, WsSubscriptionManager,
    WsSubscriptionStatus, frame_bytes, into_ws_message,
};

/// A transport that uses in-memory channels so tests can emulate server behavior.
///
/// Create it with [`MockTransport::channel_pair`] to obtain both:
/// - the transport for `WebSocketActor`
/// - a [`MockServer`] handle used by tests to receive outbound frames, push inbound frames,
///   or drop the socket.
#[derive(Clone)]
pub struct MockTransport {
    sent_tx: mpsc::UnboundedSender<WsFrame>,
    inbound_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<WsFrame>>>>,
}

impl MockTransport {
    /// Build a transport + server control pair.
    pub fn channel_pair() -> (Self, MockServer) {
        let (sent_tx, sent_rx) = mpsc::unbounded_channel::<WsFrame>();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<WsFrame>();
        (
            Self {
                sent_tx,
                inbound_rx: Arc::new(Mutex::new(Some(inbound_rx))),
            },
            MockServer {
                outbound_rx: sent_rx,
                inbound_tx: Some(inbound_tx),
            },
        )
    }
}

impl WsTransport for MockTransport {
    type Reader = MockReader;
    type Writer = MockWriter;

    fn connect(
        &self,
        _url: String,
        _buffers: WebSocketBufferConfig,
    ) -> WsTransportConnectFuture<Self::Reader, Self::Writer> {
        let sent_tx = self.sent_tx.clone();
        let inbound_rx = Arc::clone(&self.inbound_rx);
        Box::pin(async move {
            let rx = inbound_rx.lock().await.take().ok_or_else(|| {
                WebSocketError::InvalidState(
                    "mock transport only supports a single active connection".to_string(),
                )
            })?;
            Ok((MockReader { rx }, MockWriter { sent_tx }))
        })
    }
}

/// Error surface for operations on [`MockServer`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MockServerError {
    /// The inbound socket side was intentionally dropped.
    SocketDropped,
    /// The actor side is no longer receiving inbound frames.
    ChannelClosed,
}

impl std::fmt::Display for MockServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MockServerError::SocketDropped => f.write_str("mock socket already dropped"),
            MockServerError::ChannelClosed => f.write_str("mock actor channel is closed"),
        }
    }
}

impl std::error::Error for MockServerError {}

/// Server-side test handle paired with [`MockTransport`].
pub struct MockServer {
    outbound_rx: mpsc::UnboundedReceiver<WsFrame>,
    inbound_tx: Option<mpsc::UnboundedSender<WsFrame>>,
}

impl MockServer {
    /// Receive a frame written by the actor to the websocket writer.
    pub async fn recv_outbound(&mut self) -> Option<WsFrame> {
        self.outbound_rx.recv().await
    }

    /// Receive a frame with a timeout.
    pub async fn recv_outbound_timeout(&mut self, timeout: Duration) -> Option<WsFrame> {
        tokio::time::timeout(timeout, self.outbound_rx.recv())
            .await
            .unwrap_or_default()
    }

    /// Push an inbound frame to the actor.
    pub fn send_inbound(&self, frame: WsFrame) -> Result<(), MockServerError> {
        let Some(tx) = self.inbound_tx.as_ref() else {
            return Err(MockServerError::SocketDropped);
        };
        tx.send(frame).map_err(|_| MockServerError::ChannelClosed)
    }

    /// Push a UTF-8 payload as websocket text.
    pub fn send_text(&self, text: impl AsRef<str>) -> Result<(), MockServerError> {
        self.send_inbound(into_ws_message(text.as_ref().as_bytes().to_vec()))
    }

    /// Simulate server-side socket drop by closing the inbound channel.
    pub fn drop_socket(&mut self) {
        self.inbound_tx = None;
    }
}

/// Reader side for [`MockTransport`].
pub struct MockReader {
    rx: mpsc::UnboundedReceiver<WsFrame>,
}

impl futures_util::Stream for MockReader {
    type Item = Result<WsFrame, WebSocketError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.rx).poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Writer side for [`MockTransport`].
pub struct MockWriter {
    sent_tx: mpsc::UnboundedSender<WsFrame>,
}

impl Sink<WsFrame> for MockWriter {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsFrame) -> Result<(), Self::Error> {
        self.get_mut()
            .sent_tx
            .send(item)
            .map_err(|_| WebSocketError::TransportError {
                context: "mock_transport_write",
                error: "mock outbound channel closed".to_string(),
            })
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Reconnect strategy that never retries.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoReconnect;

impl WsReconnectStrategy for NoReconnect {
    fn next_delay(&mut self) -> Duration {
        Duration::from_secs(24 * 60 * 60)
    }

    fn reset(&mut self) {}

    fn should_retry(&self) -> bool {
        false
    }
}

/// Empty subscription manager for tests that don't use subscriptions.
#[derive(Debug, Clone, Default)]
pub struct NoSubscriptions;

impl WsSubscriptionManager for NoSubscriptions {
    type SubscriptionMessage = ();

    fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage> {
        Vec::new()
    }

    fn update_subscriptions(
        &mut self,
        _action: WsSubscriptionAction<Self::SubscriptionMessage>,
    ) -> Result<Vec<Self::SubscriptionMessage>, String> {
        Ok(Vec::new())
    }

    fn serialize_subscription(&mut self, _msg: &Self::SubscriptionMessage) -> Vec<u8> {
        Vec::new()
    }

    fn handle_subscription_response(&mut self, _data: &[u8]) -> WsSubscriptionStatus {
        WsSubscriptionStatus::NotSubscriptionResponse
    }
}

/// Minimal JSON-RPC endpoint for delegated request confirmation tests.
///
/// Matching behavior:
/// - requires `"id"` and either `"result"` or `"error"`
/// - `"result"` maps to `Ok(())`
/// - `"error"` maps to `Err("endpoint rejected")`
#[derive(Clone, Default)]
pub struct JsonRpcDelegatedEndpoint {
    subscriptions: NoSubscriptions,
}

impl WsEndpointHandler for JsonRpcDelegatedEndpoint {
    type Message = ();
    type Error = std::io::Error;
    type Subscription = NoSubscriptions;

    fn subscription_manager(&mut self) -> &mut Self::Subscription {
        &mut self.subscriptions
    }

    fn generate_auth(&self) -> Option<Vec<u8>> {
        None
    }

    fn parse(&mut self, _data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error> {
        Ok(WsParseOutcome::Message(WsMessageAction::Continue))
    }

    fn maybe_request_response(&self, data: &[u8]) -> bool {
        memmem(data, b"\"id\"") && (memmem(data, b"\"result\"") || memmem(data, b"\"error\""))
    }

    fn match_request_response(&mut self, data: &[u8]) -> Option<WsRequestMatch> {
        let request_id = parse_jsonrpc_id(data)?;
        if memmem(data, b"\"error\"") {
            return Some(WsRequestMatch {
                request_id,
                result: Err("endpoint rejected".to_string()),
                rate_limit_feedback: None,
            });
        }
        if memmem(data, b"\"result\"") {
            return Some(WsRequestMatch {
                request_id,
                result: Ok(()),
                rate_limit_feedback: None,
            });
        }
        None
    }

    fn handle_message(&mut self, _msg: Self::Message) -> Result<(), Self::Error> {
        Ok(())
    }

    fn handle_server_error(
        &mut self,
        _code: Option<i32>,
        _message: &str,
        _data: Option<sonic_rs::Value>,
    ) -> WsErrorAction {
        WsErrorAction::Continue
    }

    fn reset_state(&mut self) {}

    fn classify_disconnect(&self, _cause: &WsDisconnectCause) -> WsDisconnectAction {
        WsDisconnectAction::Abort
    }
}

fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn parse_jsonrpc_id(data: &[u8]) -> Option<u64> {
    let mut i = 0usize;
    while i + 4 < data.len() {
        if data[i] == b'"'
            && data.get(i + 1) == Some(&b'i')
            && data.get(i + 2) == Some(&b'd')
            && data.get(i + 3) == Some(&b'"')
        {
            i += 4;
            while i < data.len() && is_ws(data[i]) {
                i += 1;
            }
            if i >= data.len() || data[i] != b':' {
                continue;
            }
            i += 1;
            while i < data.len() && is_ws(data[i]) {
                i += 1;
            }
            let start = i;
            while i < data.len() && data[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                return None;
            }
            return std::str::from_utf8(&data[start..i]).ok()?.parse().ok();
        }
        i += 1;
    }
    None
}

#[inline]
fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t')
}

/// Utility helper for tests validating JSON-RPC ids on outbound frames.
pub fn frame_jsonrpc_id(frame: &WsFrame) -> Option<u64> {
    parse_jsonrpc_id(frame_bytes(frame)?)
}
