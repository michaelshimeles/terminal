use serde::{Deserialize, Serialize};

use crate::Timeframe;

/// One OHLCV bar. `ts` is the bar open time in Unix seconds (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub ts: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl Bar {
    pub fn new(ts: i64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> Self {
        Self { ts, open, high, low, close, volume }
    }

    /// Typical price (high + low + close) / 3, used by VWAP.
    pub fn hlc3(&self) -> f64 {
        (self.high + self.low + self.close) / 3.0
    }

    pub fn is_up(&self) -> bool {
        self.close >= self.open
    }

    /// Merge a live update for the same bar: keep the original open, widen the range,
    /// take the latest close and the larger volume (partial streams undercount).
    pub fn absorb(&mut self, live: &Bar) {
        debug_assert_eq!(self.ts, live.ts);
        self.high = self.high.max(live.high);
        self.low = self.low.min(live.low);
        self.close = live.close;
        self.volume = self.volume.max(live.volume);
    }
}

/// A symbol's bars at one timeframe, sorted by `ts` ascending with no duplicates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Series {
    pub symbol: String,
    pub timeframe: Timeframe,
    pub bars: Vec<Bar>,
}

impl Series {
    pub fn new(symbol: impl Into<String>, timeframe: Timeframe, bars: Vec<Bar>) -> Self {
        let mut s = Self { symbol: symbol.into(), timeframe, bars };
        s.normalize();
        s
    }

    /// Sort by time and drop duplicate timestamps (last write wins).
    pub fn normalize(&mut self) {
        self.bars.sort_by_key(|b| b.ts);
        let mut out: Vec<Bar> = Vec::with_capacity(self.bars.len());
        for b in self.bars.drain(..) {
            match out.last_mut() {
                Some(last) if last.ts == b.ts => *last = b,
                _ => out.push(b),
            }
        }
        self.bars = out;
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    pub fn closes(&self) -> Vec<f64> {
        self.bars.iter().map(|b| b.close).collect()
    }

    /// Index of the first bar with `ts >= t` (may equal `len()`).
    pub fn lower_bound(&self, t: i64) -> usize {
        self.bars.partition_point(|b| b.ts < t)
    }

    /// Index of the bar containing time `t`, i.e. the last bar with `ts <= t`.
    pub fn index_at_or_before(&self, t: i64) -> Option<usize> {
        let i = self.bars.partition_point(|b| b.ts <= t);
        i.checked_sub(1)
    }

    /// Apply one live bar by its `ts`: absorb it into the bar of the same period, append it if
    /// newer, or insert it where a period is missing (a feed backfilling a gap after an outage).
    /// Bars older than the whole series are ignored; paging loads those. Returns true if the
    /// series changed.
    pub fn apply_live(bars: &mut Vec<Bar>, live: Bar) -> bool {
        if bars.last().is_none_or(|last| last.ts < live.ts) {
            bars.push(live);
            return true;
        }
        match bars.binary_search_by_key(&live.ts, |b| b.ts) {
            Ok(i) => {
                let before = bars[i];
                bars[i].absorb(&live);
                bars[i] != before
            }
            Err(0) => false,
            Err(i) => {
                bars.insert(i, live);
                true
            }
        }
    }

    /// Merge newer bars (e.g. a streaming update) into the series.
    pub fn merge(&mut self, bars: impl IntoIterator<Item = Bar>) {
        let mut tail_only = true;
        for b in bars {
            match self.bars.last_mut() {
                Some(last) if last.ts == b.ts => *last = b,
                Some(last) if last.ts > b.ts => {
                    tail_only = false;
                    self.bars.push(b);
                }
                _ => self.bars.push(b),
            }
        }
        if !tail_only {
            self.normalize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(ts: i64, c: f64) -> Bar {
        Bar::new(ts, c, c, c, c, 1.0)
    }

    #[test]
    fn normalize_sorts_and_dedups() {
        let s = Series::new("X", Timeframe::D1, vec![b(3, 3.0), b(1, 1.0), b(3, 30.0), b(2, 2.0)]);
        let ts: Vec<_> = s.bars.iter().map(|b| (b.ts, b.close)).collect();
        assert_eq!(ts, vec![(1, 1.0), (2, 2.0), (3, 30.0)]);
    }

    #[test]
    fn live_updates() {
        let mut v = vec![Bar::new(60, 10.0, 11.0, 9.0, 10.5, 100.0)];
        assert!(Series::apply_live(&mut v, Bar::new(60, 10.4, 12.0, 10.0, 11.5, 40.0)));
        assert_eq!(v[0], Bar::new(60, 10.0, 12.0, 9.0, 11.5, 100.0));
        assert!(!Series::apply_live(&mut v, Bar::new(0, 1.0, 1.0, 1.0, 1.0, 1.0)));
        assert!(Series::apply_live(&mut v, Bar::new(120, 11.5, 11.6, 11.4, 11.6, 5.0)));
        assert_eq!(v.len(), 2);
        // A feed backfilling after an outage: a missing period is inserted, an older bar it
        // still holds is completed, and nothing older than the series is added.
        assert!(Series::apply_live(&mut v, Bar::new(240, 12.0, 12.0, 12.0, 12.0, 1.0)));
        assert!(Series::apply_live(&mut v, Bar::new(180, 11.6, 11.9, 11.5, 11.8, 7.0)));
        assert!(Series::apply_live(&mut v, Bar::new(120, 11.5, 11.7, 11.4, 11.65, 6.0)));
        assert_eq!(v.iter().map(|b| b.ts).collect::<Vec<_>>(), [60, 120, 180, 240]);
        assert_eq!((v[1].high, v[1].close, v[1].volume), (11.7, 11.65, 6.0));
        assert!(!Series::apply_live(&mut v, Bar::new(180, 11.6, 11.9, 11.5, 11.8, 7.0)), "no change");
    }

    #[test]
    fn lookup_and_merge() {
        let mut s = Series::new("X", Timeframe::D1, vec![b(10, 1.0), b(20, 2.0), b(30, 3.0)]);
        assert_eq!(s.index_at_or_before(25), Some(1));
        assert_eq!(s.index_at_or_before(5), None);
        assert_eq!(s.lower_bound(20), 1);
        s.merge([b(30, 3.5), b(40, 4.0)]);
        assert_eq!(s.len(), 4);
        assert_eq!(s.bars[2].close, 3.5);
        s.merge([b(15, 1.5)]);
        assert_eq!(s.bars[1].ts, 15);
    }
}
