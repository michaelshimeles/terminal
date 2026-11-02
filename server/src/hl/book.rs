//! One order book from two l2Book feeds. Hyperliquid's default l2Book has 20 levels a side but
//! arrives only about every 5 s; the `fast` one arrives about every 0.5 s with 5 levels a side.
//! A book topic subscribes to both and shows the fast levels on top of the slow book's deeper
//! ones, so the top of the book refreshes at the fast rate and the ladder keeps its depth.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tr_proto::Level;

use super::wire::FAST_BOOK_LEVELS;
use crate::provider::{Upstream, UpstreamEvent, UpstreamSink};

/// Deep levels older than this are dropped rather than shown under a fresh top of book. The slow
/// feed normally refreshes every ~5 s.
pub const SLOW_MAX_AGE: Duration = Duration::from_secs(15);

struct Snap {
    bids: Vec<Level>,
    asks: Vec<Level>,
    time_ms: i64,
    recv: Instant,
}

#[derive(Default)]
struct Part {
    book: Option<Snap>,
    rejected: Option<String>,
}

#[derive(Default)]
struct State {
    fast: Part,
    slow: Part,
    /// The primary feed (fast, or slow once fast is refused) is disconnected: hold the book until
    /// it reconnects, as a single feed would.
    down: bool,
}

/// Fast levels first, then the slow book's levels beyond them. A side the fast feed sends with
/// fewer than [`FAST_BOOK_LEVELS`] levels is the whole side, so nothing is added under it.
pub fn merge(fast: &[Level], slow: &[Level], bids: bool) -> Vec<Level> {
    let Some(last) = fast.last().filter(|_| fast.len() >= FAST_BOOK_LEVELS) else {
        return if fast.is_empty() && slow.is_empty() { Vec::new() } else { fast.to_vec() };
    };
    let cap = fast.len().max(slow.len());
    let deeper = slow.iter().filter(|l| if bids { l.px < last.px } else { l.px > last.px });
    fast.iter().chain(deeper).take(cap).copied().collect()
}

/// Two sinks, one per feed, that send one merged book to `out`.
pub fn sinks(out: UpstreamSink) -> (UpstreamSink, UpstreamSink) {
    let st = Arc::new(Mutex::new(State::default()));
    let sink = |fast: bool| {
        let (st, out) = (st.clone(), out.clone());
        UpstreamSink::new(move |up| on_event(&mut st.lock().expect("book merge lock"), fast, up, &out))
    };
    (sink(true), sink(false))
}

fn on_event(st: &mut State, fast: bool, up: Upstream, out: &UpstreamSink) {
    let primary = fast || st.fast.rejected.is_some();
    let part = if fast { &mut st.fast } else { &mut st.slow };
    match up.event {
        UpstreamEvent::Book { bids, asks, time_ms } => {
            part.book = Some(Snap { bids, asks, time_ms, recv: up.recv });
            if primary {
                st.down = false;
            }
            if !st.down {
                emit(st, up.recv, up.recv_ms, out);
            }
        }
        UpstreamEvent::Disconnected => {
            part.book = None;
            if primary {
                st.down = true;
                out.send(up);
            }
        }
        // The topic drops its book on a reconnect; the next book from either feed refills it.
        UpstreamEvent::Reconnected => {
            part.book = None;
            if primary {
                st.down = false;
                out.send(up);
            }
        }
        UpstreamEvent::Rejected(why) => {
            part.book = None;
            part.rejected = Some(why.clone());
            if st.fast.rejected.is_some() && st.slow.rejected.is_some() {
                out.send(Upstream { event: UpstreamEvent::Rejected(why), ..up });
            } else if fast {
                // The slow feed carries on alone, and is now the one whose connection counts.
                st.down = false;
                tracing::warn!("fast book feed refused ({why}); using the default feed alone");
                emit(st, up.recv, up.recv_ms, out);
            }
        }
        _ => {}
    }
}

