use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use shared_ws::client::accept_async;
use shared_ws::transport::tungstenite::TungsteniteTransport;
use shared_ws::ws::{
    ForwardAllIngress, GetConnectionStatus, ProtocolPingPong, WebSocketActor, WebSocketActorArgs,
    WebSocketBufferConfig, WebSocketEvent, WsConfirmMode, WsDelegatedError, WsDelegatedOk,
    WsDelegatedRequest, WsDisconnectAction, WsDisconnectCause, WsEndpointHandler, WsErrorAction,
    WsMessageAction, WsParseOutcome, WsReconnectStrategy, WsRequestMatch, WsSubscriptionAction,
    WsSubscriptionManager, WsSubscriptionStatus, into_ws_message,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Clone)]
struct NoReconnect;

impl WsReconnectStrategy for NoReconnect {
    fn next_delay(&mut self) -> Duration {
        Duration::from_secs(365 * 24 * 60 * 60)
    }
    fn reset(&mut self) {}
    fn should_retry(&self) -> bool {
        false
    }
}

#[derive(Debug, Default, Clone)]
struct NoSubscriptions;

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

#[derive(Clone)]
struct JsonRpcMatcherHandler {
    subs: NoSubscriptions,
}

impl JsonRpcMatcherHandler {
    fn new() -> Self {
        Self {
            subs: NoSubscriptions,
        }
    }
}

fn parse_jsonrpc_id(data: &[u8]) -> Option<u64> {
    // Extremely small test-only parser: find `"id"` then parse the following integer.
    let hay = data;
    let mut i = 0usize;
    while i + 4 < hay.len() {
        if hay[i] == b'"'
            && hay.get(i + 1) == Some(&b'i')
            && hay.get(i + 2) == Some(&b'd')
            && hay.get(i + 3) == Some(&b'"')
        {
            i += 4;
            while i < hay.len()
                && (hay[i] == b' ' || hay[i] == b'\n' || hay[i] == b'\r' || hay[i] == b'\t')
            {
                i += 1;
            }
            if i >= hay.len() || hay[i] != b':' {
                continue;
            }
            i += 1;
            while i < hay.len()
                && (hay[i] == b' ' || hay[i] == b'\n' || hay[i] == b'\r' || hay[i] == b'\t')
            {
                i += 1;
            }
            let start = i;
            while i < hay.len() && hay[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                return None;
            }
            return std::str::from_utf8(&hay[start..i])
                .ok()?
                .parse::<u64>()
                .ok();
        }
        i += 1;
    }
    None
}

impl WsEndpointHandler for JsonRpcMatcherHandler {
    type Message = ();
    type Error = std::io::Error;
    type Subscription = NoSubscriptions;

    fn subscription_manager(&mut self) -> &mut Self::Subscription {
        &mut self.subs
    }

    fn generate_auth(&self) -> Option<Vec<u8>> {
        None
    }

