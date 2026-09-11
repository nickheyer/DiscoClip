//! Per-host token buckets, so a burst of links to one platform does not get the server
//! throttled or banned there.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Requests allowed per second on average, and how many may go at once when idle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rate {
    pub per_second: f64,
    pub burst: u32,
}

impl Default for Rate {
    fn default() -> Self {
        Self {
            per_second: 4.0,
            burst: 8,
        }
    }
}

impl Rate {
    pub const UNLIMITED: Rate = Rate {
        per_second: 0.0,
        burst: 0,
    };

    pub fn is_unlimited(&self) -> bool {
        self.per_second <= 0.0
    }
}

/// The default rate and overrides by host suffix (`youtube.com` covers `www.youtube.com`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimits {
    pub default: Rate,
    pub hosts: HashMap<String, Rate>,
}

impl RateLimits {
    /// The rate for `host`: the longest matching suffix override, else the default.
    pub fn rate_for(&self, host: &str) -> Rate {
        let host = host.to_ascii_lowercase();
        self.hosts
            .iter()
            .filter(|(suffix, _)| {
                let suffix = suffix.trim_start_matches('.').to_ascii_lowercase();
                host == suffix || host.ends_with(&format!(".{suffix}"))
            })
            .max_by_key(|(suffix, _)| suffix.len())
            .map(|(_, rate)| *rate)
            .unwrap_or(self.default)
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct HostLimiter {
    limits: RwLock<RateLimits>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl HostLimiter {
    pub fn new(limits: RateLimits) -> Self {
        Self {
            limits: RwLock::new(limits),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn set(&self, limits: RateLimits) {
        *self.limits.write().unwrap_or_else(|e| e.into_inner()) = limits;
    }

    pub fn limits(&self) -> RateLimits {
        self.limits
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// How long a request to `host` must wait before it may go, taking its token.
    pub fn reserve(&self, host: &str, now: Instant) -> Duration {
        let rate = self.limits().rate_for(host);
        if rate.is_unlimited() {
            return Duration::ZERO;
        }
        let burst = f64::from(rate.burst.max(1));
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() > 4096 {
            buckets.retain(|_, b| now.duration_since(b.last) < Duration::from_secs(600));
        }
        let bucket = buckets.entry(host.to_ascii_lowercase()).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate.per_second).min(burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Duration::ZERO
        } else {
            let wait = (1.0 - bucket.tokens) / rate.per_second;
            bucket.tokens -= 1.0;
            Duration::from_secs_f64(wait)
        }
    }

    /// Waits until a request to `host` may go.
    pub async fn acquire(&self, host: &str) {
        let wait = self.reserve(host, Instant::now());
        if !wait.is_zero() {
            tracing::debug!(host, ?wait, "rate limited");
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_overrides_win_over_the_default() {
        let mut limits = RateLimits::default();
        limits.hosts.insert(
            "youtube.com".into(),
            Rate {
                per_second: 1.0,
                burst: 2,
            },
        );
        limits.hosts.insert(
            ".googlevideo.com".into(),
            Rate {
                per_second: 20.0,
                burst: 40,
            },
        );
        assert_eq!(limits.rate_for("www.youtube.com").per_second, 1.0);
        assert_eq!(limits.rate_for("YOUTUBE.COM").per_second, 1.0);
        assert_eq!(limits.rate_for("r1.googlevideo.com").burst, 40);
        assert_eq!(limits.rate_for("notyoutube.com"), Rate::default());
    }

    #[test]
    fn buckets_burst_then_pace() {
        let limiter = HostLimiter::new(RateLimits {
            default: Rate {
                per_second: 2.0,
                burst: 2,
            },
            hosts: HashMap::new(),
        });
        let t0 = Instant::now();
        assert_eq!(limiter.reserve("a.test", t0), Duration::ZERO);
        assert_eq!(limiter.reserve("a.test", t0), Duration::ZERO);
        let third = limiter.reserve("a.test", t0);
        assert!((third.as_secs_f64() - 0.5).abs() < 1e-6, "{third:?}");
        let fourth = limiter.reserve("a.test", t0);
        assert!((fourth.as_secs_f64() - 1.0).abs() < 1e-6, "{fourth:?}");
        // Another host has its own bucket; time refills.
        assert_eq!(limiter.reserve("b.test", t0), Duration::ZERO);
        assert_eq!(
            limiter.reserve("a.test", t0 + Duration::from_secs(10)),
            Duration::ZERO
        );
        limiter.set(RateLimits {
            default: Rate::UNLIMITED,
            hosts: HashMap::new(),
        });
        for _ in 0..100 {
            assert_eq!(limiter.reserve("a.test", t0), Duration::ZERO);
        }
    }
}
