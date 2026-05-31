use crate::error::CoreError;
use crate::s3::error::is_retryable;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

/// Retry policy with exponential backoff and jitter.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    /// Create a new retry policy.
    pub fn new(max_attempts: u32, base_delay_secs: u64, max_delay_secs: u64) -> Self {
        Self {
            max_attempts,
            base_delay: Duration::from_secs(base_delay_secs),
            max_delay: Duration::from_secs(max_delay_secs),
        }
    }

    /// Calculate the delay for attempt `n` (0-indexed).
    /// Uses exponential backoff: base_delay * 2^n, capped at max_delay, with jitter.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = self.base_delay_for_attempt(attempt);
        let base_ms = base.as_millis();
        if base_ms == 0 {
            return base;
        }

        let max_ms = self.max_delay.as_millis();
        let jitter_window = (base_ms / 4).max(1);
        let lower = base_ms.saturating_sub(jitter_window);
        let upper = base_ms.saturating_add(jitter_window).min(max_ms);
        let span = upper.saturating_sub(lower).saturating_add(1);
        let jitter = jitter_seed(attempt) % span;

        Duration::from_millis((lower + jitter) as u64)
    }

    /// Determine if we should retry based on the error and attempt count.
    pub fn should_retry(&self, err: &CoreError, attempt: u32) -> bool {
        if attempt >= self.max_attempts {
            return false;
        }
        is_retryable(err)
    }

    fn base_delay_for_attempt(&self, attempt: u32) -> Duration {
        let multiplier = 1u128.checked_shl(attempt.min(63)).unwrap_or(u128::MAX);
        let delay_ms = self.base_delay.as_millis().saturating_mul(multiplier);
        let capped = delay_ms.min(self.max_delay.as_millis());
        Duration::from_millis(capped as u64)
    }
}

fn jitter_seed(attempt: u32) -> u128 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos();
    now ^ ((attempt as u128) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;

    #[test]
    fn test_backoff_delays() {
        let policy = RetryPolicy::new(3, 1, 60);

        assert_delay_near(policy.delay_for_attempt(0), 1_000);
        assert_delay_near(policy.delay_for_attempt(1), 2_000);
        assert_delay_near(policy.delay_for_attempt(2), 4_000);
    }

    #[test]
    fn test_backoff_capped() {
        let policy = RetryPolicy::new(5, 10, 30);

        assert_delay_near(policy.delay_for_attempt(0), 10_000);
        assert_delay_near(policy.delay_for_attempt(1), 20_000);
        assert!(policy.delay_for_attempt(2) <= Duration::from_secs(30));
        assert!(policy.delay_for_attempt(3) <= Duration::from_secs(30));
    }

    #[test]
    fn test_should_retry() {
        let policy = RetryPolicy::new(3, 1, 60);

        let timeout = CoreError::Network("timeout".into());
        assert!(policy.should_retry(&timeout, 0));
        assert!(policy.should_retry(&timeout, 2));
        assert!(!policy.should_retry(&timeout, 3)); // max attempts

        let auth = CoreError::Auth("denied".into());
        assert!(!policy.should_retry(&auth, 0));
    }

    #[test]
    fn test_default_policy() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.base_delay, Duration::from_secs(1));
        assert_eq!(policy.max_delay, Duration::from_secs(60));
    }

    fn assert_delay_near(delay: Duration, expected_ms: u128) {
        let actual = delay.as_millis();
        let lower = expected_ms.saturating_sub(expected_ms / 4);
        let upper = expected_ms + expected_ms / 4;
        assert!(
            (lower..=upper).contains(&actual),
            "delay {}ms outside expected jitter range {}..={}ms",
            actual,
            lower,
            upper
        );
    }
}
