//! Pure viewport math: which bars are visible and how bar index / price map to pixels.
//!
//! The time axis is indexed by bar position (not wall time), like most trading charts:
//! bar `i` occupies `[i, i+1)` in index space and gaps (nights, weekends) collapse.

/// Horizontal view over a bar series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Fractional bar index at the right edge of the plot.
    pub right: f64,
    /// Horizontal pixels per bar.
    pub bar_px: f64,
    /// Plot width in pixels.
    pub width_px: f64,
}

pub const MIN_BAR_PX: f64 = 0.000_5;
pub const MAX_BAR_PX: f64 = 120.0;

impl Viewport {
    /// Show the last `count` bars of a `len`-bar series with a small right margin.
    pub fn fit_last(len: usize, count: usize, width_px: f64) -> Self {
        let count = count.clamp(1, len.max(1)) as f64;
        let margin = 3.0;
        let bar_px = (width_px / (count + margin)).clamp(MIN_BAR_PX, MAX_BAR_PX);
        Self { right: len as f64 + margin, bar_px, width_px }
    }

    pub fn visible_bars(&self) -> f64 {
        self.width_px / self.bar_px
    }

    pub fn left(&self) -> f64 {
        self.right - self.visible_bars()
    }

    /// X pixel (from the plot's left edge) of the left edge of bar `idx`.
    pub fn x_of(&self, idx: f64) -> f64 {
        (idx - self.left()) * self.bar_px
    }

    /// X pixel of the centre of bar `idx`.
    pub fn x_center(&self, idx: usize) -> f64 {
        self.x_of(idx as f64 + 0.5)
    }

    /// Fractional index under pixel `x`.
    pub fn index_at(&self, x: f64) -> f64 {
        self.left() + x / self.bar_px
    }

    /// Bar index under pixel `x`, clamped to the series.
    pub fn bar_at(&self, x: f64, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let i = self.index_at(x).floor();
        Some(i.clamp(0.0, (len - 1) as f64) as usize)
    }

    /// Half-open range of bar indices intersecting the plot.
    pub fn visible_range(&self, len: usize) -> (usize, usize) {
        let lo = self.left().floor().max(0.0) as usize;
        let hi = (self.right.ceil().max(0.0) as usize).min(len);
        (lo.min(hi), hi)
    }

    /// Pan by `dx` pixels (positive drags the chart right, revealing older bars).
    pub fn pan_px(&mut self, dx: f64) {
        self.right -= dx / self.bar_px;
    }

    pub fn pan_bars(&mut self, bars: f64) {
        self.right += bars;
    }

    /// Zoom by `factor` (>1 zooms in) keeping the bar under `anchor_x` fixed.
    pub fn zoom(&mut self, factor: f64, anchor_x: f64) {
        let idx = self.index_at(anchor_x);
        self.bar_px = (self.bar_px * factor).clamp(MIN_BAR_PX, MAX_BAR_PX);
        self.right = idx + (self.width_px - anchor_x) / self.bar_px;
    }

    /// Fit the bar-index span between `a` and `b` (either order) to the plot width,
    /// with its left end at the left edge.
    pub fn fit_range(&mut self, a: f64, b: f64) {
        self.bar_px = (self.width_px / (a - b).abs().max(2.0)).clamp(MIN_BAR_PX, MAX_BAR_PX);
        self.right = a.min(b) + self.visible_bars();
    }

    pub fn set_width(&mut self, width_px: f64) {
        // Keep the right edge pinned, like resizing a terminal pane.
        self.width_px = width_px.max(1.0);
    }

    /// Keep at least a few bars on screen, never zoom out past the whole series (with a
    /// margin), and limit the empty space on either side.
    pub fn clamp(&mut self, len: usize) {
        let min_px = self.width_px / (len.max(10) as f64 * 1.15);
        self.bar_px = self.bar_px.clamp(min_px.min(MAX_BAR_PX), MAX_BAR_PX);
        let vis = self.visible_bars();
        let min_right = (len as f64).min(5.0);
        let max_right = len as f64 + vis * 0.8;
        self.right = self.right.clamp(min_right, max_right.max(min_right));
    }
}

/// Vertical price mapping for one pane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceScale {
    pub lo: f64,
    pub hi: f64,
    pub log: bool,
    pub height_px: f64,
}

impl PriceScale {
    pub fn new(lo: f64, hi: f64, log: bool, height_px: f64) -> Self {
        let (lo, hi) = if hi > lo { (lo, hi) } else { (lo - 1.0, lo + 1.0) };
        Self { lo, hi, log: log && lo > 0.0, height_px }
    }

