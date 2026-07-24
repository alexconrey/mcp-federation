use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::config::RateLimitConfig;

/// Classic token bucket. `tokens` is refilled at `refill_rate` per second up to
/// `max_tokens`; each `try_acquire` costs one token.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(max_tokens: f64, refill_rate: f64) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
            self.last_refill = now;
        }
    }

    fn try_acquire(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-key token-bucket rate limiter. Buckets are keyed by client token
/// (or `"anonymous"` when no auth header was presented). A single mutex guards
/// the map plus every bucket — request rates are already bounded by the leaves,
/// so lock contention is not a concern at expected throughput.
pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Consume one token for `key`. Returns `true` if the request may proceed,
    /// `false` if the caller has exceeded their bucket. When rate limiting is
    /// disabled this always returns `true` and touches no state.
    pub fn check(&self, key: &str) -> bool {
        if !self.config.enabled {
            return true;
        }
        let now = Instant::now();
        let max = self.config.burst_size as f64;
        let rate = self.config.requests_per_second as f64;
        let mut buckets = self.buckets.lock().expect("rate limiter mutex poisoned");
        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(max, rate));
        bucket.try_acquire(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, rps: u32, burst: u32) -> RateLimitConfig {
        RateLimitConfig {
            enabled,
            requests_per_second: rps,
            burst_size: burst,
        }
    }

    #[test]
    fn disabled_limiter_never_blocks() {
        let limiter = RateLimiter::new(cfg(false, 1, 1));
        for _ in 0..1000 {
            assert!(limiter.check("anyone"));
        }
    }

    #[test]
    fn enabled_limiter_allows_burst_then_rejects() {
        let limiter = RateLimiter::new(cfg(true, 1, 5));
        for _ in 0..5 {
            assert!(limiter.check("team-a"));
        }
        // Bucket exhausted; no measurable time has passed so refill is ~0.
        assert!(!limiter.check("team-a"));
    }

    #[test]
    fn buckets_are_per_key() {
        let limiter = RateLimiter::new(cfg(true, 1, 2));
        assert!(limiter.check("a"));
        assert!(limiter.check("a"));
        assert!(!limiter.check("a"));
        // Different key has its own bucket.
        assert!(limiter.check("b"));
        assert!(limiter.check("b"));
        assert!(!limiter.check("b"));
    }

    #[test]
    fn refill_restores_tokens_over_time() {
        let limiter = RateLimiter::new(cfg(true, 100, 2));
        assert!(limiter.check("k"));
        assert!(limiter.check("k"));
        assert!(!limiter.check("k"));
        // 100 rps = 10ms per token; sleep 50ms to comfortably refill.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(limiter.check("k"));
    }

    #[test]
    fn refill_saturates_at_max_tokens() {
        let mut bucket = TokenBucket::new(5.0, 100.0);
        bucket.tokens = 0.0;
        // Simulate a long elapsed time — bucket should cap at max, not overflow.
        std::thread::sleep(std::time::Duration::from_millis(200));
        bucket.refill(Instant::now());
        assert!(bucket.tokens <= 5.0);
        assert!(bucket.tokens >= 4.9);
    }
}
