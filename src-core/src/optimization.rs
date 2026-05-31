//! Resource optimization helpers: rate limiter, streaming, bounded concurrency,
//! idle polling backoff, and bounded LRU cache policy.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Token bucket rate limiter for bandwidth control.
///
/// Refills tokens at `rate` bytes per second.
/// Each transfer consumes tokens equal to bytes transferred.
/// When tokens are exhausted, transfers are rate-limited.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    rate: Arc<AtomicU64>,   // bytes/sec
    tokens: Arc<AtomicU64>, // available tokens
    last_refill: Arc<std::sync::Mutex<Instant>>,
    max_burst: Arc<std::sync::Mutex<u64>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(0) // 0 = unlimited
    }
}

impl RateLimiter {
    /// Create a new rate limiter with `bytes_per_sec` limit.
    /// Use 0 for unlimited.
    pub fn new(bytes_per_sec: u64) -> Self {
        let burst = bytes_per_sec.max(1024 * 1024);
        Self {
            rate: Arc::new(AtomicU64::new(bytes_per_sec)),
            tokens: Arc::new(AtomicU64::new(if bytes_per_sec == 0 {
                u64::MAX
            } else {
                bytes_per_sec
            })),
            last_refill: Arc::new(std::sync::Mutex::new(Instant::now())),
            max_burst: Arc::new(std::sync::Mutex::new(burst)),
        }
    }

    /// Set the rate limit in bytes per second. 0 = unlimited.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
        if bytes_per_sec == 0 {
            self.tokens.store(u64::MAX, Ordering::Relaxed);
        } else {
            self.tokens.store(bytes_per_sec, Ordering::Relaxed);
        }
        if let Ok(mut burst) = self.max_burst.lock() {
            *burst = bytes_per_sec.max(1024 * 1024);
        }
    }

    /// Try to consume `bytes` tokens. Blocks until enough tokens are available.
    /// If rate is 0 (unlimited), returns immediately.
    /// Handles arbitrary `bytes` values — consumes in burst-sized chunks internally
    /// to avoid deadlock when `bytes` exceeds the burst cap.
    pub async fn consume(&self, bytes: u64) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return; // unlimited
        }

        let mut remaining = bytes;
        while remaining > 0 {
            self.refill();
            let available = self.tokens.load(Ordering::Relaxed);
            let take = available.min(remaining);
            if take > 0 {
                self.tokens.fetch_sub(take, Ordering::Relaxed);
                remaining -= take;
            }
            if remaining > 0 {
                let wait_ms = (remaining as f64 / rate as f64 * 1000.0).ceil() as u64;
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms.min(100))).await;
            }
        }
    }

    fn refill(&self) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        if let Ok(mut last) = self.last_refill.lock() {
            let elapsed = last.elapsed().as_secs_f64();
            if elapsed > 0.01 {
                let new_tokens = (elapsed * rate as f64) as u64;
                let current = self.tokens.load(Ordering::Relaxed);
                let burst = *self.max_burst.lock().unwrap_or_else(|e| e.into_inner());
                let after = current.saturating_add(new_tokens).min(burst);
                self.tokens.store(after, Ordering::Relaxed);
                *last = Instant::now();
            }
        }
    }
}

/// Threshold for streaming (multipart) upload, in bytes.
/// Files larger than this use multipart upload.
pub const STREAMING_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MB

/// Chunk size for streaming upload.
pub const STREAMING_CHUNK_SIZE: u64 = 5 * 1024 * 1024; // 5 MB

/// Decide if a file should use streaming upload based on size.
pub fn should_stream(size: u64) -> bool {
    size > STREAMING_THRESHOLD
}

/// Number of concurrent transfer slots (read from config).
pub const DEFAULT_MAX_CONCURRENT: u32 = 4;
pub const MAX_CONCURRENT_LIMIT: u32 = 32;

/// Clamp concurrency to valid range.
pub fn clamp_concurrency(val: u32) -> u32 {
    val.clamp(1, MAX_CONCURRENT_LIMIT)
}

/// Idle polling backoff for remote reconciliation.
///
/// Phase 8 requires the exact sequence 30s -> 60s -> 120s -> 300s while the
/// sync loop is idle, and a reset as soon as any local or remote work happens.
#[derive(Debug, Clone)]
pub struct IdleBackoff {
    steps: [Duration; 4],
    idle_cycles: usize,
}

impl Default for IdleBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleBackoff {
    pub fn new() -> Self {
        Self {
            steps: [
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(300),
            ],
            idle_cycles: 0,
        }
    }

    pub fn reset(&mut self) {
        self.idle_cycles = 0;
    }

    pub fn current_delay(&self) -> Duration {
        self.steps[self.idle_cycles.min(self.steps.len() - 1)]
    }

    pub fn next_idle_delay(&mut self) -> Duration {
        let delay = self.current_delay();
        self.idle_cycles = self.idle_cycles.saturating_add(1);
        delay
    }
}

