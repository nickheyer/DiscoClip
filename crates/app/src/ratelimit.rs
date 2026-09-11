//! Strike counting per key inside a sliding window, for slowing down guesses.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    max: u32,
    window: Duration,
    strikes: Mutex<HashMap<String, VecDeque<Instant>>>,
}

const SWEEP_ABOVE: usize = 4096;

impl RateLimiter {
    /// Allows `max` strikes per key within any `window`.
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            strikes: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `key` may proceed; when it may not, how long until it may.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let strikes = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        let Some(log) = strikes.get(key) else {
            return Ok(());
        };
        let start = now.checked_sub(self.window).unwrap_or(now);
        let recent: Vec<&Instant> = log.iter().filter(|at| **at > start).collect();
        if recent.len() < self.max as usize {
            return Ok(());
        }
        let oldest = **recent.first().expect("at least max strikes");
        Err(oldest + self.window - now)
    }

    /// Counts one strike against `key`.
    pub fn strike(&self, key: &str) {
        self.strike_at(key, Instant::now());
    }

    pub fn strike_at(&self, key: &str, now: Instant) {
        let mut strikes = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        let start = now.checked_sub(self.window).unwrap_or(now);
        if strikes.len() > SWEEP_ABOVE {
            strikes.retain(|_, log| log.iter().any(|at| *at > start));
        }
        let log = strikes.entry(key.to_string()).or_default();
        while log.front().is_some_and(|at| *at <= start) {
            log.pop_front();
        }
        log.push_back(now);
    }

    /// Forgets `key`'s strikes.
    pub fn clear(&self, key: &str) {
        self.strikes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max_strikes_until_the_window_passes() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(limiter.check_at("a", t0).is_ok());
        limiter.strike_at("a", t0);
        limiter.strike_at("a", t0 + Duration::from_secs(10));
        assert!(limiter.check_at("a", t0 + Duration::from_secs(11)).is_ok());
        limiter.strike_at("a", t0 + Duration::from_secs(20));
        assert_eq!(
            limiter.check_at("a", t0 + Duration::from_secs(30)),
            Err(Duration::from_secs(30))
        );
        assert!(limiter.check_at("b", t0 + Duration::from_secs(30)).is_ok());
        assert_eq!(
            limiter.check_at("a", t0 + Duration::from_secs(59)),
            Err(Duration::from_secs(1))
        );
        assert!(limiter.check_at("a", t0 + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn clearing_forgets_strikes() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        limiter.strike_at("a", t0);
        assert!(limiter.check_at("a", t0).is_err());
        limiter.clear("a");
        assert!(limiter.check_at("a", t0).is_ok());
    }

    #[test]
    fn stale_keys_are_swept() {
        let limiter = RateLimiter::new(1, Duration::from_secs(1));
        let t0 = Instant::now();
        for i in 0..=SWEEP_ABOVE {
            limiter.strike_at(&i.to_string(), t0);
        }
        assert_eq!(limiter.strikes.lock().unwrap().len(), SWEEP_ABOVE + 1);
        limiter.strike_at("late", t0 + Duration::from_secs(5));
        assert_eq!(limiter.strikes.lock().unwrap().len(), 1);
    }
}
