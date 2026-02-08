use bytes::Bytes;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::frame::{WsFrame, into_ws_frame};

/// Result emitted by ping/pong strategies when processing inbound frames.
#[derive(Debug, Clone)]
pub enum WsPongResult {
    NotPong,
    PongReceived(Option<Duration>),
    InvalidPong,
    Reply(WsFrame),
}

/// Ping/pong handling contract for endpoint adapters.
pub trait WsPingPongStrategy: Send + Sync + 'static {
    fn create_ping(&mut self) -> Option<WsFrame>;
    fn handle_inbound(&mut self, message: &WsFrame) -> WsPongResult;
    fn is_stale(&self) -> bool;
    fn reset(&mut self);
    fn interval(&self) -> Duration;
    fn timeout(&self) -> Duration;
}

/// Standard websocket ping/pong handler operating on control frames.
pub struct ProtocolPingPong {
    interval: Duration,
    timeout: Duration,
    last_ping: Option<Instant>,
    last_pong: Option<Instant>,
}

impl ProtocolPingPong {
    pub fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            last_ping: None,
            last_pong: None,
        }
    }
}

impl WsPingPongStrategy for ProtocolPingPong {
    fn create_ping(&mut self) -> Option<WsFrame> {
        self.last_ping = Some(Instant::now());
        Some(WsFrame::Ping(Bytes::new()))
    }

    fn handle_inbound(&mut self, message: &WsFrame) -> WsPongResult {
        match message {
            WsFrame::Pong(_) => {
                let now = Instant::now();
                let rtt = self
                    .last_ping
                    .map(|sent| now.saturating_duration_since(sent));
                self.last_pong = Some(now);
                WsPongResult::PongReceived(rtt)
            }
            WsFrame::Ping(payload) => WsPongResult::Reply(WsFrame::Pong(payload.clone())),
            _ => WsPongResult::NotPong,
        }
    }

    fn is_stale(&self) -> bool {
        let Some(last_ping) = self.last_ping else {
            return false;
        };
        if last_ping.elapsed() <= self.timeout {
            return false;
        }
        // Stale if we have not observed a pong after the most recent ping.
        match self.last_pong {
            Some(last_pong) => last_pong < last_ping,
            None => true,
        }
    }

