//! A sliding-window limiter: at most `budget` units in any `window`. The server uses one for
//! each Hyperliquid per-IP limit it has to respect (REST weight, websocket messages sent, new
//! websocket connections), always with a budget below Hyperliquid's own.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct WindowLimiter {
    window: Duration,
    budget: u32,
    log: Mutex<VecDeque<(Instant, u32)>>,
}

impl WindowLimiter {
    pub fn new(budget: u32, window: Duration) -> Self {
        Self { window, budget: budget.max(1), log: Mutex::new(VecDeque::new()) }
    }

    pub fn per_minute(budget: u32) -> Self {
        Self::new(budget, Duration::from_secs(60))
    }

    pub fn budget(&self) -> u32 {
        self.budget
    }

    /// Units spent in the current window.
    pub fn used(&self) -> u32 {
        let mut log = self.log.lock().expect("limiter lock");
        Self::expire(&mut log, Instant::now(), self.window);
        log.iter().map(|(_, n)| n).sum()
    }

    fn expire(log: &mut VecDeque<(Instant, u32)>, now: Instant, window: Duration) {
        while log.front().is_some_and(|(t, _)| now.duration_since(*t) >= window) {
            log.pop_front();
        }
    }

    /// Spend `n` now if the window has room, or say how long until it will. A request larger
    /// than the whole budget waits for an empty window, then goes through alone.
    pub fn try_acquire(&self, n: u32) -> Result<(), Duration> {
        let now = Instant::now();
        let mut log = self.log.lock().expect("limiter lock");
        Self::expire(&mut log, now, self.window);
        let mut used: u32 = log.iter().map(|(_, k)| k).sum();
        if used + n <= self.budget || log.is_empty() {
            log.push_back((now, n));
            return Ok(());
        }
        // Wait until enough of the oldest entries expire.
        for (t, k) in log.iter() {
            used -= k;
            if used + n <= self.budget {
                return Err((*t + self.window).saturating_duration_since(now) + Duration::from_millis(1));
            }
        }
        Err(self.window)
    }

    /// Wait for room, then spend `n`. Dropping the future before it completes spends nothing.
    pub async fn acquire(&self, n: u32) {
        while let Err(wait) = self.try_acquire(n) {
            tokio::time::sleep(wait).await;
        }
    }

    /// Record units already spent (a response that cost more than estimated, or a 429 that
    /// should keep everyone off the endpoint for a window).
    pub fn charge(&self, n: u32) {
        self.log.lock().expect("limiter lock").push_back((Instant::now(), n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_holds_within_a_window() {
        let l = WindowLimiter::new(100, Duration::from_millis(200));
        assert!(l.try_acquire(60).is_ok());
        assert!(l.try_acquire(40).is_ok());
        let wait = l.try_acquire(1).unwrap_err();
        assert!(wait > Duration::from_millis(150) && wait <= Duration::from_millis(201), "{wait:?}");
        assert_eq!(l.used(), 100);
        std::thread::sleep(Duration::from_millis(210));
        assert_eq!(l.used(), 0);
        assert!(l.try_acquire(100).is_ok());
    }

    #[test]
    fn oversized_requests_go_alone() {
        let l = WindowLimiter::new(10, Duration::from_millis(100));
        assert!(l.try_acquire(25).is_ok(), "an empty window admits one oversized request");
        assert!(l.try_acquire(1).is_err());
        l.charge(5);
        assert_eq!(l.used(), 30);
    }

    #[tokio::test]
    async fn acquire_waits_for_room() {
        let l = WindowLimiter::new(2, Duration::from_millis(150));
        let t0 = Instant::now();
        l.acquire(1).await;
        l.acquire(1).await;
        assert!(t0.elapsed() < Duration::from_millis(50));
        l.acquire(1).await;
        assert!(t0.elapsed() >= Duration::from_millis(150), "the third waited for the window to roll");
    }
}
