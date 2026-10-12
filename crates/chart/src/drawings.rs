//! User drawings. Anchors are stored as (timestamp, price) so they survive pan, zoom and
//! timeframe changes; they are mapped to bar indices only when drawn.

use serde::{Deserialize, Serialize};

pub const FIB_LEVELS: [f64; 7] = [0.0, 0.236, 0.382, 0.5, 0.618, 0.786, 1.0];
/// Trend-based extension levels, projected from the third point by the a→b swing.
pub const FIB_EXT_LEVELS: [f64; 8] = [0.0, 0.382, 0.618, 1.0, 1.272, 1.618, 2.0, 2.618];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub ts: i64,
    pub price: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tool {
    Trendline,
    Horizontal,
    Vertical,
    Fib,
    /// Starts at the first point and runs through the second to the plot edge.
    Ray,
    /// Runs through both points to both plot edges.
    ExtendedLine,
    /// Starts at the point and runs right.
    HorizontalRay,
    /// Horizontal and vertical line through one point.
    CrossLine,
    /// Two points set the base line; the third sets the parallel line's offset.
    ParallelChannel,
    /// Trend-based Fibonacci extension: swing a→b projected from c.
    FibExtension,
    Rectangle,
    /// Entry, target and stop; target above entry for a long.
    LongPosition,
    ShortPosition,
    /// Price and time span between two points.
    Measure,
    /// Zoom the time axis to the span between two points; never stored as a drawing.
    Zoom,
}

impl Tool {
    pub fn points(self) -> usize {
        match self {
            Tool::Horizontal | Tool::Vertical | Tool::HorizontalRay | Tool::CrossLine => 1,
            Tool::ParallelChannel | Tool::FibExtension | Tool::LongPosition | Tool::ShortPosition => 3,
            _ => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Trendline => "Trendline",
            Tool::Horizontal => "Horizontal line",
            Tool::Vertical => "Vertical line",
            Tool::Fib => "Fib retracement",
            Tool::Ray => "Ray",
            Tool::ExtendedLine => "Extended line",
            Tool::HorizontalRay => "Horizontal ray",
            Tool::CrossLine => "Cross line",
            Tool::ParallelChannel => "Parallel channel",
            Tool::FibExtension => "Trend-based fib extension",
            Tool::Rectangle => "Rectangle",
            Tool::LongPosition => "Long position",
            Tool::ShortPosition => "Short position",
            Tool::Measure => "Measure",
            Tool::Zoom => "Zoom in",
        }
    }

    /// What the next click places, by the number of points placed so far.
    pub fn step(self, placed: usize) -> &'static str {
        let steps: &[&str] = match self {
            Tool::ParallelChannel => &["click start", "click end", "click to set the channel width"],
            Tool::FibExtension => &["click swing start", "click swing end", "click the retracement point"],
            Tool::LongPosition | Tool::ShortPosition => &["click entry", "click target", "click stop"],
            Tool::Zoom => &["click first corner", "click second corner"],
            _ if self.points() == 1 => &["click to place"],
            _ => &["click first point", "click second point"],
        };
        steps[placed.min(steps.len() - 1)]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drawing {
    pub tool: Tool,
    pub a: Anchor,
    pub b: Anchor,
    /// Third point of three-point tools (channel width, extension origin, stop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<Anchor>,
}

impl Drawing {
    pub fn new(tool: Tool, a: Anchor, b: Anchor) -> Self {
        Self { tool, a, b, c: None }
    }

    /// Build from placed points: one-point tools repeat the first point, and a missing
    /// third point stays `None` (a preview while the tool is being placed).
    pub fn from_points(tool: Tool, pts: &[Anchor]) -> Option<Self> {
        let a = *pts.first()?;
        let b = pts.get(1).copied().unwrap_or(a);
        Some(Self { tool, a, b, c: pts.get(2).copied() })
    }

    /// Fib levels as `(ratio, price)`, measured from `b` (0) back to `a` (1), the
    /// usual retracement convention for a swing drawn from `a` to `b`.
    pub fn fib_levels(&self) -> Vec<(f64, f64)> {
        FIB_LEVELS.iter().map(|&r| (r, self.b.price + (self.a.price - self.b.price) * r)).collect()
    }

