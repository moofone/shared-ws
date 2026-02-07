use bytes::Bytes;
use defi_ws::transport::tungstenite::TungsteniteTransport;
use defi_ws::ws::{
    ProtocolPingPong, WebSocketActor, WebSocketActorArgs, WebSocketBufferConfig, WebSocketEvent,
    WsDisconnectAction, WsDisconnectCause, WsErrorAction, WsEndpointHandler, WsMessage,
    WsMessageAction, WsParseOutcome, WsReconnectStrategy, WsSubscriptionAction, WsTlsConfig,
    WsSubscriptionManager, WsSubscriptionStatus,
};
use kameo::Actor;
use std::io;
use std::time::Duration;
use thiserror::Error;

#[derive(Clone, Debug)]
struct NoopSubscriptions;

impl WsSubscriptionManager for NoopSubscriptions {
    type SubscriptionMessage = Vec<u8>;

    fn initial_subscriptions(&mut self) -> Vec<Self::SubscriptionMessage> {
        Vec::new()
    }

    fn update_subscriptions(
        &mut self,
        _action: WsSubscriptionAction<Self::SubscriptionMessage>,
    ) -> Result<Vec<Self::SubscriptionMessage>, String> {
        Ok(Vec::new())
    }

    fn serialize_subscription(&mut self, msg: &Self::SubscriptionMessage) -> Vec<u8> {
        msg.clone()
    }

    fn handle_subscription_response(&mut self, _data: &[u8]) -> WsSubscriptionStatus {
        WsSubscriptionStatus::NotSubscriptionResponse
    }
}

#[derive(Clone, Debug)]
struct NoReconnect;

impl WsReconnectStrategy for NoReconnect {
    fn next_delay(&mut self) -> Duration {
        Duration::from_secs(0)
    }

    fn reset(&mut self) {}

    fn should_retry(&self) -> bool {
        false
    }
}

#[derive(Debug, Error)]
enum PtrCheckError {
    #[error("pointer mismatch")]
    PointerMismatch,
    #[error("length mismatch")]
    LengthMismatch,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
struct PtrCheckHandler {
    expected_ptr: usize,
    expected_len: usize,
    manager: NoopSubscriptions,
}

impl PtrCheckHandler {
    fn new(expected_ptr: usize, expected_len: usize) -> Self {
        Self {
            expected_ptr,
            expected_len,
            manager: NoopSubscriptions,
        }
    }
}

impl WsEndpointHandler for PtrCheckHandler {
    type Message = ();
    type Error = PtrCheckError;
    type Subscription = NoopSubscriptions;

    fn subscription_manager(&mut self) -> &mut Self::Subscription {
        &mut self.manager
    }

    fn generate_auth(&self) -> Option<Vec<u8>> {
        None
    }

    fn parse(&mut self, data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error> {
        if data.as_ptr() as usize != self.expected_ptr {
            return Err(PtrCheckError::PointerMismatch);
        }
        if data.len() != self.expected_len {
            return Err(PtrCheckError::LengthMismatch);
        }
        Ok(WsParseOutcome::Message(WsMessageAction::Continue))
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

#[tokio::test]
async fn inbound_processing_is_zero_copy() {
    let payload = Bytes::from_static(b"{\"jsonrpc\":\"2.0\"}");
    let expected_ptr = payload.as_ptr() as usize;
    let expected_len = payload.len();

    let handler = PtrCheckHandler::new(expected_ptr, expected_len);
    let reconnect = NoReconnect;
    let ping = ProtocolPingPong::new(Duration::from_secs(5), Duration::from_secs(10));

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "wss://example.invalid".to_string(),
        tls: WsTlsConfig::default(),
        transport: TungsteniteTransport::default(),
        reconnect_strategy: reconnect,
        handler,
        ping_strategy: ping,
        enable_ping: true,
        stale_threshold: Duration::from_secs(30),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 8,
        rate_limiter: None,
        circuit_breaker: None,
        latency_policy: None,
        registration: None,
        metrics: None,
    });

    let message = WsMessage::Binary(payload.clone());
    let result = actor.ask(WebSocketEvent::Inbound(message)).await;

    assert!(
        result.is_ok(),
        "expected zero-copy inbound parse, got {result:?}"
    );
}
