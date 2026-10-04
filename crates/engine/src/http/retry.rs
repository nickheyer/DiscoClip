//! Retrying requests that failed for reasons that pass: connection trouble, timeouts,
//! rate limiting and server errors, with exponential backoff and `Retry-After`.

use std::time::Duration;

use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};

use super::cookies::parse_http_date;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub attempts: u32,
    pub base_ms: u64,
    pub max_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_ms: 500,
            max_ms: 10_000,
        }
    }
}

impl RetryPolicy {
    pub const NONE: RetryPolicy = RetryPolicy {
        attempts: 1,
        base_ms: 0,
        max_ms: 0,
    };

    /// Whether a response with `status` is worth asking for again.
    pub fn retries_status(status: StatusCode) -> bool {
        matches!(
            status.as_u16(),
            408 | 425 | 429 | 500 | 502 | 503 | 504 | 522 | 524
        )
    }

    /// The pause before attempt `attempt` (1 = the first retry): `Retry-After` when the
    /// server named one, else exponential from `base_ms`, capped at `max_ms`.
    pub fn delay(&self, attempt: u32, headers: Option<&HeaderMap>) -> Duration {
        let cap = Duration::from_millis(self.max_ms.max(self.base_ms));
        if let Some(after) = headers.and_then(retry_after) {
            return after.min(cap.max(Duration::from_secs(30)));
        }
        let factor = 2u64.saturating_pow(attempt.saturating_sub(1).min(20));
        Duration::from_millis(self.base_ms.saturating_mul(factor)).min(cap)
    }
}

/// The `Retry-After` header, as seconds or an HTTP date.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(value)?;
    let wait = at.duration_since(jiff::Timestamp::now());
    Some(Duration::try_from(wait).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_grow_and_honour_retry_after() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay(1, None), Duration::from_millis(500));
        assert_eq!(policy.delay(2, None), Duration::from_millis(1000));
        assert_eq!(policy.delay(3, None), Duration::from_millis(2000));
        assert_eq!(policy.delay(30, None), Duration::from_millis(10_000));
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(policy.delay(1, Some(&headers)), Duration::from_secs(7));
        headers.insert(header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(policy.delay(1, Some(&headers)), Duration::from_secs(30));
        headers.insert(
            header::RETRY_AFTER,
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        assert_eq!(policy.delay(1, Some(&headers)), Duration::ZERO);
        assert!(RetryPolicy::retries_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(RetryPolicy::retries_status(StatusCode::BAD_GATEWAY));
        assert!(!RetryPolicy::retries_status(StatusCode::NOT_FOUND));
        assert!(!RetryPolicy::retries_status(StatusCode::FORBIDDEN));
    }
}