    fn reset(&mut self) {
        self.last_ping = None;
        self.last_pong = None;
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Application-level ping/pong strategy that tracks pending requests by key.
pub struct WsApplicationPingPong<CF, PF>
where
    CF: Fn() -> (Bytes, String) + Send + Sync + 'static,
    PF: Fn(&Bytes) -> Option<String> + Send + Sync + 'static,
{
    interval: Duration,
    timeout: Duration,
    pending_pings: HashMap<String, Instant>,
    max_pending: usize,
    oldest_pending: Option<Instant>,
    create_message: CF,
    parse_pong: PF,
}

impl<CF, PF> WsApplicationPingPong<CF, PF>
where
    CF: Fn() -> (Bytes, String) + Send + Sync + 'static,
    PF: Fn(&Bytes) -> Option<String> + Send + Sync + 'static,
{
    pub fn new(interval: Duration, timeout: Duration, create: CF, parse: PF) -> Self {
        Self {
            interval,
            timeout,
            pending_pings: HashMap::new(),
            max_pending: 10,
            oldest_pending: None,
            create_message: create,
            parse_pong: parse,
        }
    }

    pub fn with_max_pending(mut self, max_pending: usize) -> Self {
        self.max_pending = max_pending;
        self
    }

    fn touch_oldest_on_insert(&mut self, when: Instant) {
        self.oldest_pending = Some(self.oldest_pending.map(|old| old.min(when)).unwrap_or(when));
    }

    fn cleanup_stale(&mut self) {
        let now = Instant::now();
        self.pending_pings
            .retain(|_, sent| now.duration_since(*sent) <= self.timeout);
        self.oldest_pending = self.pending_pings.values().copied().min();
    }

    fn process_application(&mut self, payload: &Bytes) -> WsPongResult {
        if let Some(key) = (self.parse_pong)(payload)
            && let Some(sent_at) = self.pending_pings.remove(&key)
        {
            if self.oldest_pending == Some(sent_at) {
                self.oldest_pending = self.pending_pings.values().copied().min();
            }
            return WsPongResult::PongReceived(Some(sent_at.elapsed()));
        }

        WsPongResult::NotPong
    }
}

impl<CF, PF> WsPingPongStrategy for WsApplicationPingPong<CF, PF>
where
    CF: Fn() -> (Bytes, String) + Send + Sync + 'static,
    PF: Fn(&Bytes) -> Option<String> + Send + Sync + 'static,
{
    fn create_ping(&mut self) -> Option<WsFrame> {
        self.cleanup_stale();
        if self.pending_pings.len() >= self.max_pending {
            return None;
        }

        let (payload, key) = (self.create_message)();
        let now = Instant::now();
        self.pending_pings.insert(key, now);
        self.touch_oldest_on_insert(now);

        Some(into_ws_frame(payload))
    }

    fn handle_inbound(&mut self, message: &WsFrame) -> WsPongResult {
        match message {
            WsFrame::Text(text) => self.process_application(text.as_bytes()),
            WsFrame::Binary(bytes) => self.process_application(bytes),
            _ => WsPongResult::NotPong,
        }
    }

    fn is_stale(&self) -> bool {
        self.oldest_pending
            .map(|oldest| oldest.elapsed() > self.timeout)
            .unwrap_or(false)
    }

    fn reset(&mut self) {
        self.pending_pings.clear();
        self.oldest_pending = None;
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn protocol_ping_pong_reports_rtt_and_replies_to_ping() {
        let mut strategy = ProtocolPingPong::new(Duration::from_secs(5), Duration::from_secs(10));

        let ping_message = strategy.create_ping().expect("ping should be generated");
        assert!(matches!(ping_message, WsFrame::Ping(_)));

        match strategy.handle_inbound(&WsFrame::Ping(Bytes::from_static(b"payload"))) {
            WsPongResult::Reply(WsFrame::Pong(p)) => assert_eq!(p.as_ref(), b"payload"),
            other => panic!("expected reply pong, got {other:?}"),
        }

        let result = strategy.handle_inbound(&WsFrame::Pong(Bytes::new()));
        match result {
            WsPongResult::PongReceived(rtt) => assert!(rtt.is_some()),
            other => panic!("expected pong received, got {other:?}"),
        }

        assert!(!strategy.is_stale());

        strategy.last_ping = Some(Instant::now() - Duration::from_secs(20));
        strategy.last_pong = None;
        assert!(strategy.is_stale());

        strategy.reset();
        assert!(!strategy.is_stale());
    }

    #[test]
    fn application_ping_pong_tracks_pending_and_applies_limits() {
        let counter = Arc::new(AtomicUsize::new(0));
        let create_counter = counter.clone();
        let mut strategy = WsApplicationPingPong::new(
            Duration::from_secs(1),
            Duration::from_secs(2),
            move || {
                let id = create_counter.fetch_add(1, Ordering::Relaxed);
                let key = format!("req-{id}");
                (Bytes::from(key.clone()), key)
            },
            |bytes: &Bytes| {
                std::str::from_utf8(bytes.as_ref())
                    .ok()
                    .map(|s| s.to_string())
            },
        )
        .with_max_pending(1);

        let outbound = strategy
            .create_ping()
            .expect("first ping should be allowed");
        assert_eq!(strategy.pending_pings.len(), 1);

        let second = strategy.create_ping();
        assert!(second.is_none(), "max pending should block new ping");

        let pong = match &outbound {
            WsFrame::Text(_) | WsFrame::Binary(_) => outbound.clone(),
            other => panic!("expected text/binary payload, got {other:?}"),
        };

        match strategy.handle_inbound(&pong) {
            WsPongResult::PongReceived(Some(_)) => {}
            other => panic!("expected pong received with RTT, got {other:?}"),
        }

        assert_eq!(strategy.pending_pings.len(), 0);
        assert!(!strategy.is_stale());

        strategy.reset();
        assert!(strategy.pending_pings.is_empty());
        assert!(strategy.oldest_pending.is_none());
    }

    #[test]
    fn application_ping_pong_cleans_up_stale_entries() {
        let counter = Arc::new(AtomicUsize::new(0));
        let create_counter = counter.clone();
        let mut strategy = WsApplicationPingPong::new(
            Duration::from_millis(5),
            Duration::from_millis(10),
            move || {
                let id = create_counter.fetch_add(1, Ordering::Relaxed);
                let key = format!("req-{id}");
                (Bytes::from(key.clone()), key)
            },
            |bytes: &Bytes| {
                std::str::from_utf8(bytes.as_ref())
                    .ok()
                    .map(|s| s.to_string())
            },
        );

        let _ = strategy.create_ping();
        assert_eq!(strategy.pending_pings.len(), 1);

        std::thread::sleep(Duration::from_millis(15));

        assert!(strategy.is_stale());

        strategy.create_ping();
        assert_eq!(strategy.pending_pings.len(), 1);
        assert!(!strategy.is_stale());

        strategy.reset();
        assert!(strategy.pending_pings.is_empty());
        assert!(strategy.oldest_pending.is_none());
    }
}
