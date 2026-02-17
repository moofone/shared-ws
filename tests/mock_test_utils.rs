use std::time::{Duration, Instant};

use kameo::error::SendError;
use shared_ws::testing::{JsonRpcDelegatedEndpoint, MockTransport, NoReconnect, frame_jsonrpc_id};
use shared_ws::ws::{
    ForwardAllIngress, GetConnectionStatus, ProtocolPingPong, WebSocketActor, WebSocketActorArgs,
    WebSocketEvent, WsConnectionStatus, WsDelegatedError, WsDelegatedRequest, into_ws_message,
};

type TestWsActor = WebSocketActor<
    JsonRpcDelegatedEndpoint,
    NoReconnect,
    ProtocolPingPong,
    ForwardAllIngress<()>,
    MockTransport,
>;
type TestWsActorRef = kameo::prelude::ActorRef<TestWsActor>;

fn delegated_request(request_id: u64, method: &str) -> WsDelegatedRequest {
    let payload = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"{}\",\"params\":{{}}}}",
        request_id, method
    );
    WsDelegatedRequest::confirmed(
        request_id,
        fnv1a64(payload.as_bytes()),
        into_ws_message(payload),
        Instant::now() + Duration::from_millis(500),
    )
}

async fn wait_until_connected(ws: &TestWsActorRef, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let status = ws
            .ask(GetConnectionStatus)
            .await
            .expect("get connection status");
        if status == WsConnectionStatus::Connected {
            return;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for connected status");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_until_disconnected(ws: &TestWsActorRef, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let status = ws
            .ask(GetConnectionStatus)
            .await
            .expect("get connection status");
        if status == WsConnectionStatus::Disconnected {
            return;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for disconnected status");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_transport_confirms_delegated_request() {
    let (transport, mut server) = MockTransport::channel_pair();
    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://mock".to_string(),
        transport,
        reconnect_strategy: NoReconnect,
        handler: JsonRpcDelegatedEndpoint::default(),
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(30), Duration::from_secs(30)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(30),
        ws_buffers: Default::default(),
        outbound_capacity: 64,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });
    actor
        .tell(WebSocketEvent::Connect)
        .send()
        .await
        .expect("connect event accepted");
    wait_until_connected(&actor, Duration::from_secs(1)).await;

    let request_id = 91_u64;
    let ask_task = tokio::spawn({
        let actor = actor.clone();
        async move {
            actor
                .ask(delegated_request(request_id, "private/buy"))
                .await
        }
    });

    let outbound = server.recv_outbound().await.expect("outbound frame");
    assert_eq!(frame_jsonrpc_id(&outbound), Some(request_id));
    server
        .send_text(format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"ok\":true}}}}",
            request_id
        ))
        .expect("inbound response");

    let result = ask_task.await.expect("ask join").expect("delegated ok");
    assert!(result.confirmed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_server_drop_socket_is_observable_in_delegated_flow() {
    let (transport, mut server) = MockTransport::channel_pair();
    let actor = WebSocketActor::spawn(WebSocketActorArgs {
        url: "ws://mock".to_string(),
        transport,
        reconnect_strategy: NoReconnect,
        handler: JsonRpcDelegatedEndpoint::default(),
        ingress: ForwardAllIngress::default(),
        ping_strategy: ProtocolPingPong::new(Duration::from_secs(30), Duration::from_secs(30)),
        enable_ping: false,
        stale_threshold: Duration::from_secs(30),
        ws_buffers: Default::default(),
        outbound_capacity: 64,
        circuit_breaker: None,
        latency_policy: None,
        payload_latency_sampling: None,
        registration: None,
        metrics: None,
    });
    actor
        .tell(WebSocketEvent::Connect)
        .send()
        .await
        .expect("connect event accepted");
    wait_until_connected(&actor, Duration::from_secs(1)).await;

    let request_id = 92_u64;
    let ask_task = tokio::spawn({
        let actor = actor.clone();
        async move {
            actor
                .ask(delegated_request(request_id, "private/test_drop_socket"))
                .await
        }
    });

    let outbound = server.recv_outbound().await.expect("outbound frame");
    assert_eq!(frame_jsonrpc_id(&outbound), Some(request_id));

    server.drop_socket();

    let result = ask_task.await.expect("ask join");
    assert!(matches!(
        result,
        Err(SendError::HandlerError(WsDelegatedError::Unconfirmed {
            request_id: 92,
            ..
        })) | Err(SendError::HandlerError(WsDelegatedError::NotDelivered {
            request_id: 92,
            ..
        }))
    ));

    wait_until_disconnected(&actor, Duration::from_secs(1)).await;
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
