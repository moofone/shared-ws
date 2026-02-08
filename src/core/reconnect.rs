use std::time::Duration;

use super::types::WsReconnectStrategy;

/// Simple exponential backoff reconnect strategy.
///
/// This keeps policy in the reconnect strategy (not in the websocket actor):
/// callers can select base/max/factor per exchange.
#[derive(Clone, Debug)]
pub struct ExponentialBackoffReconnect {
    base: Duration,
    max: Duration,
    factor: f64,
    current: Duration,
    retry: bool,
}

impl ExponentialBackoffReconnect {
    pub fn new(base: Duration, max: Duration, factor: f64) -> Self {
        let factor = if factor.is_finite() && factor > 1.0 {
            factor
        } else {
            1.5
        };
        Self {
            base,
            max,
            factor,
            current: base,
            retry: true,
        }
    }

    pub fn abort(mut self) -> Self {
        self.retry = false;
        self
    }
}

impl Default for ExponentialBackoffReconnect {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30), 1.5)
    }
}

impl WsReconnectStrategy for ExponentialBackoffReconnect {
    fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        let next = (self.current.as_secs_f64() * self.factor).min(self.max.as_secs_f64());
        self.current = Duration::from_secs_f64(next);
        delay
    }

    fn reset(&mut self) {
        self.current = self.base;
    }

    fn should_retry(&self) -> bool {
        self.retry
    }
}