    /// Range padded by `margin` (fraction of height) top and bottom.
    pub fn padded(lo: f64, hi: f64, margin: f64, log: bool, height_px: f64) -> Self {
        if log && lo > 0.0 {
            let (a, b) = (lo.ln(), hi.ln());
            let pad = ((b - a).max(1e-9)) * margin;
            return Self::new((a - pad).exp(), (b + pad).exp(), true, height_px);
        }
        let pad = ((hi - lo).abs().max(hi.abs() * 1e-4).max(1e-9)) * margin;
        Self::new(lo - pad, hi + pad, false, height_px)
    }

    fn t(&self, p: f64) -> f64 {
        if self.log { p.max(f64::MIN_POSITIVE).ln() } else { p }
    }

    fn inv(&self, v: f64) -> f64 {
        if self.log { v.exp() } else { v }
    }

    /// Transformed (linear or log) bounds, as sent to the GPU.
    pub fn bounds_t(&self) -> (f64, f64) {
        (self.t(self.lo), self.t(self.hi))
    }

    /// Y pixel from the pane top for price `p`.
    pub fn y_of(&self, p: f64) -> f64 {
        let (a, b) = self.bounds_t();
        (1.0 - (self.t(p) - a) / (b - a)) * self.height_px
    }

    pub fn price_at(&self, y: f64) -> f64 {
        let (a, b) = self.bounds_t();
        self.inv(a + (1.0 - y / self.height_px) * (b - a))
    }

    /// Stretch the range about its centre (drag on the price axis); `factor > 1` zooms in.
    pub fn stretch(&mut self, factor: f64) {
        let (a, b) = self.bounds_t();
        let mid = (a + b) / 2.0;
        let half = (b - a) / 2.0 / factor.max(1e-3);
        self.lo = self.inv(mid - half);
        self.hi = self.inv(mid + half);
    }

    /// Shift the range by `dy` pixels (vertical pan).
    pub fn pan_px(&mut self, dy: f64) {
        let (a, b) = self.bounds_t();
        let d = dy / self.height_px * (b - a);
        self.lo = self.inv(a + d);
        self.hi = self.inv(b + d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn fit_range_spans_the_width() {
        let mut v = Viewport { right: 100.0, bar_px: 10.0, width_px: 500.0 };
        v.fit_range(80.0, 30.0);
        assert!(close(v.bar_px, 10.0));
        assert!(close(v.left(), 30.0));
        assert!(close(v.right, 80.0));
        // A tiny span keeps at least two bars and never exceeds the zoom limit.
        v.fit_range(40.0, 40.0);
        assert!(close(v.bar_px, MAX_BAR_PX.min(250.0)));
        assert!(close(v.left(), 40.0));
    }

    #[test]
    fn mapping_roundtrip() {
        let v = Viewport { right: 100.0, bar_px: 10.0, width_px: 500.0 };
        assert!(close(v.left(), 50.0));
        assert!(close(v.x_of(60.0), 100.0));
        assert!(close(v.index_at(100.0), 60.0));
        assert_eq!(v.visible_range(80), (50, 80));
        assert_eq!(v.bar_at(105.0, 80), Some(60));
        assert_eq!(v.bar_at(-1e9, 80), Some(0));
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut v = Viewport { right: 1_000_000.0, bar_px: 2.0, width_px: 1600.0 };
        let before = v.index_at(400.0);
        v.zoom(1.5, 400.0);
        assert!((v.index_at(400.0) - before).abs() < 1e-6);
        assert!(close(v.bar_px, 3.0));
        v.zoom(1e9, 0.0);
        assert_eq!(v.bar_px, MAX_BAR_PX);
    }

    #[test]
    fn pan_and_clamp() {
        let mut v = Viewport::fit_last(1000, 100, 1000.0);
        let r = v.right;
        v.pan_px(100.0);
        assert!(v.right < r);
        v.pan_px(-1e9);
        v.clamp(1000);
        assert!(v.right <= 1000.0 + v.visible_bars() * 0.8 + 1e-9);
        v.pan_px(1e9);
        v.clamp(1000);
        assert!(close(v.right, 5.0));
        // Zooming out stops once the whole series fits.
        v.zoom(1e-6, 500.0);
        v.clamp(1000);
        assert!(v.visible_bars() <= 1150.0 + 1e-6);
    }

    #[test]
    fn price_scale_linear_and_log() {
        let s = PriceScale::new(100.0, 200.0, false, 100.0);
        assert!(close(s.y_of(150.0), 50.0));
        assert!(close(s.price_at(25.0), 175.0));
        let l = PriceScale::new(10.0, 1000.0, true, 100.0);
        assert!(close(l.y_of(100.0), 50.0));
        assert!((l.price_at(50.0) - 100.0).abs() < 1e-9);
        let mut m = PriceScale::new(100.0, 200.0, false, 100.0);
        m.stretch(2.0);
        assert!(close(m.lo, 125.0) && close(m.hi, 175.0));
        let p = PriceScale::padded(100.0, 200.0, 0.1, false, 100.0);
        assert!(close(p.lo, 90.0) && close(p.hi, 210.0));
    }
}
