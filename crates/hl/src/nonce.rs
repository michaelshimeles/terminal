//! Nonces: one per signed action, strictly increasing per signer, close to the clock.
//!
//! Hyperliquid keeps the 100 highest nonces per signer and accepts a new one if it is larger
//! than the smallest of those and within (now − 2 days, now + 1 day). Using
//! `max(last + 1, now_ms)` keeps every nonce unique even when two actions are signed in the
//! same millisecond.

use std::sync::atomic::{AtomicU64, Ordering};

/// Milliseconds since the Unix epoch (the browser's clock on the web).
pub fn now_ms() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The next nonce for one signer. Share one per agent key.
#[derive(Debug, Default)]
pub struct NonceManager {
    last: AtomicU64,
}

impl NonceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// `max(last + 1, now)`, atomically.
    pub fn next(&self, now: u64) -> u64 {
        let mut cur = self.last.load(Ordering::Relaxed);
        loop {
            let n = (cur + 1).max(now);
            match self.last.compare_exchange_weak(cur, n, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return n,
                Err(seen) => cur = seen,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_follow_the_clock_and_never_repeat() {
        let n = NonceManager::new();
        assert_eq!(n.next(1_000), 1_000);
        assert_eq!(n.next(1_000), 1_001, "same millisecond");
        assert_eq!(n.next(900), 1_002, "a clock that steps back never reuses one");
        assert_eq!(n.next(5_000), 5_000);
        let shared = std::sync::Arc::new(NonceManager::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = shared.clone();
                std::thread::spawn(move || (0..500).map(|_| s.next(7)).collect::<Vec<_>>())
            })
            .collect();
        let mut all: Vec<u64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 4000, "unique across threads");
        assert!(now_ms() > 1_700_000_000_000);
    }
}