/// Simple adaptive concurrency controller for transfer workers.
#[derive(Debug, Clone)]
pub struct AdaptiveConcurrency {
    current: u32,
    min: u32,
    max: u32,
    target_latency: Duration,
}

impl AdaptiveConcurrency {
    pub fn new(configured: u32) -> Self {
        Self {
            current: clamp_concurrency(configured),
            min: 1,
            max: MAX_CONCURRENT_LIMIT,
            target_latency: Duration::from_secs(2),
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    pub fn record_success(&mut self, latency: Duration) {
        if latency <= self.target_latency && self.current < self.max {
            self.current += 1;
        } else if latency > self.target_latency.saturating_mul(3) {
            self.current = self.current.saturating_sub(1).max(self.min);
        }
    }

    pub fn record_failure(&mut self) {
        self.current = (self.current / 2).max(self.min);
    }
}

/// Metadata for an item in a bounded local cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub key: String,
    pub size_bytes: u64,
    pub last_access_unix_ms: i64,
}

/// Return keys that should be evicted to satisfy both item and byte budgets.
pub fn lru_eviction_candidates(
    entries: &[CacheEntry],
    max_items: usize,
    max_bytes: u64,
) -> Vec<String> {
    let mut newest_first = entries.to_vec();
    newest_first.sort_by(|a, b| {
        b.last_access_unix_ms
            .cmp(&a.last_access_unix_ms)
            .then_with(|| a.key.cmp(&b.key))
    });

    let mut kept_items = 0usize;
    let mut kept_bytes = 0u64;
    let mut evicted = Vec::new();

    for entry in newest_first {
        let fits_items = kept_items < max_items;
        let fits_bytes = kept_bytes.saturating_add(entry.size_bytes) <= max_bytes;
        if fits_items && fits_bytes {
            kept_items += 1;
            kept_bytes = kept_bytes.saturating_add(entry.size_bytes);
        } else {
            evicted.push(entry.key);
        }
    }

    evicted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_stream() {
        let small = 1024u64; // 1KB
        let large = 20 * 1024 * 1024u64; // 20MB
        assert!(!should_stream(small));
        assert!(should_stream(large));
        assert!(!should_stream(STREAMING_THRESHOLD));
        assert!(should_stream(STREAMING_THRESHOLD + 1));
    }

    #[test]
    fn test_clamp_concurrency() {
        assert_eq!(clamp_concurrency(0), 1);
        assert_eq!(clamp_concurrency(4), 4);
        assert_eq!(clamp_concurrency(100), 32);
    }

    #[tokio::test]
    async fn test_rate_limiter_unlimited() {
        let limiter = RateLimiter::new(0);
        limiter.consume(1_000_000).await; // should not block
    }

    #[tokio::test]
    async fn test_rate_limiter_basic() {
        let limiter = RateLimiter::new(1024 * 1024); // 1MB/s
                                                     // Small consumption should not block
        tokio::time::timeout(std::time::Duration::from_millis(100), limiter.consume(1024))
            .await
            .expect("should not timeout on small consume");
    }

    #[test]
    fn test_set_rate() {
        let limiter = RateLimiter::new(1024);
        limiter.set_rate(0);
        // unlimited — no need to wait
        let rate = limiter.rate.load(Ordering::Relaxed);
        assert_eq!(rate, 0);
    }

    #[test]
    fn idle_backoff_uses_required_phase8_sequence() {
        let mut backoff = IdleBackoff::new();
        assert_eq!(backoff.next_idle_delay(), Duration::from_secs(30));
        assert_eq!(backoff.next_idle_delay(), Duration::from_secs(60));
        assert_eq!(backoff.next_idle_delay(), Duration::from_secs(120));
        assert_eq!(backoff.next_idle_delay(), Duration::from_secs(300));
        assert_eq!(backoff.next_idle_delay(), Duration::from_secs(300));

        backoff.reset();
        assert_eq!(backoff.next_idle_delay(), Duration::from_secs(30));
    }

    #[test]
    fn adaptive_concurrency_grows_and_backs_off() {
        let mut controller = AdaptiveConcurrency::new(4);
        controller.record_success(Duration::from_millis(200));
        assert_eq!(controller.current(), 5);

        controller.record_failure();
        assert_eq!(controller.current(), 2);

        controller.record_failure();
        controller.record_failure();
        assert_eq!(controller.current(), 1);
    }

    #[test]
    fn lru_eviction_respects_item_and_byte_budgets() {
        let entries = vec![
            CacheEntry {
                key: "old".into(),
                size_bytes: 10,
                last_access_unix_ms: 1,
            },
            CacheEntry {
                key: "new".into(),
                size_bytes: 10,
                last_access_unix_ms: 3,
            },
            CacheEntry {
                key: "middle".into(),
                size_bytes: 10,
                last_access_unix_ms: 2,
            },
        ];

        let evicted = lru_eviction_candidates(&entries, 2, 20);
        assert_eq!(evicted, vec!["old"]);

        let evicted = lru_eviction_candidates(&entries, 3, 15);
        assert_eq!(evicted, vec!["middle", "old"]);
    }
}
