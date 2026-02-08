use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use serde::Deserialize;
use sonic_rs::JsonContainerTrait;
use sonic_rs::JsonValueTrait;
use sonic_rs::PointerTree;
use sonic_rs::Value;

use shared_ws::ws::{
    WsDisconnectAction, WsDisconnectCause, WsEndpointHandler, WsErrorAction, WsMessageAction,
    WsParseOutcome, WsSubscriptionAction, WsSubscriptionManager, WsSubscriptionStatus,
};

#[derive(Default)]
struct NoopSubs;

impl WsSubscriptionManager for NoopSubs {
    type SubscriptionMessage = ();

    #[inline]
    fn maybe_subscription_response(&self, _data: &[u8]) -> bool {
        // Benchmark focuses on JSON parsing only; skip ACK parsing preflight entirely.
        false
    }

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
        // This bench models the websocket text-frame path where UTF-8 validity is already
        // guaranteed by the transport layer (tungstenite validates text frames).
        let v: Value = unsafe { sonic_rs::from_slice_unchecked(data) }.map_err(|_| JsonErr)?;
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

fn bybit_public_trade_frame(trades: usize) -> Bytes {
    // Representative Bybit v5 publicTrade topic notification. Bybit encodes numbers as strings.
    // Example shape:
    // {"topic":"publicTrade.BTCUSDT","data":[{...}, ...]}
    //
    // Include a few extra fields to more closely resemble real frames, so we can see
    // whether DOM parsing (parses all fields) or lazy field extraction wins.
    let trades = trades.max(1);
    let mut s = String::with_capacity(128 + trades * 96);
    s.push_str(
        "{\"topic\":\"publicTrade.BTCUSDT\",\"type\":\"snapshot\",\"ts\":1700000000000,\"data\":[",
    );
    for i in 0..trades {
        if i != 0 {
            s.push(',');
        }
        let ts = 1_700_000_000_000i64 + (i as i64);
        let price = 43_000.0 + (i as f64) * 0.01;
        let qty = 0.001 + (i as f64) * 0.000001;
        let side = if (i & 1) == 0 { "Buy" } else { "Sell" };
        let tick = if (i % 3) == 0 {
            "PlusTick"
        } else if (i % 3) == 1 {
            "ZeroPlusTick"
        } else {
            "MinusTick"
        };
        // Keep strings simple (no escapes) for predictable parsing.
        s.push_str("{\"T\":");
        s.push_str(&ts.to_string());
        s.push_str(",\"p\":\"");
        s.push_str(&format!("{price:.2}"));
        s.push_str("\",\"v\":\"");
        s.push_str(&format!("{qty:.6}"));
        s.push_str("\",\"S\":\"");
        s.push_str(side);
        s.push_str("\",\"s\":\"BTCUSDT\"");
        s.push_str(",\"L\":\"");
        s.push_str(tick);
        s.push_str("\",\"i\":\"");
        s.push_str(&(1_000_000_000u64 + i as u64).to_string());
        s.push_str("\",\"m\":");
        s.push_str(if (i & 1) == 0 { "false" } else { "true" });
        s.push_str(",\"BT\":");
        s.push_str(&(1700000000000u64 + i as u64).to_string());
        s.push('}');
    }
    s.push_str("]}");
    Bytes::from(s)
}

fn deribit_trades_frame(trades: usize) -> Bytes {
    // Representative Deribit subscription notification frame:
    // {"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-PERPETUAL.100ms","data":[...]}}
    //
    // Include extra per-trade fields to better match real Deribit payloads.
    let trades = trades.max(1);
    let mut s = String::with_capacity(196 + trades * 120);
    s.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"subscription\",\"params\":{\"channel\":\"trades.BTC-PERPETUAL.100ms\",\"data\":[");
    for i in 0..trades {
        if i != 0 {
            s.push(',');
        }
        let ts = 1_700_000_000_000i64 + (i as i64);
        let price = 69_000.0 + (i as f64) * 0.5;
        let amount = 10.0 + (i as f64);
        let direction = if (i & 1) == 0 { "buy" } else { "sell" };
        let tick_dir = (i % 4) as i64;
        let mark = price + 1.0;
        let index = price - 1.0;
        s.push_str("{\"timestamp\":");
        s.push_str(&ts.to_string());
        s.push_str(",\"price\":");
        s.push_str(&format!("{price:.1}"));
        s.push_str(",\"amount\":");
        s.push_str(&format!("{amount:.1}"));
        s.push_str(",\"direction\":\"");
        s.push_str(direction);
        s.push_str("\",\"trade_id\":\"");
        s.push_str(&(9_000_000_000u64 + i as u64).to_string());
        s.push_str("\",\"tick_direction\":");
        s.push_str(&tick_dir.to_string());
        s.push_str(",\"mark_price\":");
        s.push_str(&format!("{mark:.1}"));
        s.push_str(",\"index_price\":");
        s.push_str(&format!("{index:.1}"));
        s.push_str(",\"instrument_name\":\"BTC-PERPETUAL\"}");
    }
    s.push_str("]}}");
    Bytes::from(s)
}

