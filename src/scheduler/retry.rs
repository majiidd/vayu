//! Retry budget and backoff.
//!
//! * `auto`: keep retrying until 10 minutes pass without useful progress.
//! * `N`: give up after N consecutive failed attempts that made no useful progress.
//! * `forever`: never give up on retryable failures (still cancellable).
//!
//! Useful progress (at least [`PROGRESS_BYTES`] in an attempt) resets the budget. Delays grow
//! exponentially from about one second to one minute with jitter, and honor `Retry-After`.

use std::time::{Duration, Instant};

use crate::cli::RetryMode;

pub const AUTO_BUDGET: Duration = Duration::from_secs(10 * 60);
pub const PROGRESS_BYTES: u64 = 32 * 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

pub struct RetryState {
    mode: RetryMode,
    consecutive: u32,
    last_progress: Instant,
    pub attempts: u32,
}

impl RetryState {
    pub fn new(mode: RetryMode, now: Instant) -> RetryState {
        RetryState {
            mode,
            consecutive: 0,
            last_progress: now,
            attempts: 0,
        }
    }

    /// Record a failed attempt. Returns the delay before the next one, or `None` to give up.
    pub fn on_failure(
        &mut self,
        progressed: bool,
        retry_after: Option<Duration>,
        now: Instant,
    ) -> Option<Duration> {
        if progressed {
            self.consecutive = 0;
            self.last_progress = now;
        }
        self.consecutive += 1;
        self.attempts += 1;
        let exhausted = match self.mode {
            RetryMode::Count(limit) => self.consecutive > limit,
            RetryMode::Auto => now.duration_since(self.last_progress) >= AUTO_BUDGET,
            RetryMode::Forever => false,
        };
        if exhausted {
            return None;
        }
        let exponent = (self.consecutive - 1).min(6);
        let base = Duration::from_secs(1 << exponent).min(MAX_BACKOFF);
        let mut delay = base.mul_f64(0.5 + fastrand::f64() * 0.5);
        if let Some(hint) = retry_after {
            delay = delay.max(hint.min(MAX_RETRY_AFTER));
        }
        if self.mode == RetryMode::Auto {
            let left = AUTO_BUDGET.saturating_sub(now.duration_since(self.last_progress));
            delay = delay.min(left.max(Duration::from_secs(1)));
        }
        Some(delay)
    }

    pub fn describe_exhausted(&self) -> String {
        match self.mode {
            RetryMode::Count(0) => "retries are disabled (--retry 0)".into(),
            RetryMode::Count(n) => format!("gave up after {n} retries without progress"),
            RetryMode::Auto => "gave up after 10 minutes without progress".into(),
            RetryMode::Forever => "gave up".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted_retries_reset_on_progress() {
        let t = Instant::now();
        let mut r = RetryState::new(RetryMode::Count(2), t);
        assert!(r.on_failure(false, None, t).is_some());
        assert!(r.on_failure(false, None, t).is_some());
        assert!(r.on_failure(false, None, t).is_none());
        let mut r = RetryState::new(RetryMode::Count(2), t);
        for _ in 0..10 {
            assert!(
                r.on_failure(true, None, t).is_some(),
                "progress resets the count"
            );
        }
        assert!(
            RetryState::new(RetryMode::Count(0), t)
                .on_failure(false, None, t)
                .is_none()
        );
    }

    #[test]
    fn auto_budget_is_time_without_progress() {
        let t = Instant::now();
        let mut r = RetryState::new(RetryMode::Auto, t);
        for i in 0..50 {
            let now = t + Duration::from_secs(i * 10);
            assert!(r.on_failure(false, None, now).is_some());
        }
        assert!(r.on_failure(false, None, t + AUTO_BUDGET).is_none());
        let mut r = RetryState::new(RetryMode::Auto, t);
        assert!(r.on_failure(true, None, t + AUTO_BUDGET * 3).is_some());
    }

    #[test]
    fn backoff_grows_with_jitter_and_cap() {
        let t = Instant::now();
        let mut r = RetryState::new(RetryMode::Forever, t);
        let first = r.on_failure(false, None, t).unwrap();
        assert!(first >= Duration::from_millis(500) && first <= Duration::from_secs(1));
        let mut last = Duration::ZERO;
        for _ in 0..20 {
            last = r.on_failure(false, None, t).unwrap();
        }
        assert!(last >= Duration::from_secs(30) && last <= MAX_BACKOFF);
    }

    #[test]
    fn honors_retry_after() {
        let t = Instant::now();
        let mut r = RetryState::new(RetryMode::Forever, t);
        let d = r
            .on_failure(false, Some(Duration::from_secs(120)), t)
            .unwrap();
        assert_eq!(d, Duration::from_secs(120));
        let d = r
            .on_failure(false, Some(Duration::from_secs(86_400)), t)
            .unwrap();
        assert_eq!(d, MAX_RETRY_AFTER);
    }
}
