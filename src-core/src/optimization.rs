//! Resource optimization helpers: rate limiter, streaming, bounded concurrency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

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

    /// Try to consume `bytes` tokens. Returns the delay needed (if any).
    /// If rate is 0 (unlimited), returns immediately.
    pub async fn consume(&self, bytes: u64) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return; // unlimited
        }

        loop {
            self.refill();
            let available = self.tokens.load(Ordering::Relaxed);
            if available >= bytes {
                self.tokens.fetch_sub(bytes, Ordering::Relaxed);
                return;
            }
            // Need to wait for more tokens
            let deficit = bytes - available;
            let wait_ms = (deficit as f64 / rate as f64 * 1000.0).ceil() as u64;
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms.min(100))).await;
            self.refill();
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
}