fn bench_inject_1000_json_frames_dom_value(c: &mut Criterion) {
    let payload = typical_trade_json();
    let mut handler = JsonHandler::default();

    c.bench_function("inject_1000_json_frames_parse_only", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let msg = match handler.parse(black_box(payload.as_ref())) {
                    Ok(WsParseOutcome::Message(WsMessageAction::Process(msg))) => msg,
                    // This bench is specifically measuring the happy-path parse cost.
                    _ => unreachable!("unexpected parse outcome in parse-only benchmark"),
                };
                // Explicitly exclude any downstream processing/aggregation from this bench.
                black_box(msg);
            }
        })
    });
}

fn bench_inject_1000_json_frames_get_fields(c: &mut Criterion) {
    let payload = typical_trade_json();

    c.bench_function("inject_1000_json_frames_get_fields_only", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                // Parse only the fields we care about, without allocating a full Value DOM.
                // This models the "fast ingress" approach for high-volume streams.
                // Note: `sonic_rs::get` returns a lazy view (`LazyValue`). Avoid returning
                // borrowed `&str` from this bench to keep lifetimes simple; measure presence/types.
                let symbol_is_str = sonic_rs::get(black_box(payload.as_ref()), &["symbol"])
                    .ok()
                    .map(|v| v.is_str())
                    .unwrap_or(false);
                let price_is_str = sonic_rs::get(black_box(payload.as_ref()), &["price"])
                    .ok()
                    .map(|v| v.is_str())
                    .unwrap_or(false);
                let qty_is_str = sonic_rs::get(black_box(payload.as_ref()), &["qty"])
                    .ok()
                    .map(|v| v.is_str())
                    .unwrap_or(false);
                let ts = sonic_rs::get(black_box(payload.as_ref()), &["ts"])
                    .ok()
                    .and_then(|v| v.as_i64());
                let side_is_str = sonic_rs::get(black_box(payload.as_ref()), &["side"])
                    .ok()
                    .map(|v| v.is_str())
                    .unwrap_or(false);

                black_box((symbol_is_str, price_is_str, qty_is_str, ts, side_is_str));
            }
        })
    });
}

fn bench_inject_1000_json_frames_get_many(c: &mut Criterion) {
    let payload = typical_trade_json();

    // Build a pointer tree once; this lets sonic-rs extract multiple paths in a single parse pass.
    let mut tree = PointerTree::new();
    tree.add_path(&["symbol"]);
    tree.add_path(&["price"]);
    tree.add_path(&["qty"]);
    tree.add_path(&["ts"]);
    tree.add_path(&["side"]);

    c.bench_function("inject_1000_json_frames_get_many_only", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let vals = sonic_rs::get_many(black_box(payload.as_ref()), &tree).unwrap();

                // Same principle as get_fields_only: don't return borrowed strings; just verify
                // types/presence and extract primitive numeric where possible.
                let symbol_is_str = vals[0].as_ref().map(|v| v.is_str()).unwrap_or(false);
                let price_is_str = vals[1].as_ref().map(|v| v.is_str()).unwrap_or(false);
                let qty_is_str = vals[2].as_ref().map(|v| v.is_str()).unwrap_or(false);
                let ts = vals[3].as_ref().and_then(|v| v.as_i64());
                let side_is_str = vals[4].as_ref().map(|v| v.is_str()).unwrap_or(false);

                black_box((symbol_is_str, price_is_str, qty_is_str, ts, side_is_str));
            }
        })
    });
}

