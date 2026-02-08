use rand::{Rng, SeedableRng, rngs::SmallRng};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

/// Connection attempt guard that backs off after repeated failures.
#[derive(Debug, Clone)]
pub struct WsCircuitBreaker {
    state: CircuitState,
    failure_threshold: u32,
    success_threshold: u32,
    timeout: Duration,
    failures: u32,
    successes: u32,
}

impl WsCircuitBreaker {
    pub fn new(failure_threshold: u32, success_threshold: u32, timeout: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            failure_threshold: failure_threshold.max(1),
            success_threshold: success_threshold.max(1),
            timeout,
            failures: 0,
            successes: 0,
        }
    }

    pub fn can_proceed(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open { until } => {
                if Instant::now() >= until {
                    self.state = CircuitState::HalfOpen;
                    self.failures = 0;
                    self.successes = 0;
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true,
        }
    }

    pub fn record_success(&mut self) {
        match self.state {
            CircuitState::HalfOpen => {
                self.successes = self.successes.saturating_add(1);
                if self.successes >= self.success_threshold {
                    self.state = CircuitState::Closed;
                    self.failures = 0;
                    self.successes = 0;
                }
            }
            CircuitState::Closed => {
                self.failures = 0;
            }
            CircuitState::Open { .. } => {}
        }
    }

    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
        match self.state {
            CircuitState::Closed if self.failures >= self.failure_threshold => {
                self.open_circuit();
            }
            CircuitState::HalfOpen => {
                self.open_circuit();
            }
            CircuitState::Open { .. } => {}
            CircuitState::Closed => {}
        }
    }

    pub fn time_until_retry(&self) -> Option<Duration> {
        match self.state {
            CircuitState::Open { until } => until.checked_duration_since(Instant::now()),
            _ => None,
        }
    }

    fn open_circuit(&mut self) {
        self.state = CircuitState::Open {
            until: Instant::now() + self.timeout,
        };
        self.failures = 0;
        self.successes = 0;
    }
}

pub fn jitter_delay(base: Duration) -> Duration {
    if base.is_zero() {
        return base;
    }

    let mut rng = SmallRng::from_entropy();
    let jitter: f64 = rng.gen_range(0.5..=1.0);
    let nanos = (base.as_nanos() as f64 * jitter) as u128;
    Duration::from_nanos(nanos.min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn circuit_breaker_transitions_through_states() {
        let mut breaker = WsCircuitBreaker::new(2, 1, Duration::from_millis(30));

        assert!(breaker.can_proceed());
        breaker.record_failure();
        breaker.record_failure();

        assert!(!breaker.can_proceed());
        let wait = breaker.time_until_retry().expect("breaker should be open");
        assert!(wait <= Duration::from_millis(30));

        thread::sleep(Duration::from_millis(35));
        assert!(breaker.can_proceed());

        breaker.record_success();
        assert!(breaker.can_proceed());

        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.can_proceed());

        thread::sleep(Duration::from_millis(35));
        assert!(breaker.can_proceed());
        breaker.record_failure();
        assert!(!breaker.can_proceed());
    }

    #[test]
    fn jitter_delay_respects_bounds() {
        let base = Duration::from_millis(100);
        for _ in 0..100 {
            let delay = jitter_delay(base);
            assert!(delay >= Duration::from_millis(50));
            assert!(delay <= base);
        }

        assert_eq!(
            jitter_delay(Duration::from_millis(0)),
            Duration::from_millis(0)
        );
    }
}