fn emit(st: &State, recv: Instant, recv_ms: i64, out: &UpstreamSink) {
    let slow = st.slow.book.as_ref().filter(|s| recv.saturating_duration_since(s.recv) <= SLOW_MAX_AGE);
    let (bids, asks, time_ms) = match (st.fast.book.as_ref(), slow) {
        (None, None) => return,
        (Some(f), None) => (f.bids.clone(), f.asks.clone(), f.time_ms),
        (None, Some(s)) => (s.bids.clone(), s.asks.clone(), s.time_ms),
        (Some(f), Some(s)) => (merge(&f.bids, &s.bids, true), merge(&f.asks, &s.asks, false), f.time_ms.max(s.time_ms)),
    };
    out.send(Upstream { recv, recv_ms, event: UpstreamEvent::Book { bids, asks, time_ms } });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv(px: &[f64]) -> Vec<Level> {
        px.iter().map(|&px| Level { px, sz: 1.0, n: 1 }).collect()
    }

    fn pxs(l: &[Level]) -> Vec<f64> {
        l.iter().map(|l| l.px).collect()
    }

    #[test]
    fn fast_levels_sit_on_top_of_deeper_slow_ones() {
        let slow: Vec<f64> = (0..20).map(|i| 100.0 - i as f64).collect();
        // The top moved down one tick since the slow snapshot.
        let fast = lv(&[99.0, 98.0, 97.0, 96.0, 95.0]);
        let m = merge(&fast, &lv(&slow), true);
        assert_eq!(pxs(&m[..6]), [99.0, 98.0, 97.0, 96.0, 95.0, 94.0]);
        assert_eq!(m.len(), 19, "5 fast bids, then the 14 slow ones below 95");
        assert!(m.windows(2).all(|w| w[0].px > w[1].px));

        let asks: Vec<f64> = (0..20).map(|i| 101.0 + i as f64).collect();
        let fast = lv(&[103.0, 104.0, 105.0, 106.0, 107.0]);
        let m = merge(&fast, &lv(&asks), false);
        assert_eq!(pxs(&m[..6]), [103.0, 104.0, 105.0, 106.0, 107.0, 108.0]);
        assert_eq!(m.len(), 18, "slow asks at or inside the fast range are gone");
        assert!(m.windows(2).all(|w| w[0].px < w[1].px));
    }

    #[test]
    fn a_short_fast_side_is_the_whole_side() {
        assert_eq!(pxs(&merge(&lv(&[99.0, 98.0]), &lv(&[100.0, 99.0, 98.0, 97.0]), true)), [99.0, 98.0]);
        assert!(merge(&[], &lv(&[100.0]), true).is_empty());
    }

    struct Rig {
        got: Arc<Mutex<Vec<UpstreamEvent>>>,
        fast: UpstreamSink,
        slow: UpstreamSink,
        t0: Instant,
    }

    impl Rig {
        fn new() -> Self {
            let got = Arc::new(Mutex::new(Vec::new()));
            let g = got.clone();
            let (fast, slow) = sinks(UpstreamSink::new(move |u| g.lock().unwrap().push(u.event)));
            Self { got, fast, slow, t0: Instant::now() }
        }

        fn send(&self, fast: bool, at_ms: u64, event: UpstreamEvent) {
            let up = Upstream { recv: self.t0 + Duration::from_millis(at_ms), recv_ms: at_ms as i64, event };
            if fast { self.fast.send(up) } else { self.slow.send(up) }
        }

        fn book(&self, fast: bool, at_ms: u64, top: f64) {
            let n = if fast { FAST_BOOK_LEVELS } else { 20 };
            let bids = lv(&(0..n).map(|i| top - i as f64).collect::<Vec<_>>());
            let asks = lv(&(0..n).map(|i| top + 1.0 + i as f64).collect::<Vec<_>>());
            self.send(fast, at_ms, UpstreamEvent::Book { bids, asks, time_ms: at_ms as i64 });
        }

        fn take(&self) -> Vec<UpstreamEvent> {
            std::mem::take(&mut self.got.lock().unwrap())
        }
    }

    fn depth(e: &UpstreamEvent) -> (usize, f64) {
        let UpstreamEvent::Book { bids, .. } = e else { panic!("not a book: {e:?}") };
        (bids.len(), bids[0].px)
    }

    /// The regression: the topic used to refresh only when the ~5 s default feed did.
    #[test]
    fn the_book_refreshes_at_the_fast_rate_and_keeps_its_depth() {
        let r = Rig::new();
        r.book(false, 0, 100.0);
        // 10 s of fast books every 500 ms, slow ones every 5 s.
        for i in 1..=20u64 {
            r.book(true, i * 500, 100.0 + i as f64 * 0.25);
            if i % 10 == 0 {
                r.book(false, i * 500, 100.0);
            }
        }
        let got: Vec<_> = r.take().iter().map(depth).collect();
        assert_eq!(got.len(), 1 + 20 + 2, "one book per upstream update");
        let mut tops: Vec<f64> = got.iter().map(|&(_, top)| top).collect();
        tops.dedup();
        assert_eq!(tops.len(), 21, "a new top of book every 500 ms: {tops:?}");
        assert!(got.iter().all(|&(n, _)| n == 20), "deep levels stay under the fast top: {got:?}");
    }

    #[test]
    fn stale_deep_levels_are_dropped() {
        let r = Rig::new();
        r.book(false, 0, 100.0);
        r.book(true, 1_000, 100.0);
        r.book(true, SLOW_MAX_AGE.as_millis() as u64 + 1_000, 100.0);
        let got = r.take();
        assert_eq!(got.iter().map(|e| depth(e).0).collect::<Vec<_>>(), [20, 20, FAST_BOOK_LEVELS]);
    }

    #[test]
    fn the_fast_feed_owns_the_connection_state() {
        let r = Rig::new();
        r.book(true, 0, 100.0);
        r.send(false, 10, UpstreamEvent::Disconnected);
        r.send(false, 20, UpstreamEvent::Reconnected);
        assert_eq!(r.take().len(), 1, "the slow feed's reconnect doesn't reset the topic");

        r.send(true, 30, UpstreamEvent::Disconnected);
        r.book(false, 40, 100.0);
        assert!(matches!(r.take()[..], [UpstreamEvent::Disconnected]), "no book while the fast feed is down");
        r.send(true, 50, UpstreamEvent::Reconnected);
        r.book(true, 60, 100.0);
        let got = r.take();
        assert!(matches!(got[0], UpstreamEvent::Reconnected));
        assert_eq!(depth(&got[1]).0, 20, "the slow book that came in meanwhile is used");
    }

    #[test]
    fn a_refused_fast_feed_falls_back_to_the_default_one() {
        let r = Rig::new();
        r.book(false, 0, 100.0);
        r.send(true, 10, UpstreamEvent::Rejected("no".into()));
        r.send(false, 20, UpstreamEvent::Disconnected);
        r.send(false, 30, UpstreamEvent::Rejected("no".into()));
        let got = r.take();
        assert_eq!(depth(&got[0]).0, 20);
        assert_eq!(depth(&got[1]).0, 20, "the slow book alone after the fast feed is refused");
        assert!(matches!(got[2], UpstreamEvent::Disconnected), "the slow feed's state counts now");
        assert!(matches!(&got[3], UpstreamEvent::Rejected(w) if w == "no"), "rejected once both are");
        assert_eq!(got.len(), 4);
    }
}
