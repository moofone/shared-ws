use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use shared_rate_limiter::{Cost, Key, RateLimiter};
use shared_ws::ws::{
    WsConfirmMode, WsDelegatedError, WsDelegatedOk, WsDelegatedRequest, WsEndpointHandler,
    WsMessageAction, WsParseOutcome, WsRateLimitFeedback, WsRequestMatch, WsSubscriptionManager,
};

use crate::coordinator::{DelegatedOutcome, DelegatedSendCoordinator};
use tokio::sync::Mutex;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct DeribitSubscribe {
    pub channels: Vec<String>,
}

impl DeribitSubscribe {
    pub fn to_jsonrpc_payload(&self, request_id: u64) -> String {
        // Deribit public subscribe (JSON-RPC 2.0) payload.
        // Example:
        // {"jsonrpc":"2.0","id":42,"method":"public/subscribe","params":{"channels":["trades.BTC-PERPETUAL.raw"]}}
        let mut out = String::with_capacity(128);
        out.push_str("{\"jsonrpc\":\"2.0\",\"id\":");
        out.push_str(&request_id.to_string());
        out.push_str(",\"method\":\"public/subscribe\",\"params\":{\"channels\":[");
        for (i, ch) in self.channels.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            // minimal escape for test/example purposes
            for b in ch.bytes() {
                if b == b'\"' || b == b'\\' {
                    out.push('\\');
                }
                out.push(b as char);
            }
            out.push('"');
        }
        out.push_str("]}}");
        out
    }

    pub fn fingerprint(&self) -> u64 {
        // Stable cheap hash: method + NUL + channels joined with NUL.
        let mut h = Fnv1a64::new();
        h.update(b"public/subscribe\0");
        for ch in &self.channels {
            h.update(ch.as_bytes());
            h.update(b"\0");
        }
        h.finish()
    }
}

#[derive(Clone)]
pub struct DeribitPublicHandler<S> {
    subs: S,
}

impl<S> DeribitPublicHandler<S>
where
    S: WsSubscriptionManager,
{
    pub fn new(subs: S) -> Self {
        Self { subs }
    }
}

impl<S> WsEndpointHandler for DeribitPublicHandler<S>
where
    S: WsSubscriptionManager,
{
    type Message = ();
    type Error = std::io::Error;
    type Subscription = S;

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
        // Cheap checks for delegated subscribe responses:
        // require `"id"` and either `"result"` or `"error"`.
        if !memmem(data, b"\"id\"") {
            return false;
        }
        memmem(data, b"\"result\"") || memmem(data, b"\"error\"")
    }

    fn match_request_response(&mut self, data: &[u8]) -> Option<WsRequestMatch> {
        let request_id = parse_jsonrpc_id(data)?;
        if memmem(data, b"\"result\"") {
            return Some(WsRequestMatch {
                request_id,
                result: Ok(()),
                rate_limit_feedback: None,
            });
        }
        if memmem(data, b"\"error\"") {
            // Optional: surface server throttling as feedback if present. For this sprint,
            // we just use a best-effort signal.
            let feedback = if memmem(data, b"too_many_requests") || memmem(data, b"rate") {
                Some(WsRateLimitFeedback {
                    retry_after: Some(Duration::from_millis(250)),
                    hint: Some("server_rate_limit".to_string()),
                })
            } else {
                None
            };
            return Some(WsRequestMatch {
                request_id,
                result: Err("endpoint error".to_string()),
                rate_limit_feedback: feedback,
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
    ) -> shared_ws::ws::WsErrorAction {
        shared_ws::ws::WsErrorAction::Continue
    }

    fn reset_state(&mut self) {}

    fn classify_disconnect(
        &self,
        _cause: &shared_ws::ws::WsDisconnectCause,
    ) -> shared_ws::ws::WsDisconnectAction {
        shared_ws::ws::WsDisconnectAction::Abort
    }
}

pub struct DeribitCoordinator {
    limiter: Mutex<RateLimiter>,
    key: Key,
}

impl DeribitCoordinator {
    pub fn new(limiter: RateLimiter, key: Key) -> Self {
        Self {
            limiter: Mutex::new(limiter),
            key,
        }
    }

    pub async fn delegated_subscribe<R, P, I, T, S>(
        &self,
        ws: &kameo::prelude::ActorRef<
            shared_ws::ws::WebSocketActor<DeribitPublicHandler<S>, R, P, I, T>,
        >,
        sub: DeribitSubscribe,
        confirm_deadline: Instant,
    ) -> DelegatedOutcome<WsDelegatedOk, WsDelegatedError>
    where
        S: WsSubscriptionManager,
        R: shared_ws::ws::WsReconnectStrategy,
        P: shared_ws::ws::WsPingPongStrategy,
        I: shared_ws::ws::WsIngress<
                Message = <DeribitPublicHandler<S> as WsEndpointHandler>::Message,
            >,
        T: shared_ws::transport::WsTransport,
    {
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let payload = sub.to_jsonrpc_payload(request_id);
        let fingerprint = sub.fingerprint();

        // External coordinator: acquire permit immediately before sending.
        let coordinator = DelegatedSendCoordinator::new(&self.limiter, self.key);
        coordinator
            .send_and_account(
                ws,
                WsDelegatedRequest {
                    request_id,
                    fingerprint,
                    frame: shared_ws::ws::into_ws_message(payload),
                    confirm_deadline,
                    confirm_mode: WsConfirmMode::Confirmed,
                },
                Cost::ONE,
            )
            .await
    }
}

struct Fnv1a64(u64);

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn update(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(Self::PRIME);
        }
        self.0 = h;
    }

    fn finish(self) -> u64 {
        self.0
    }
}

fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn parse_jsonrpc_id(data: &[u8]) -> Option<u64> {
    // Minimal scanner: find `"id"` then parse the following integer.
    let hay = data;
    let mut i = 0usize;
    while i + 4 < hay.len() {
        if hay[i] == b'"'
            && hay.get(i + 1) == Some(&b'i')
            && hay.get(i + 2) == Some(&b'd')
            && hay.get(i + 3) == Some(&b'"')
        {
            i += 4;
            while i < hay.len() && is_ws(hay[i]) {
                i += 1;
            }
            if i >= hay.len() || hay[i] != b':' {
                continue;
            }
            i += 1;
            while i < hay.len() && is_ws(hay[i]) {
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

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t')
}
