use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sonic_rs::{JsonValueTrait, PointerTree};

use shared_ws::client;
use shared_ws::ws::{WsFrame, into_ws_frame};

#[inline]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Debug)]
struct TickState {
    last_trade_t: i64,
    count_in_same_trade_t: u32,
    last_tick_bucket: i64,
    count_in_same_tick_bucket: u32,
    last_seen_at: Instant,
    last_seq: Option<i64>,
    recent_seqs: HashSet<i64>,
}

impl Default for TickState {
    fn default() -> Self {
        Self {
            last_trade_t: 0,
            count_in_same_trade_t: 0,
            last_tick_bucket: 0,
            count_in_same_tick_bucket: 0,
            last_seen_at: Instant::now(),
            last_seq: None,
            recent_seqs: HashSet::new(),
        }
    }
}

fn usage() -> &'static str {
    "bybit_trade_batch_logger\n\
  Subscribes to Bybit `publicTrade.{symbol}` and logs when:\n\
  - a message contains a batch of trades (data array has > 1 element)\n\
  - multiple trades share the same `T` timestamp (ms)\n\
  - multiple trades fall into the same configurable tick bucket (default 50ms)\n\
\n\
USAGE:\n\
  cargo run -p examples-ws --example bybit_trade_batch_logger -- [--url <wss-url>] [--symbols <CSV>] [--tick-ms <N>] [--dump]\n\
\n\
DEFAULTS:\n\
  --url      wss://stream.bybit.com/v5/public/linear\n\
  --symbols  BTCUSDT,ETHUSDT\n\
  --tick-ms  50\n\
"
}

