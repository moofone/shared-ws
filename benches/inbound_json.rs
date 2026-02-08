use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use sonic_rs::Value;

use shared_ws::core::{
    WsDisconnectAction, WsDisconnectCause, WsEndpointHandler, WsErrorAction, WsMessageAction,
    WsParseOutcome, WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus,
};

#[derive(Default)]
struct NoopSubs;

impl WsSubscriptionManager for NoopSubs {
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

#[derive(Default)]
struct JsonHandler {
    subs: NoopSubs,
}

#[derive(Debug)]
struct JsonErr;

impl std::fmt::Display for JsonErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsonErr")
    }
}

impl std::error::Error for JsonErr {}

impl WsEndpointHandler for JsonHandler {
    type Message = Value;
    type Error = JsonErr;
    type Subscription = NoopSubs;

    fn subscription_manager(&mut self) -> &mut Self::Subscription {
        &mut self.subs
    }

    fn generate_auth(&self) -> Option<Vec<u8>> {
        None
    }

    fn parse(&mut self, data: &[u8]) -> Result<WsParseOutcome<Self::Message>, Self::Error> {
        let v: Value = sonic_rs::from_slice(data).map_err(|_| JsonErr)?;
        Ok(WsParseOutcome::Message(WsMessageAction::Process(v)))
    }

    fn handle_message(&mut self, _msg: Self::Message) -> Result<(), Self::Error> {
        Ok(())
    }

    fn handle_server_error(
        &mut self,
        _code: Option<i32>,
        _message: &str,
        _data: Option<Value>,
    ) -> WsErrorAction {
        WsErrorAction::Continue
    }

    fn reset_state(&mut self) {}

    fn classify_disconnect(&self, _cause: &WsDisconnectCause) -> WsDisconnectAction {
        WsDisconnectAction::ImmediateReconnect
    }
}

fn typical_trade_json() -> Bytes {
    // Simple representative payload with a few numeric/string fields.
    Bytes::from_static(
        br#"{"type":"trade","symbol":"BTCUSDT","price":"43123.12","qty":"0.001","ts":1700000000000,"side":"buy","id":1234567}"#,
    )
}

fn bench_inject_1000_json_frames(c: &mut Criterion) {
    let payload = typical_trade_json();
    let mut handler = JsonHandler::default();

    c.bench_function("inject_1000_json_frames_parse_pipeline", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                handler
                    .subscription_manager()
                    .handle_subscription_response(black_box(payload.as_ref()));

                match handler.parse(black_box(payload.as_ref())).unwrap() {
                    WsParseOutcome::Message(WsMessageAction::Process(msg)) => {
                        handler.handle_message(msg).unwrap();
                    }
                    other => {
                        let _ = black_box(other);
                    }
                }
            }
        })
    });
}

criterion_group!(benches, bench_inject_1000_json_frames);
criterion_main!(benches);