#[inline]
fn parse_f64_lexical(s: &str) -> f64 {
    // lexical-core expects bytes; input is ASCII digits + '.'.
    lexical_core::parse::<f64>(s.as_bytes()).unwrap_or(f64::NAN)
}

#[derive(Debug, Deserialize)]
struct BybitFrame<'a> {
    #[serde(borrow)]
    data: Vec<BybitTradeRow<'a>>,
}

#[derive(Debug, Deserialize)]
struct BybitTradeRow<'a> {
    #[serde(rename = "T")]
    t: i64,
    #[serde(borrow, rename = "p")]
    p: &'a str,
    #[serde(borrow, rename = "v")]
    v: &'a str,
}

#[derive(Debug, Deserialize)]
struct DeribitFrame {
    params: DeribitParams,
}

#[derive(Debug, Deserialize)]
struct DeribitParams {
    data: Vec<DeribitTradeRow>,
}

#[derive(Debug, Deserialize)]
struct DeribitTradeRow {
    timestamp: i64,
    price: f64,
    amount: f64,
}

fn bench_bybit_trade_frame_dom_vs_lazy(c: &mut Criterion) {
    const TOTAL_TRADES: usize = 100_000;
    for trades_per_frame in [1usize, 10, 50, 200] {
        let payload = bybit_public_trade_frame(trades_per_frame);
        let frames = (TOTAL_TRADES / trades_per_frame).max(1);

        c.bench_function(
            &format!("bybit_frame_dom_extract_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let v: Value = sonic_rs::from_slice(black_box(payload.as_ref())).unwrap();
                        let data = v.get("data").and_then(|x| x.as_array()).unwrap();
                        for row in data {
                            let ts = row.get("T").and_then(|x| x.as_i64()).unwrap_or(0) as f64;
                            let p = row
                                .get("p")
                                .and_then(|x| x.as_str().map(parse_f64_lexical))
                                .unwrap_or(0.0);
                            let q = row
                                .get("v")
                                .and_then(|x| x.as_str().map(parse_f64_lexical))
                                .unwrap_or(0.0);
                            sum += ts + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );

        c.bench_function(
            &format!("bybit_frame_lazy_iter_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let lv = sonic_rs::get(black_box(payload.as_ref()), &["data"]).unwrap();
                        let mut it = lv.into_array_iter().unwrap();
                        for elem in &mut it {
                            let elem = elem.unwrap();
                            let ts = elem.get("T").and_then(|x| x.as_i64()).unwrap_or(0) as f64;
                            let p = elem
                                .get("p")
                                .and_then(|x| x.as_str().map(parse_f64_lexical))
                                .unwrap_or(0.0);
                            let q = elem
                                .get("v")
                                .and_then(|x| x.as_str().map(parse_f64_lexical))
                                .unwrap_or(0.0);
                            sum += ts + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );

        c.bench_function(
            &format!("bybit_frame_lazy_obj_iter_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let lv = sonic_rs::get(black_box(payload.as_ref()), &["data"]).unwrap();
                        let mut it = lv.into_array_iter().unwrap();
                        for elem in &mut it {
                            let elem = elem.unwrap();
                            let mut ts = 0i64;
                            let mut p = 0.0f64;
                            let mut q = 0.0f64;
                            let mut found = 0u8;

                            let Some(obj) = elem.into_object_iter() else {
                                continue;
                            };
                            for kv in obj {
                                let Ok((k, v)) = kv else { break };
                                match k.as_ref() {
                                    "T" => {
                                        ts = v.as_i64().unwrap_or(0);
                                        found = found.saturating_add(1);
                                    }
                                    "p" => {
                                        p = v.as_str().map(parse_f64_lexical).unwrap_or(0.0);
                                        found = found.saturating_add(1);
                                    }
                                    "v" => {
                                        q = v.as_str().map(parse_f64_lexical).unwrap_or(0.0);
                                        found = found.saturating_add(1);
                                    }
                                    _ => {}
                                }
                                if found >= 3 {
                                    break;
                                }
                            }

                            sum += (ts as f64) + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );

        c.bench_function(
            &format!("bybit_frame_typed_extract_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let v: BybitFrame<'_> =
                            sonic_rs::from_slice(black_box(payload.as_ref())).unwrap();
                        for row in &v.data {
                            let ts = row.t as f64;
                            let p = parse_f64_lexical(row.p);
                            let q = parse_f64_lexical(row.v);
                            sum += ts + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );
    }
}

fn bench_deribit_trade_frame_dom_vs_lazy(c: &mut Criterion) {
    const TOTAL_TRADES: usize = 100_000;
    for trades_per_frame in [1usize, 10, 50, 200] {
        let payload = deribit_trades_frame(trades_per_frame);
        let frames = (TOTAL_TRADES / trades_per_frame).max(1);

        c.bench_function(
            &format!("deribit_frame_dom_extract_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let v: Value = sonic_rs::from_slice(black_box(payload.as_ref())).unwrap();
                        let data = v
                            .get("params")
                            .and_then(|x| x.get("data"))
                            .and_then(|x| x.as_array())
                            .unwrap();
                        for row in data {
                            let ts =
                                row.get("timestamp").and_then(|x| x.as_i64()).unwrap_or(0) as f64;
                            let p = row.get("price").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            let q = row.get("amount").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            sum += ts + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );

        c.bench_function(
            &format!("deribit_frame_lazy_iter_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let lv = sonic_rs::get(black_box(payload.as_ref()), &["params", "data"])
                            .unwrap();
                        let mut it = lv.into_array_iter().unwrap();
                        for elem in &mut it {
                            let elem = elem.unwrap();
                            let ts =
                                elem.get("timestamp").and_then(|x| x.as_i64()).unwrap_or(0) as f64;
                            let p = elem.get("price").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            let q = elem.get("amount").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            sum += ts + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );

        c.bench_function(
            &format!("deribit_frame_lazy_obj_iter_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let lv = sonic_rs::get(black_box(payload.as_ref()), &["params", "data"])
                            .unwrap();
                        let mut it = lv.into_array_iter().unwrap();
                        for elem in &mut it {
                            let elem = elem.unwrap();
                            let mut ts = 0i64;
                            let mut p = 0.0f64;
                            let mut q = 0.0f64;
                            let mut found = 0u8;

                            let Some(obj) = elem.into_object_iter() else {
                                continue;
                            };
                            for kv in obj {
                                let Ok((k, v)) = kv else { break };
                                match k.as_ref() {
                                    "timestamp" => {
                                        ts = v.as_i64().unwrap_or(0);
                                        found = found.saturating_add(1);
                                    }
                                    "price" => {
                                        p = v.as_f64().unwrap_or(0.0);
                                        found = found.saturating_add(1);
                                    }
                                    "amount" => {
                                        q = v.as_f64().unwrap_or(0.0);
                                        found = found.saturating_add(1);
                                    }
                                    _ => {}
                                }
                                if found >= 3 {
                                    break;
                                }
                            }

                            sum += (ts as f64) + p + q;
                        }
                    }
                    black_box(sum);
                })
            },
        );

        c.bench_function(
            &format!("deribit_frame_typed_extract_n{trades_per_frame}_t{TOTAL_TRADES}"),
            |b| {
                b.iter(|| {
                    let mut sum = 0.0f64;
                    for _ in 0..frames {
                        let v: DeribitFrame =
                            sonic_rs::from_slice(black_box(payload.as_ref())).unwrap();
                        for row in &v.params.data {
                            sum += (row.timestamp as f64) + row.price + row.amount;
                        }
                    }
                    black_box(sum);
                })
            },
        );
    }
}

criterion_group!(
    benches,
    bench_inject_1000_json_frames_dom_value,
    bench_inject_1000_json_frames_get_fields,
    bench_inject_1000_json_frames_get_many,
    bench_bybit_trade_frame_dom_vs_lazy,
    bench_deribit_trade_frame_dom_vs_lazy
);
criterion_main!(benches);