fn parse_args() -> (String, Vec<String>, bool, i64) {
    let mut url = "wss://stream.bybit.com/v5/public/linear".to_string();
    let mut symbols = vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()];
    let mut tick_ms: i64 = 50;
    let mut dump = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => {
                url = it.next().unwrap_or_else(|| {
                    eprintln!("{usage}", usage = usage());
                    std::process::exit(2);
                });
            }
            "--symbols" => {
                let csv = it.next().unwrap_or_else(|| {
                    eprintln!("{usage}", usage = usage());
                    std::process::exit(2);
                });
                symbols = csv
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.trim().to_string())
                    .collect();
                if symbols.is_empty() {
                    eprintln!("--symbols must not be empty");
                    std::process::exit(2);
                }
            }
            "--tick-ms" => {
                let v = it.next().unwrap_or_else(|| {
                    eprintln!("{usage}", usage = usage());
                    std::process::exit(2);
                });
                tick_ms = v.parse::<i64>().unwrap_or(50).max(1);
            }
            "--dump" => dump = true,
            "-h" | "--help" => {
                eprintln!("{usage}", usage = usage());
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}\n\n{usage}", usage = usage());
                std::process::exit(2);
            }
        }
    }

    (url, symbols, dump, tick_ms)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (url, symbols, dump, tick_ms) = parse_args();

    println!("connecting: {url}");
    let mut ws = client::connect_async(&url).await?;

    // Subscribe: {"op":"subscribe","args":["publicTrade.BTCUSDT","publicTrade.ETHUSDT"]}
    let args: Vec<String> = symbols.iter().map(|s| format!("publicTrade.{s}")).collect();
    let sub = format!(
        "{{\"op\":\"subscribe\",\"args\":[{}]}}",
        args.iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    ws.send(into_ws_frame(Bytes::from(sub))).await?;
    println!("subscribed: {}", args.join(", "));
    println!("dump mode: {dump}");
    println!("tick_ms: {tick_ms}");

    // Bybit recommends ping every ~20s to keep the connection alive.
    let mut last_ping = Instant::now();

    // Track per-symbol tick state. Keyed by `Bytes` so we can look up by `&[u8]` (no alloc).
    let mut state: HashMap<Bytes, TickState> = HashMap::with_capacity(symbols.len());
    for sym in &symbols {
        state.insert(Bytes::copy_from_slice(sym.as_bytes()), TickState::default());
    }

    // Single-pass extraction of `topic`, `ts`, and `data`.
    let mut tree = PointerTree::new();
    tree.add_path(&["topic"]);
    tree.add_path(&["ts"]);
    tree.add_path(&["data"]);

    loop {
        if last_ping.elapsed() >= Duration::from_secs(20) {
            let _ = ws.send(WsFrame::text_static("{\"op\":\"ping\"}")).await;
            last_ping = Instant::now();
        }

        let Some(frame) = ws.next().await else {
            eprintln!("ws ended");
            break;
        };
        let frame = frame?;

        let WsFrame::Text(text) = frame else {
            continue;
        };

        let text = text.as_str();

        // Cheap filter to avoid parsing non-trade traffic (ack/pong/etc).
        if !text.contains("\"topic\":\"publicTrade.") {
            continue;
        }

        // Parse once and keep LazyValues borrowed from `text`.
        let vals = match sonic_rs::get_many(text, &tree) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(topic) = vals[0].as_ref().and_then(|v| v.as_str()) else {
            continue;
        };
        let ts = vals[1].as_ref().and_then(|v| v.as_i64()).unwrap_or(0);
        let Some(data_lv) = vals[2].as_ref() else {
            continue;
        };

        let Some(it) = data_lv.clone().into_array_iter() else {
            continue;
        };

        let mut n: usize = 0;
        let mut first_t: Option<i64> = None;
        let mut last_t: i64 = 0;
        let mut all_same_t = true;
        let mut first_sym_hash: Option<u64> = None;
        let mut multi_sym = false;

        for elem in it {
            let Ok(elem) = elem else { break };

            let t = elem.get("T").and_then(|x| x.as_i64()).unwrap_or(0);
            let sym_lv = match elem.get("s") {
                Some(v) => v,
                None => continue,
            };
            let sym = match sym_lv.as_str() {
                Some(s) => s,
                None => continue,
            };
            let sym_b = sym.as_bytes();
            let seq = elem.get("seq").and_then(|x| x.as_i64());

            if let Some(ft) = first_t {
                if t != ft {
                    all_same_t = false;
                }
            } else {
                first_t = Some(t);
            }
            last_t = t;

            let h = fnv1a64(sym_b);
            if let Some(first) = first_sym_hash {
                if first != h {
                    multi_sym = true;
                }
            } else {
                first_sym_hash = Some(h);
            }

            let st = if let Some(st) = state.get_mut(sym_b) {
                st
            } else {
                // Unexpected symbol; allocate once on first sighting.
                state.insert(Bytes::copy_from_slice(sym_b), TickState::default());
                state.get_mut(sym_b).expect("just inserted")
            };

            if t != 0 {
                // Exact-timestamp collision detection.
                if t == st.last_trade_t {
                    st.count_in_same_trade_t = st.count_in_same_trade_t.saturating_add(1);
                    if st.count_in_same_trade_t == 2 {
                        println!(
                            "[SAME_T] symbol={} T={} (multiple trades share same trade timestamp ms)",
                            sym, t
                        );
                        if dump {
                            println!("{text}");
                        }
                    }
                } else {
                    st.last_trade_t = t;
                    st.count_in_same_trade_t = 1;
                }

                // "Tick bucket" collision detection (e.g. 50ms / 100ms buckets).
                let bucket = t / tick_ms;
                if bucket == st.last_tick_bucket {
                    st.count_in_same_tick_bucket = st.count_in_same_tick_bucket.saturating_add(1);
                    if st.count_in_same_tick_bucket == 2 {
                        println!(
                            "[MULTI_IN_TICK] symbol={} tick_ms={} bucket={} (multiple trades in same tick bucket)",
                            sym, tick_ms, bucket
                        );
                        if dump {
                            println!("{text}");
                        }
                    }
                } else {
                    st.last_tick_bucket = bucket;
                    st.count_in_same_tick_bucket = 1;
                }
            }

            // Bybit docs note: multiple messages may be sent for the same seq.
            if let Some(seq) = seq {
                if st.recent_seqs.contains(&seq) && st.last_seq != Some(seq) {
                    println!("[REPEAT_SEQ] symbol={sym} seq={seq} (seen again)");
                    if dump {
                        println!("{text}");
                    }
                }
                st.recent_seqs.insert(seq);
                if st.recent_seqs.len() > 4096 {
                    st.recent_seqs.clear();
                }
                st.last_seq = Some(seq);
            }

            st.last_seen_at = Instant::now();
            n = n.saturating_add(1);
        }

        if n > 1 {
            println!(
                "[BATCH] topic={} ts={} data_len={} all_same_T={} first_T={} last_T={}",
                topic,
                ts,
                n,
                all_same_t,
                first_t.unwrap_or(0),
                last_t
            );
            if multi_sym {
                println!(
                    "[MULTI_SYMBOL_IN_DATA] topic={} (data contains >1 symbol)",
                    topic
                );
            }
            if dump {
                println!("{text}");
            }
        }
    }

    Ok(())
}
