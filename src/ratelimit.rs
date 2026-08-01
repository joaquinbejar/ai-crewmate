//! Per-token request rate limiting for the MCP surface.
//!
//! A token bucket per presented credential, keyed on the token's hash so a
//! flood of *invalid* tokens is rejected without a database round-trip too.
//!
//! **This limiter is per process.** The bus is deliberately stateless and
//! horizontally scalable, so with N replicas the effective ceiling is N times
//! the configured rate. That is the honest trade: a shared limiter would need
//! shared state on every request, which contradicts "one binary, one database".
//! When a hard global ceiling matters, put it in the reverse proxy — this
//! limiter is the in-process backstop that keeps one runaway agent from
//! saturating the instance it is talking to.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Entries idle for longer than this are dropped, so the map tracks active
/// callers rather than every token ever presented.
const IDLE_EVICTION: Duration = Duration::from_secs(600);

/// How often a request triggers a sweep of idle entries. Cheap amortisation:
/// no background task, no timer, no extra thread.
const SWEEP_EVERY: usize = 512;

struct Bucket {
    tokens: f64,
    last: Instant,
}

struct Inner {
    buckets: HashMap<String, Bucket>,
    since_sweep: usize,
}

/// Token bucket rate limiter. Cloning shares the same state.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<Inner>>,
    /// Sustained rate, requests per second.
    refill_per_sec: f64,
    /// Bucket capacity: how large a burst is tolerated before throttling.
    burst: f64,
}

/// How long a throttled caller should wait, for the `Retry-After` header.
pub struct Throttled {
    pub retry_after_secs: u64,
}

impl RateLimiter {
    /// `per_minute` of 0 disables limiting entirely (`new` returns `None`).
    pub fn new(per_minute: u32) -> Option<Self> {
        if per_minute == 0 {
            return None;
        }
        // A burst of a tenth of the minute's budget (at least 10) absorbs the
        // bursty shape of agent traffic — a session start fires whoami,
        // team_digest and read_messages back to back — without letting a loop
        // sustain more than the configured rate.
        let burst = ((per_minute as f64) / 10.0).max(10.0);
        Some(Self {
            inner: Arc::new(Mutex::new(Inner {
                buckets: HashMap::new(),
                since_sweep: 0,
            })),
            refill_per_sec: per_minute as f64 / 60.0,
            burst,
        })
    }

    /// Charge one request against `key`. `Err(Throttled)` means reject.
    pub fn check(&self, key: &str) -> Result<(), Throttled> {
        let now = Instant::now();
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            // A poisoned lock means another thread panicked while holding it.
            // Failing open is the right call: the limiter is a backstop, not
            // an authorization decision.
            Err(poisoned) => poisoned.into_inner(),
        };

        inner.since_sweep += 1;
        if inner.since_sweep >= SWEEP_EVERY {
            inner.since_sweep = 0;
            inner
                .buckets
                .retain(|_, b| now.duration_since(b.last) < IDLE_EVICTION);
        }

        let burst = self.burst;
        let refill = self.refill_per_sec;
        let bucket = inner.buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: burst,
            last: now,
        });

        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill).min(burst);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }

        // Time until one whole token is available again.
        let deficit = 1.0 - bucket.tokens;
        Err(Throttled {
            retry_after_secs: (deficit / refill).ceil().max(1.0) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_zero() {
        assert!(RateLimiter::new(0).is_none());
    }

    #[test]
    fn allows_a_burst_then_throttles() {
        // 60/min = 1/s sustained, burst 10.
        let rl = RateLimiter::new(60).expect("enabled");
        for i in 0..10 {
            assert!(rl.check("k").is_ok(), "burst request {i} should pass");
        }
        let throttled = rl.check("k");
        assert!(
            throttled.is_err(),
            "the 11th immediate request is throttled"
        );
        assert!(throttled.expect_err("throttled").retry_after_secs >= 1);
    }

    #[test]
    fn keys_are_independent() {
        let rl = RateLimiter::new(60).expect("enabled");
        for _ in 0..10 {
            let _ = rl.check("a");
        }
        assert!(
            rl.check("b").is_ok(),
            "one caller's budget must not affect another's"
        );
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new(600).expect("enabled"); // 10/s, burst 60
        for _ in 0..60 {
            let _ = rl.check("k");
        }
        assert!(rl.check("k").is_err(), "bucket is drained");
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            rl.check("k").is_ok(),
            "a quarter second refills 2.5 tokens at 10/s"
        );
    }
}