    fn parse(&mut self, _data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error> {
        Ok(WsParseOutcome::Message(WsMessageAction::Continue))
    }

    fn maybe_request_response(&self, data: &[u8]) -> bool {
        // Cheap precheck: only attempt full match if the payload contains `"id"`.
        data.windows(4).any(|w| w == b"\"id\"")
    }

    fn match_request_response(&mut self, data: &[u8]) -> Option<WsRequestMatch> {
        let request_id = parse_jsonrpc_id(data)?;
        if data.windows(8).any(|w| w == b"\"result\"") {
            return Some(WsRequestMatch {
                request_id,
                result: Ok(()),
                rate_limit_feedback: None,
            });
        }
        if data.windows(7).any(|w| w == b"\"error\"") {
            return Some(WsRequestMatch {
                request_id,
                result: Err("error".to_string()),
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

#[derive(Clone, Copy, Debug)]
enum ServerMode {
    ConfirmOk,
    ConfirmErr,
}

async fn spawn_confirm_server(mode: ServerMode) -> (SocketAddr, mpsc::UnboundedReceiver<Bytes>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel::<Bytes>();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                if let Some(Ok(msg)) = ws.next().await {
                    let bytes = shared_ws::ws::message_bytes(&msg)
                        .map(Bytes::copy_from_slice)
                        .unwrap_or_else(|| Bytes::from_static(b""));
                    let _ = tx.send(bytes.clone());

                    let id = parse_jsonrpc_id(bytes.as_ref()).unwrap_or(0);
                    let resp = match mode {
                        ServerMode::ConfirmOk => format!(
                            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"ok\":true}}}}",
                            id
                        ),
                        ServerMode::ConfirmErr => {
                            format!(
                                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"message\":\"no\"}}}}",
                                id
                            )
                        }
                    };
                    let _ = ws.send(into_ws_message(resp)).await;
                }
            });
        }
    });

    (addr, rx)
}

fn assert_confirmed_ok(ok: WsDelegatedOk, request_id: u64) {
    assert_eq!(ok.request_id, request_id);
    assert!(ok.confirmed);
    assert!(ok.rate_limit_feedback.is_none());
}

async fn wait_connected<E, R, P, I, T>(
    actor: &kameo::prelude::ActorRef<WebSocketActor<E, R, P, I, T>>,
    timeout: Duration,
) where
    E: WsEndpointHandler,
    R: WsReconnectStrategy,
    P: shared_ws::ws::WsPingPongStrategy,
    I: shared_ws::ws::WsIngress<Message = E::Message>,
    T: shared_ws::transport::WsTransport,
{
    let deadline = Instant::now() + timeout;
    loop {
        let status = actor.ask(GetConnectionStatus).await.unwrap();
        if matches!(status, shared_ws::ws::WsConnectionStatus::Connected) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for Connected status (last={status:?})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_confirmed_happy_path() {
    let (addr, mut rx) = spawn_confirm_server(ServerMode::ConfirmOk).await;
    let handler = JsonRpcMatcherHandler::new();

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: format!("ws://{}", addr),
        transport: TungsteniteTransport::default(),
        reconnect_strategy: NoReconnect,
        handler,
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(60),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(2)).await;

    let request_id = 42u64;
    let outbound = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"sub\"}}",
        request_id
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let ok = actor
        .ask(WsDelegatedRequest {
            request_id,
            fingerprint: 999,
            frame: into_ws_message(outbound.clone()),
            confirm_deadline: deadline,
            confirm_mode: WsConfirmMode::Confirmed,
        })
        .await
        .unwrap();

    assert_confirmed_ok(ok, request_id);

    let server_got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server_got.as_ref(), outbound.as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_endpoint_rejected_is_surfaced() {
    let (addr, _rx) = spawn_confirm_server(ServerMode::ConfirmErr).await;
    let handler = JsonRpcMatcherHandler::new();

    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: format!("ws://{}", addr),
        transport: TungsteniteTransport::default(),
        reconnect_strategy: NoReconnect,
        handler,
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(60), Duration::from_secs(60)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(60),
        ws_buffers: WebSocketBufferConfig::default(),
        outbound_capacity: 32,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });

    actor.tell(WebSocketEvent::Connect).send().await.unwrap();
    wait_connected(&actor, Duration::from_secs(2)).await;

    let request_id = 7u64;
    let outbound = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"sub\"}}",
        request_id
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let err = actor
        .ask(WsDelegatedRequest {
            request_id,
            fingerprint: 999,
            frame: into_ws_message(outbound),
            confirm_deadline: deadline,
            confirm_mode: WsConfirmMode::Confirmed,
        })
        .await
        .expect_err("expected endpoint rejection");

    match err {
        kameo::error::SendError::HandlerError(WsDelegatedError::EndpointRejected {
            request_id: got,
            ..
        }) => assert_eq!(got, request_id),
        other => panic!("expected EndpointRejected handler error, got {other:?}"),
    }
}
