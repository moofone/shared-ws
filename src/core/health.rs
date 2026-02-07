use hdrhistogram::Histogram;
use std::time::{Duration, Instant};

use super::circular_buffer::CircularBuffer;
use super::types::WsConnectionStats;

const MAX_RECENT_ERRORS: usize = 100;
const MAX_ERROR_TEXT_BYTES: usize = 1024;

#[derive(Debug, Clone)]
struct ServerErrorRec {
    _timestamp: Instant,
    _code: Option<i32>,
    _message: String,
}

#[derive(Debug, Clone)]
struct InternalErrorRec {
    _timestamp: Instant,
    _context: String,
    _error: String,
}

fn truncate_string(s: &str) -> String {
    if s.len() <= MAX_ERROR_TEXT_BYTES {
        return s.to_string();
    }

    let mut end = MAX_ERROR_TEXT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Health monitor tracking websocket connection state without interior mutability.
#[derive(Debug)]
pub struct WsHealthMonitor {
    connection_started: Instant,
    last_message_received: Instant,
    last_message_sent: Instant,
    stale_threshold: Duration,
    message_count: u64,
    error_count: u64,
    reconnect_count: u64,
    server_errors: CircularBuffer<ServerErrorRec>,
    internal_errors: CircularBuffer<InternalErrorRec>,
    latency_histogram: Histogram<u64>,
}

impl WsHealthMonitor {
    pub fn new(stale_threshold: Duration) -> Self {
        let now = Instant::now();
        Self {
            connection_started: now,
            last_message_received: now,
            last_message_sent: now,
            stale_threshold,
            message_count: 0,
            error_count: 0,
            reconnect_count: 0,
            server_errors: CircularBuffer::new(MAX_RECENT_ERRORS),
            internal_errors: CircularBuffer::new(MAX_RECENT_ERRORS),
            latency_histogram: Histogram::new_with_bounds(1, 1_000_000, 3)
                .expect("histogram bounds are valid"),
        }
    }

    pub fn reset(&mut self) {
        let now = Instant::now();
        self.connection_started = now;
        self.last_message_received = now;
        self.last_message_sent = now;
    }

    pub fn record_message(&mut self) {
        self.last_message_received = Instant::now();
        self.message_count = self.message_count.saturating_add(1);
    }

    pub fn record_inbound_frames(&mut self, frames: u64) {
        if frames == 0 {
            return;
        }
        self.last_message_received = Instant::now();
        self.message_count = self.message_count.saturating_add(frames);
    }

    pub fn record_sent(&mut self) {
        self.last_message_sent = Instant::now();
    }

    pub fn record_error(&mut self) {
        self.error_count = self.error_count.saturating_add(1);
    }

    pub fn record_server_error(&mut self, code: Option<i32>, message: &str) {
        self.record_error();
        self.server_errors.push(ServerErrorRec {
            _timestamp: Instant::now(),
            _code: code,
            _message: truncate_string(message),
        });
    }

    pub fn record_internal_error(&mut self, context: &str, error: &str) {
        self.record_error();
        self.internal_errors.push(InternalErrorRec {
            _timestamp: Instant::now(),
            _context: truncate_string(context),
            _error: truncate_string(error),
        });
    }

    pub fn record_rtt(&mut self, latency: Duration) {
        let micros = latency.as_micros().min(u64::MAX as u128) as u64;
        let _ = self.latency_histogram.record(micros);
    }

    pub fn increment_reconnect(&mut self) {
        self.reconnect_count = self.reconnect_count.saturating_add(1);
    }

    pub fn is_stale(&self) -> bool {
        self.last_message_received.elapsed() > self.stale_threshold
    }

    pub fn clear_error_buffers(&mut self) {
        self.server_errors.clear();
        self.internal_errors.clear();
    }

    pub fn get_stats(&self) -> WsConnectionStats {
        let latency_samples = self.latency_histogram.len();
        let (p50, p99) = if latency_samples == 0 {
            (0, 0)
        } else {
            (
                self.latency_histogram.value_at_percentile(50.0),
                self.latency_histogram.value_at_percentile(99.0),
            )
        };

        WsConnectionStats {
            uptime: self.connection_started.elapsed(),
            messages: self.message_count,
            errors: self.error_count,
            reconnects: self.reconnect_count,
            last_message_age: self.last_message_received.elapsed(),
            recent_server_errors: self.server_errors.len(),
            recent_internal_errors: self.internal_errors.len(),
            p50_latency_us: p50,
            p99_latency_us: p99,
            latency_samples,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_monitor_records_rtt_and_percentiles() {
        let mut monitor = WsHealthMonitor::new(Duration::from_secs(5));
        monitor.record_rtt(Duration::from_micros(100));
        monitor.record_rtt(Duration::from_micros(200));
        monitor.record_rtt(Duration::from_micros(300));

        let stats = monitor.get_stats();
        assert_eq!(stats.latency_samples, 3);
        assert_eq!(stats.p50_latency_us, 200);
        assert_eq!(stats.p99_latency_us, 300);
    }

    #[test]
    fn health_monitor_truncates_error_buffers() {
        let mut monitor = WsHealthMonitor::new(Duration::from_secs(5));

        for i in 0..105 {
            monitor.record_server_error(Some(i), "server error");
        }

        assert_eq!(monitor.server_errors.len(), 100);
        assert_eq!(monitor.server_errors.front().unwrap()._code, Some(5));
        assert_eq!(monitor.error_count, 105);

        monitor.clear_error_buffers();
        assert!(monitor.server_errors.is_empty());

        for i in 0..105 {
            monitor.record_internal_error("ctx", &format!("error-{i}"));
        }

        assert_eq!(monitor.internal_errors.len(), 100);
        assert_eq!(monitor.internal_errors.front().unwrap()._error, "error-5");
        assert_eq!(monitor.error_count, 210);
    }

    #[test]
    fn health_monitor_detects_stale_connections() {
        let mut monitor = WsHealthMonitor::new(Duration::from_secs(1));
        assert!(!monitor.is_stale());

        monitor.last_message_received = monitor
            .last_message_received
            .checked_sub(Duration::from_secs(10))
            .unwrap();

        assert!(monitor.is_stale());
    }

    #[test]
    fn health_monitor_caps_error_string_sizes() {
        let mut monitor = WsHealthMonitor::new(Duration::from_secs(5));
        let huge = "x".repeat(MAX_ERROR_TEXT_BYTES + 10);
        monitor.record_server_error(None, &huge);
        assert_eq!(
            monitor.server_errors.front().unwrap()._message.len(),
            MAX_ERROR_TEXT_BYTES
        );
    }
}