    /// Extension levels as `(ratio, price)`: the a→b move projected from `c` (or `b`
    /// while `c` is not placed yet).
    pub fn fib_extension_levels(&self) -> Vec<(f64, f64)> {
        let base = self.c.unwrap_or(self.b).price;
        let swing = self.b.price - self.a.price;
        FIB_EXT_LEVELS.iter().map(|&r| (r, base + swing * r)).collect()
    }

    /// Long/short position stats, with `a` the entry, `b` the target and `c` the stop.
    pub fn position(&self) -> Option<PositionStats> {
        let long = match self.tool {
            Tool::LongPosition => true,
            Tool::ShortPosition => false,
            _ => return None,
        };
        Some(PositionStats::new(self.a.price, self.b.price, self.c.map(|c| c.price), long))
    }
}

/// Signed returns of a position's target and stop, and its reward-to-risk ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionStats {
    pub target_pct: f64,
    pub stop_pct: Option<f64>,
    pub reward_risk: Option<f64>,
}

impl PositionStats {
    pub fn new(entry: f64, target: f64, stop: Option<f64>, long: bool) -> Self {
        let side = if long { 1.0 } else { -1.0 };
        let pct = |p: f64| if entry != 0.0 { (p - entry) / entry * 100.0 * side } else { 0.0 };
        let reward_risk = stop.filter(|s| *s != entry).map(|s| (target - entry).abs() / (entry - s).abs());
        Self { target_pct: pct(target), stop_pct: stop.map(pct), reward_risk }
    }
}

/// Change and percentage change from `from` to `to`.
pub fn price_change(from: f64, to: f64) -> (f64, f64) {
    let d = to - from;
    (d, if from != 0.0 { d / from * 100.0 } else { 0.0 })
}

