use crate::error::CoreError;
use crate::s3::error::is_retryable;
use std::time::Duration;

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
    /// Uses exponential backoff: base_delay * 2^n, capped at max_delay.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let exp = 2u64.pow(attempt);
        let delay = self.base_delay.as_secs().saturating_mul(exp);
        let capped = std::cmp::min(delay, self.max_delay.as_secs());
        Duration::from_secs(capped)
    }

    /// Determine if we should retry based on the error and attempt count.
    pub fn should_retry(&self, err: &CoreError, attempt: u32) -> bool {
        if attempt >= self.max_attempts {
            return false;
        }
        is_retryable(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;

    #[test]
    fn test_backoff_delays() {
        let policy = RetryPolicy::new(3, 1, 60);

        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(1));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(4));
    }

    #[test]
    fn test_backoff_capped() {
        let policy = RetryPolicy::new(5, 10, 30);

        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(10));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(20));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(30)); // capped
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(30)); // capped
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
}
