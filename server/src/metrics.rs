//! Counters and the server-added latency recorder behind `/metrics` and `/stats`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Default)]
pub struct Metrics {
    pub upstream_messages: AtomicU64,
    pub upstream_bytes: AtomicU64,
    pub upstream_reconnects: AtomicU64,
    pub upstream_rejected: AtomicU64,
    pub clients: AtomicI64,
    pub clients_total: AtomicU64,
    pub client_subscriptions: AtomicI64,
    pub frames_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub resyncs: AtomicU64,
    pub slow_consumer_closes: AtomicU64,
    pub rest_requests: AtomicU64,
    pub rest_errors: AtomicU64,
    /// Upstream frame received → the client frame it caused written to the socket, per topic.
    pub latency_quotes: LatencyRecorder,
    pub latency_book: LatencyRecorder,
    pub latency_trades: LatencyRecorder,
    pub latency_bars: LatencyRecorder,
}

/// Which latency histogram a client frame counts towards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicKind {
    Quotes,
    Book,
    Trades,
    Bars,
}

impl TopicKind {
    pub const ALL: [TopicKind; 4] = [TopicKind::Quotes, TopicKind::Book, TopicKind::Trades, TopicKind::Bars];

    pub fn label(self) -> &'static str {
        match self {
            TopicKind::Quotes => "quotes",
            TopicKind::Book => "book",
            TopicKind::Trades => "trades",
            TopicKind::Bars => "bars",
        }
    }
}

impl Metrics {
    pub fn latency_for(&self, kind: TopicKind) -> &LatencyRecorder {
        match kind {
            TopicKind::Quotes => &self.latency_quotes,
            TopicKind::Book => &self.latency_book,
            TopicKind::Trades => &self.latency_trades,
            TopicKind::Bars => &self.latency_bars,
        }
    }

    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// Keeps the most recent samples so quantiles reflect current behaviour.
pub struct LatencyRecorder {
    samples: Mutex<VecDeque<u32>>,
    count: AtomicU64,
    cap: usize,
}

impl Default for LatencyRecorder {
    fn default() -> Self {
        Self::with_capacity(50_000)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Quantiles {
    /// Samples in the window the quantiles come from.
    pub window: usize,
    /// Samples since start.
    pub total: u64,
    pub p50_us: u32,
    pub p90_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
}

impl LatencyRecorder {
    pub fn with_capacity(cap: usize) -> Self {
        Self { samples: Mutex::new(VecDeque::with_capacity(cap.min(4096))), count: AtomicU64::new(0), cap }
    }

    pub fn record(&self, d: Duration) {
        let us = d.as_micros().min(u32::MAX as u128) as u32;
        let mut s = self.samples.lock().expect("latency lock");
        if s.len() == self.cap {
            s.pop_front();
        }
        s.push_back(us);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn quantiles(&self) -> Quantiles {
        let mut v: Vec<u32> = self.samples.lock().expect("latency lock").iter().copied().collect();
        v.sort_unstable();
        let q = |p: f64| if v.is_empty() { 0 } else { v[((v.len() - 1) as f64 * p).round() as usize] };
        Quantiles {
            window: v.len(),
            total: self.count.load(Ordering::Relaxed),
            p50_us: q(0.50),
            p90_us: q(0.90),
            p99_us: q(0.99),
            max_us: v.last().copied().unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_over_recent_samples() {
        let r = LatencyRecorder::with_capacity(100);
        for us in 1..=200u64 {
            r.record(Duration::from_micros(us));
        }
        let q = r.quantiles();
        assert_eq!((q.window, q.total), (100, 200));
        assert_eq!((q.p50_us, q.p99_us, q.max_us), (151, 199, 200));
        assert_eq!(LatencyRecorder::default().quantiles().p99_us, 0);
    }
}