/// Compact duration such as `3d 4h`, `2h 15m` or `45m`.
pub fn format_span(secs: i64) -> String {
    let s = secs.unsigned_abs();
    let (d, h, m) = (s / 86_400, s % 86_400 / 3_600, s % 3_600 / 60);
    match (d, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Undo/redo over a chart's drawings, by snapshots. Changes are picked up by
/// [`History::track`] wherever they came from (toolbar, hotkeys, commands, menus).
#[derive(Debug, Clone, Default)]
pub struct History {
    undo: Vec<Vec<Drawing>>,
    redo: Vec<Vec<Drawing>>,
    /// The drawings as last seen; `None` until the first track.
    last: Option<Vec<Drawing>>,
}

impl History {
    /// Undo steps kept per chart.
    pub const LIMIT: usize = 100;

    /// Record `current` as a new step if it differs from the last state seen. A new step
    /// clears the redo stack.
    pub fn track(&mut self, current: &[Drawing]) {
        match &mut self.last {
            None => self.last = Some(current.to_vec()),
            Some(last) if last.as_slice() != current => {
                self.undo.push(std::mem::replace(last, current.to_vec()));
                if self.undo.len() > Self::LIMIT {
                    self.undo.remove(0);
                }
                self.redo.clear();
            }
            Some(_) => {}
        }
    }

    /// Restore the previous state into `current`. Returns whether there was one.
    pub fn undo(&mut self, current: &mut Vec<Drawing>) -> bool {
        self.track(current);
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(std::mem::replace(current, prev));
        self.last = Some(current.clone());
        true
    }

    /// Re-apply the last undone state into `current`. Returns whether there was one.
    pub fn redo(&mut self, current: &mut Vec<Drawing>) -> bool {
        self.track(current);
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(std::mem::replace(current, next));
        self.last = Some(current.clone());
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

/// Screen-space geometry of a drawing, produced by the chart for hit testing.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Segment([f64; 2], [f64; 2]),
    HLine(f64),
    VLine(f64),
    /// Axis-aligned box between two corners (filled, so the inside counts as a hit).
    Rect([f64; 2], [f64; 2]),
}

/// Distance in pixels from point `p` to a shape.
pub fn distance(shape: &Shape, p: [f64; 2]) -> f64 {
    match shape {
        Shape::HLine(y) => (p[1] - y).abs(),
        Shape::VLine(x) => (p[0] - x).abs(),
        Shape::Segment(a, b) => {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len2 = dx * dx + dy * dy;
            let t = if len2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0) };
            let (cx, cy) = (a[0] + t * dx, a[1] + t * dy);
            ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt()
        }
        Shape::Rect(a, b) => {
            let dx = (a[0].min(b[0]) - p[0]).max(p[0] - a[0].max(b[0])).max(0.0);
            let dy = (a[1].min(b[1]) - p[1]).max(p[1] - a[1].max(b[1])).max(0.0);
            (dx * dx + dy * dy).sqrt()
        }
    }
}

/// The line through `a` and `b` clipped to the box `min..max`: from `a` onwards for a
/// ray, or both ways for an extended line. `None` when `a == b` or the line misses the box.
pub fn extend_line(a: [f64; 2], b: [f64; 2], min: [f64; 2], max: [f64; 2], both: bool) -> Option<([f64; 2], [f64; 2])> {
    let d = [b[0] - a[0], b[1] - a[1]];
    if d[0] == 0.0 && d[1] == 0.0 {
        return None;
    }
    // Liang–Barsky over the parameter t of a + t·d.
    let (mut t0, mut t1) = (if both { f64::NEG_INFINITY } else { 0.0 }, f64::INFINITY);
    for k in 0..2 {
        if d[k] == 0.0 {
            if a[k] < min[k] || a[k] > max[k] {
                return None;
            }
            continue;
        }
        let (ta, tb) = ((min[k] - a[k]) / d[k], (max[k] - a[k]) / d[k]);
        t0 = t0.max(ta.min(tb));
        t1 = t1.min(ta.max(tb));
    }
    (t0 <= t1).then(|| ([a[0] + t0 * d[0], a[1] + t0 * d[1]], [a[0] + t1 * d[0], a[1] + t1 * d[1]]))
}

/// Vertical offset that moves the line through `a` and `b` onto `c`.
pub fn parallel_offset(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    if a[0] == b[0] {
        return c[1] - a[1];
    }
    let y_on_line = a[1] + (c[0] - a[0]) * (b[1] - a[1]) / (b[0] - a[0]);
    c[1] - y_on_line
}

/// Snap `price` to the nearest of a bar's O/H/L/C (magnet mode).
pub fn magnet(price: f64, ohlc: [f64; 4]) -> f64 {
    ohlc.into_iter().min_by(|a, b| (a - price).abs().total_cmp(&(b - price).abs())).unwrap_or(price)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ts: i64, price: f64) -> Anchor {
        Anchor { ts, price }
    }

    #[test]
    fn fib_levels_retrace_from_b() {
        let d = Drawing::new(Tool::Fib, at(0, 100.0), at(1, 200.0));
        let lv = d.fib_levels();
        assert_eq!(lv[0], (0.0, 200.0));
        assert_eq!(lv[3], (0.5, 150.0));
        assert!((lv[4].1 - 138.2).abs() < 1e-9);
        assert_eq!(lv[6], (1.0, 100.0));
    }

    #[test]
    fn fib_extension_projects_the_swing_from_c() {
        let d = Drawing::from_points(Tool::FibExtension, &[at(0, 100.0), at(1, 200.0), at(2, 150.0)]).unwrap();
        let lv = d.fib_extension_levels();
        assert_eq!(lv[0], (0.0, 150.0));
        assert_eq!(lv[3], (1.0, 250.0));
        assert!((lv[5].1 - 311.8).abs() < 1e-9);
        // Before the third point is placed the projection starts at b.
        let preview = Drawing::from_points(Tool::FibExtension, &[at(0, 100.0), at(1, 200.0)]).unwrap();
        assert_eq!(preview.fib_extension_levels()[0], (0.0, 200.0));
    }

    #[test]
    fn points_and_steps() {
        assert_eq!(Tool::HorizontalRay.points(), 1);
        assert_eq!(Tool::CrossLine.points(), 1);
        assert_eq!(Tool::Ray.points(), 2);
        assert_eq!(Tool::Measure.points(), 2);
        assert_eq!(Tool::ParallelChannel.points(), 3);
        assert_eq!(Tool::LongPosition.points(), 3);
        assert_eq!(Tool::LongPosition.step(2), "click stop");
        assert_eq!(Tool::Trendline.step(1), "click second point");
        assert_eq!(Tool::Horizontal.step(0), "click to place");
        assert_eq!(Tool::Horizontal.step(5), "click to place");
    }

    #[test]
    fn from_points_fills_missing_anchors() {
        assert_eq!(Drawing::from_points(Tool::Ray, &[]), None);
        let one = Drawing::from_points(Tool::Horizontal, &[at(5, 10.0)]).unwrap();
        assert_eq!((one.a, one.b, one.c), (at(5, 10.0), at(5, 10.0), None));
    }

    #[test]
    fn drawings_without_c_still_parse() {
        let old = r#"{"tool":"Trendline","a":{"ts":1,"price":2.0},"b":{"ts":3,"price":4.0}}"#;
        let d: Drawing = serde_json::from_str(old).unwrap();
        assert_eq!(d, Drawing::new(Tool::Trendline, at(1, 2.0), at(3, 4.0)));
        // Two-point drawings serialize without a `c` key, as before.
        assert!(!serde_json::to_string(&d).unwrap().contains("\"c\""));
        let ch = Drawing::from_points(Tool::ParallelChannel, &[at(0, 1.0), at(1, 2.0), at(2, 3.0)]).unwrap();
        assert_eq!(serde_json::from_str::<Drawing>(&serde_json::to_string(&ch).unwrap()).unwrap(), ch);
    }

    #[test]
    fn position_stats() {
        let long = Drawing::from_points(Tool::LongPosition, &[at(0, 100.0), at(1, 110.0), at(2, 95.0)]).unwrap();
        let s = long.position().unwrap();
        assert!((s.target_pct - 10.0).abs() < 1e-9);
        assert!((s.stop_pct.unwrap() + 5.0).abs() < 1e-9);
        assert!((s.reward_risk.unwrap() - 2.0).abs() < 1e-9);
        let short = Drawing::from_points(Tool::ShortPosition, &[at(0, 100.0), at(1, 90.0), at(2, 104.0)]).unwrap();
        let s = short.position().unwrap();
        assert!((s.target_pct - 10.0).abs() < 1e-9);
        assert!((s.stop_pct.unwrap() + 4.0).abs() < 1e-9);
        assert!((s.reward_risk.unwrap() - 2.5).abs() < 1e-9);
        // No stop yet (preview), and non-position tools have no stats.
        let preview = Drawing::from_points(Tool::LongPosition, &[at(0, 100.0), at(1, 110.0)]).unwrap();
        assert_eq!(preview.position().unwrap().reward_risk, None);
        assert_eq!(Drawing::new(Tool::Ray, at(0, 1.0), at(1, 2.0)).position(), None);
    }

    #[test]
    fn measure_helpers() {
        assert_eq!(price_change(100.0, 125.0), (25.0, 25.0));
        assert_eq!(price_change(0.0, 5.0), (5.0, 0.0));
        assert_eq!(format_span(45 * 60), "45m");
        assert_eq!(format_span(2 * 3600), "2h");
        assert_eq!(format_span(2 * 3600 + 15 * 60), "2h 15m");
        assert_eq!(format_span(-(3 * 86_400 + 4 * 3600)), "3d 4h");
        assert_eq!(format_span(7 * 86_400 + 59), "7d");
    }

    #[test]
    fn hit_distances() {
        assert_eq!(distance(&Shape::Segment([0.0, 0.0], [10.0, 0.0]), [5.0, 3.0]), 3.0);
        assert_eq!(distance(&Shape::Segment([0.0, 0.0], [10.0, 0.0]), [13.0, 4.0]), 5.0);
        assert_eq!(distance(&Shape::HLine(50.0), [999.0, 47.0]), 3.0);
        assert_eq!(distance(&Shape::VLine(10.0), [12.0, 0.0]), 2.0);
        let r = Shape::Rect([10.0, 30.0], [0.0, 10.0]);
        assert_eq!(distance(&r, [5.0, 20.0]), 0.0);
        assert_eq!(distance(&r, [13.0, 34.0]), 5.0);
        assert_eq!(distance(&r, [5.0, 7.0]), 3.0);
    }

    #[test]
    fn rays_and_extended_lines_clip_to_the_plot() {
        let (min, max) = ([0.0, 0.0], [100.0, 50.0]);
        // A ray from (10,10) through (20,20) leaves the box at the bottom edge.
        assert_eq!(extend_line([10.0, 10.0], [20.0, 20.0], min, max, false), Some(([10.0, 10.0], [50.0, 50.0])));
        // Extended both ways it also reaches the top edge.
        assert_eq!(extend_line([10.0, 10.0], [20.0, 20.0], min, max, true), Some(([0.0, 0.0], [50.0, 50.0])));
        // Horizontal and vertical lines.
        assert_eq!(extend_line([10.0, 5.0], [20.0, 5.0], min, max, true), Some(([0.0, 5.0], [100.0, 5.0])));
        assert_eq!(extend_line([10.0, 5.0], [10.0, 9.0], min, max, false), Some(([10.0, 5.0], [10.0, 50.0])));
        // Degenerate and outside.
        assert_eq!(extend_line([1.0, 1.0], [1.0, 1.0], min, max, true), None);
        assert_eq!(extend_line([10.0, 60.0], [20.0, 60.0], min, max, true), None);
    }

    #[test]
    fn channel_offset() {
        assert_eq!(parallel_offset([0.0, 0.0], [10.0, 10.0], [5.0, 15.0]), 10.0);
        assert_eq!(parallel_offset([0.0, 0.0], [10.0, 0.0], [3.0, -4.0]), -4.0);
        assert_eq!(parallel_offset([2.0, 0.0], [2.0, 10.0], [7.0, 3.0]), 3.0);
    }

    #[test]
    fn history_undoes_and_redoes_tracked_changes() {
        let line = |p: f64| Drawing::new(Tool::Horizontal, at(0, p), at(0, p));
        let mut h = History::default();
        let mut d = vec![line(1.0)];
        h.track(&d); // the starting state is not an undo step
        assert!(!h.can_undo());
        d.push(line(2.0));
        h.track(&d);
        d.push(line(3.0));
        h.track(&d);
        h.track(&d); // unchanged: no new step
        assert!(h.undo(&mut d));
        assert_eq!(d, vec![line(1.0), line(2.0)]);
        assert!(h.undo(&mut d));
        assert_eq!(d, vec![line(1.0)]);
        assert!(!h.undo(&mut d), "nothing older than the starting state");
        assert!(h.redo(&mut d));
        assert_eq!(d, vec![line(1.0), line(2.0)]);
        // A new change after undo drops the redo branch.
        d.clear();
        h.track(&d);
        assert!(!h.can_redo());
        assert!(!h.redo(&mut d));
        assert!(h.undo(&mut d));
        assert_eq!(d, vec![line(1.0), line(2.0)]);
    }

    #[test]
    fn history_flushes_untracked_changes_before_undo() {
        let line = |p: f64| Drawing::new(Tool::Horizontal, at(0, p), at(0, p));
        let mut h = History::default();
        let mut d = Vec::new();
        h.track(&d);
        // Changed (e.g. by a command) and undone before the next frame tracked it.
        d.push(line(1.0));
        assert!(h.undo(&mut d));
        assert!(d.is_empty());
        assert!(h.redo(&mut d));
        assert_eq!(d, vec![line(1.0)]);
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::default();
        let mut d = Vec::new();
        h.track(&d);
        for i in 0..History::LIMIT + 20 {
            d.push(Drawing::new(Tool::Vertical, at(i as i64, 0.0), at(i as i64, 0.0)));
            h.track(&d);
        }
        let mut steps = 0;
        while h.undo(&mut d) {
            steps += 1;
        }
        assert_eq!(steps, History::LIMIT);
        assert_eq!(d.len(), 20, "the oldest steps were dropped");
    }

    #[test]
    fn magnet_snaps() {
        assert_eq!(magnet(10.4, [10.0, 12.0, 9.0, 11.0]), 10.0);
        assert_eq!(magnet(11.7, [10.0, 12.0, 9.0, 11.0]), 12.0);
    }
}
