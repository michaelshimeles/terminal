//! Chart pane: GPU-rendered price/volume/indicator panes with egui-drawn axes, legend,
//! crosshair, drawings and congressional trade markers.

mod toolbar;

use std::sync::Arc;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2,
    vec2,
};
use tr_chart::drawings::{self, Anchor, Drawing, Shape, Tool};
use tr_chart::gpu::{ChartCallback, Frame, LineKind, LineRef, Style};
use tr_chart::scale;
use tr_chart::scene::{self, ChartType, Overlay, PricePane, SeriesGpu};
use tr_chart::viewport::{PriceScale, Viewport};
use tr_congress::{Trade, TxType};
use tr_core::{AssetClass, Bar, Security, Timeframe};

use crate::command::{IndicatorSpec, Range};
use crate::congress_view::Action;
use crate::theme::{self, amber, amber_dim, mono, muted, text, ui_font};
use crate::widgets;
use crate::worker::{BuiltIndicator, PaneId, Request, Worker, initial_limit};

const AXIS_W: f32 = 66.0;
const TIME_H: f32 = 16.0;
/// Fallback header row, used when the pane's tab strip has no room for the controls.
const HEADER_H: f32 = 24.0;
const RANGE_H: f32 = 18.0;
/// Height of the interval / chart-type segmented controls.
pub const CONTROL_H: f32 = 20.0;
const DEFAULT_VISIBLE: usize = 160;

#[derive(Default)]
struct DrawState {
    tool: Option<Tool>,
    /// Points placed so far for the active tool.
    pending: Vec<Anchor>,
    selected: Option<usize>,
}

/// One piece of a drawing's screen geometry, with optional label and fill.
struct Part {
    shape: Shape,
    label: Option<(String, Pos2, Align2)>,
    fill: Option<Color32>,
    /// Line colour when not selected (default cyan).
    color: Option<Color32>,
    dashed: bool,
}

impl Part {
    fn new(shape: Shape) -> Self {
        Self { shape, label: None, fill: None, color: None, dashed: false }
    }

    fn label(mut self, text: String, at: Pos2, align: Align2) -> Self {
        self.label = Some((text, at, align));
        self
    }

    fn fill(mut self, fill: Color32) -> Self {
        self.fill = Some(fill);
        self
    }

    fn color(mut self, color: Color32) -> Self {
        self.color = Some(color);
        self
    }

    fn dashed(mut self) -> Self {
        self.dashed = true;
        self
    }
}

pub struct ChartState {
    pub id: PaneId,
    pub symbol: String,
    pub tf: Timeframe,
    pub chart_type: ChartType,
    pub log: bool,
    pub magnet: bool,
    pub show_volume: bool,
    pub show_congress: bool,
    pub indicators: Vec<IndicatorSpec>,
    pub drawings: Vec<Drawing>,
    pub compare: Option<String>,
    /// Drawing toolbar open on the left edge (closed it is a slim tab).
    pub toolbar_open: bool,
    /// Keep the drawing tool active after placing a drawing.
    pub keep_drawing: bool,
    /// Drawings can't be selected or deleted one by one.
    pub lock_drawings: bool,
    pub hide_drawings: bool,

    bars: Arc<Vec<Bar>>,
    gpu: Option<SeriesGpu>,
    generation: u64,
    viewport: Option<Viewport>,
    manual_scale: Option<PriceScale>,
    loading: bool,
    paging: bool,
    exhausted: bool,
    error: Option<String>,
    built: Vec<BuiltIndicator>,
    compare_line: Option<LineRef>,
    compare_scale: f64,
    markers: Vec<Trade>,
    pending_range: Option<Range>,
    draw: DrawState,
    /// Last tool picked in each toolbar group (index into the group).
    tool_picks: [usize; toolbar::GROUPS.len()],
    hover_marker: Option<usize>,
    /// Resolved security for `symbol`.
    sec: Security,
    last_live: Option<std::time::Instant>,
    last_indicator_req: Option<std::time::Instant>,
    /// App-level actions requested from this chart (context menu), drained by the app.
    pub actions: Vec<Action>,
    /// False while a pane is being dragged over the workspace: no crosshair or hover cards.
    pub hover_enabled: bool,
    /// The workspace drew this chart's controls in the pane's tab strip, so skip the
    /// header row and give the plot the space.
    pub controls_in_strip: bool,
}

impl ChartState {
    pub fn new(id: PaneId, symbol: &str, tf: Timeframe) -> Self {
        Self {
            id,
            symbol: tr_data::security(symbol).key(),
            tf,
            chart_type: if tr_data::security(symbol).class == AssetClass::Govt {
                ChartType::Line
            } else {
                ChartType::Candles
            },
            log: false,
            magnet: false,
            show_volume: true,
            show_congress: true,
            indicators: vec![IndicatorSpec::Sma(20), IndicatorSpec::Sma(50)],
            drawings: Vec::new(),
            compare: None,
            toolbar_open: false,
            keep_drawing: false,
            lock_drawings: false,
            hide_drawings: false,
            bars: Arc::new(Vec::new()),
            gpu: None,
            generation: 0,
            viewport: None,
            manual_scale: None,
            loading: false,
            paging: false,
            exhausted: false,
            error: None,
            built: Vec::new(),
            compare_line: None,
            compare_scale: 1.0,
            markers: Vec::new(),
            pending_range: None,
            draw: DrawState::default(),
            tool_picks: [0; toolbar::GROUPS.len()],
            hover_marker: None,
            sec: tr_data::security(symbol),
            last_live: None,
            last_indicator_req: None,
            actions: Vec::new(),
            hover_enabled: true,
            controls_in_strip: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    /// (Re)load history for the current symbol and timeframe.
    pub fn load(&mut self, w: &Worker) {
        self.generation += 1;
        self.loading = true;
        self.paging = false;
        self.exhausted = false;
        self.error = None;
        self.viewport = None;
        self.manual_scale = None;
        self.compare_line = None;
        self.markers.clear();
        // Never show the previous symbol's bars under the new title.
        self.bars = Arc::new(Vec::new());
        self.gpu = None;
        self.built.clear();
        w.send(Request::History {
            pane: self.id,
            generation: self.generation,
            symbol: self.symbol.clone(),
            tf: self.tf,
            limit: initial_limit(self.tf),
        });
        self.last_live = None;
        if self.sec.has_congress_trades() {
            w.send(Request::Markers { pane: self.id, symbol: self.symbol.clone() });
        }
    }

    pub fn set_symbol(&mut self, symbol: &str, w: &Worker) {
        let sec = tr_data::security(symbol);
        if sec.class == AssetClass::Govt {
            self.chart_type = ChartType::Line;
        } else if self.sec.class == AssetClass::Govt {
            self.chart_type = ChartType::Candles;
        }
        self.symbol = sec.key();
        self.sec = sec;
        self.draw = DrawState::default();
        self.load(w);
    }

    pub fn set_timeframe(&mut self, tf: Timeframe, w: &Worker) {
        if tf != self.tf {
            self.tf = tf;
            self.load(w);
        }
    }

    pub fn set_range(&mut self, r: Range, w: &Worker) {
        self.pending_range = Some(r);
        if r.timeframe() != self.tf {
            self.tf = r.timeframe();
            self.load(w);
        } else {
            self.apply_range();
        }
    }

    fn apply_range(&mut self) {
        let Some(r) = self.pending_range.take() else { return };
        let (Some(v), Some(last)) = (self.viewport.as_mut(), self.bars.last()) else { return };
        let count = match r.span_secs(last.ts) {
            Some(span) => self.bars.len() - self.bars.partition_point(|b| b.ts < last.ts - span),
            None => self.bars.len(),
        };
        *v = Viewport::fit_last(self.bars.len(), count.max(2), v.width_px);
        self.manual_scale = None;
    }

    pub fn toggle_indicator(&mut self, spec: IndicatorSpec, w: &Worker) {
        if let Some(i) = self.indicators.iter().position(|s| s.same_kind(&spec)) {
            if self.indicators[i] == spec {
                self.indicators.remove(i);
            } else {
                self.indicators[i] = spec;
            }
        } else {
            self.indicators.push(spec);
        }
        self.request_indicators(w);
    }

    pub fn clear_indicators(&mut self) {
        self.indicators.clear();
        self.built.clear();
    }

    pub fn set_compare(&mut self, symbol: Option<String>, w: &Worker) {
        self.compare = symbol;
        self.compare_line = None;
        self.request_compare(w);
    }

    pub fn start_tool(&mut self, tool: Tool) {
        self.draw.tool = Some(tool);
        self.draw.pending.clear();
        // The toolbar group shows the tool last used from it, hotkeys included.
        if let Some((g, k)) = toolbar::group_of(tool) {
            self.tool_picks[g] = k;
        }
    }

    /// Esc: cancel an in-progress drawing, then deselect. Returns whether it consumed the key.
    pub fn cancel(&mut self) -> bool {
        if self.draw.tool.is_some() || self.draw.selected.is_some() {
            self.draw = DrawState::default();
            true
        } else {
            false
        }
    }

    pub fn delete_selected(&mut self) -> bool {
        if self.lock_drawings {
            return false;
        }
        match self.draw.selected.take() {
            Some(i) if i < self.drawings.len() => {
                self.drawings.remove(i);
                true
            }
            _ => false,
        }
    }

    /// Zoom the time axis to the span between two anchors (the zoom tool).
    fn zoom_to(&mut self, a: &Anchor, b: &Anchor) {
        let (ia, ib) = (self.index_of_ts(a.ts), self.index_of_ts(b.ts));
        if let Some(v) = self.viewport.as_mut() {
            v.fit_range(ia, ib);
            v.clamp(self.bars.len());
        }
        self.manual_scale = None;
    }

    pub fn reset_view(&mut self) {
        if let Some(v) = self.viewport.as_mut() {
            *v = Viewport::fit_last(self.bars.len(), DEFAULT_VISIBLE, v.width_px);
        }
        self.manual_scale = None;
    }

    pub fn pan_bars(&mut self, n: f64) {
        if let Some(v) = self.viewport.as_mut() {
            v.pan_bars(n);
            v.clamp(self.bars.len());
        }
    }

    pub fn zoom(&mut self, factor: f64) {
        if let Some(v) = self.viewport.as_mut() {
            let anchor = v.width_px * 0.85;
            v.zoom(factor, anchor);
            v.clamp(self.bars.len());
        }
    }

    fn request_indicators(&self, w: &Worker) {
        if !self.bars.is_empty() {
            w.send(Request::Indicators {
                pane: self.id,
                generation: self.generation,
                bars: self.bars.clone(),
                tf: self.tf,
                specs: self.indicators.clone(),
                palette: theme::palette(),
            });
        }
    }

    fn request_compare(&self, w: &Worker) {
        if let (Some(sym), false) = (&self.compare, self.bars.is_empty()) {
            w.send(Request::Compare {
                pane: self.id,
                generation: self.generation,
                symbol: sym.clone(),
                tf: self.tf,
                base: self.bars.clone(),
            });
        }
    }

    // ---- worker responses -------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn on_history(
        &mut self,
        generation: u64,
        bars: Arc<Vec<Bar>>,
        gpu: SeriesGpu,
        prepended: usize,
        from_cache: bool,
        exhausted: bool,
        live: bool,
        w: &Worker,
    ) {
        if generation != self.generation {
            return;
        }
        if live {
            self.last_live = Some(std::time::Instant::now());
        }
        let old_len = self.bars.len();
        let new_len = bars.len();
        self.bars = bars;
        self.gpu = Some(gpu);
        self.paging = false;
        self.exhausted = exhausted || self.exhausted && prepended == 0;
        if !from_cache {
            self.loading = false;
        }
        self.error = None;
        match self.viewport.as_mut() {
            None => {
                self.viewport = Some(Viewport::fit_last(new_len, DEFAULT_VISIBLE, 800.0));
            }
            Some(v) if prepended > 0 => v.right += prepended as f64,
            Some(v) => {
                // A refresh replaced the data: keep the same distance from the latest bar.
                let from_end = old_len as f64 - v.right;
                v.right = new_len as f64 - from_end;
            }
        }
        self.apply_range();
        // Live ticks only move the last bar: refresh indicators at most every 1.5 s and
        // leave the compare series alone.
        let due = self.last_indicator_req.is_none_or(|t| t.elapsed().as_millis() >= 1500);
        if !live || due {
            self.request_indicators(w);
            self.last_indicator_req = Some(std::time::Instant::now());
        }
        if !live {
            self.request_compare(w);
        }
    }

    pub fn on_history_error(&mut self, generation: u64, error: String) {
        if generation == self.generation {
            self.loading = false;
            self.paging = false;
            self.exhausted = true;
            if self.bars.is_empty() {
                self.error = Some(error);
            }
        }
    }

    pub fn on_indicators(&mut self, generation: u64, built: Vec<BuiltIndicator>) {
        if generation == self.generation {
            self.built = built;
        }
    }

    pub fn on_compare(&mut self, generation: u64, symbol: &str, line: LineRef) {
        if generation == self.generation && self.compare.as_deref() == Some(symbol) {
            self.compare_line = Some(line);
        }
    }

    pub fn on_markers(&mut self, symbol: &str, trades: Vec<Trade>) {
        if symbol == self.symbol {
            self.markers = trades;
        }
    }

    // ---- coordinate helpers -----------------------------------------------------------

    /// Fractional bar index for a timestamp (extrapolated past either end).
    fn index_of_ts(&self, ts: i64) -> f64 {
        let b = &self.bars;
        let (Some(first), Some(last)) = (b.first(), b.last()) else { return 0.0 };
        let step = self.tf.seconds() as f64;
        if ts > last.ts {
            return (b.len() - 1) as f64 + (ts - last.ts) as f64 / step + 0.5;
        }
        if ts < first.ts {
            return (ts - first.ts) as f64 / step + 0.5;
        }
        b.partition_point(|x| x.ts <= ts).saturating_sub(1) as f64 + 0.5
    }

    fn ts_of_index(&self, idx: f64) -> i64 {
        let b = &self.bars;
        let Some(last) = b.last() else { return 0 };
        let i = idx.floor();
        if i >= b.len() as f64 {
            return last.ts + ((i - (b.len() - 1) as f64) * self.tf.seconds() as f64) as i64;
        }
        if i < 0.0 {
            return b[0].ts + (i * self.tf.seconds() as f64) as i64;
        }
        b[i as usize].ts
    }

    // ---- rendering --------------------------------------------------------------------

    /// Draw the pane into `rect`. Returns true if the pane was clicked (focus request).
    /// Width of [`Self::controls`] (interval and chart-type segmented controls).
    pub fn controls_width(ui: &Ui) -> f32 {
        let tf: Vec<&str> = Timeframe::ALL.iter().map(|t| t.label()).collect();
        let ct: Vec<&str> = ChartType::ALL.iter().map(|c| c.label()).collect();
        widgets::segmented_width(ui, &tf, CONTROL_H) + widgets::segmented_width(ui, &ct, CONTROL_H) + 8.0
    }

    /// Interval and chart-type controls, laid out left to right in `ui`. Returns whether
    /// one was clicked.
    pub fn controls(&mut self, ui: &mut Ui, w: &Worker) -> bool {
        let mut clicked = false;
        let tf_labels: Vec<&str> = Timeframe::ALL.iter().map(|t| t.label()).collect();
        let selected_tf = Timeframe::ALL.iter().position(|t| *t == self.tf);
        if let Some(i) = widgets::segmented(ui, (self.id, "tf"), &tf_labels, selected_tf, CONTROL_H) {
            self.set_timeframe(Timeframe::ALL[i], w);
            clicked = true;
        }
        ui.add_space(8.0);
        let ct_labels: Vec<&str> = ChartType::ALL.iter().map(|c| c.label()).collect();
        let selected_ct = ChartType::ALL.iter().position(|c| *c == self.chart_type);
        if let Some(i) = widgets::segmented(ui, (self.id, "ct"), &ct_labels, selected_ct, CONTROL_H) {
            self.chart_type = ChartType::ALL[i];
            clicked = true;
        }
        clicked
    }

    pub fn show(&mut self, ui: &mut Ui, rect: Rect, focused: bool, w: &Worker) -> bool {
        let painter = ui.painter_at(rect);
        // Sits under a tab strip inside a card: square top, rounded bottom.
        let r = theme::CARD_RADIUS.saturating_sub(1);
        painter.rect_filled(rect, CornerRadius { nw: 0, ne: 0, sw: r, se: r }, theme::card());
        let mut clicked = false;

        // Controls live in the tab strip when it has room; otherwise a slim header row.
        let header_h = if self.controls_in_strip { 0.0 } else { HEADER_H };
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), header_h));
        if header_h > 0.0 {
            painter.hline(header.x_range(), header.bottom(), Stroke::new(1.0, theme::border()));
            let mut hui = ui.new_child(
                UiBuilder::new()
                    .max_rect(header.shrink2(vec2(8.0, 0.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center))
                    .id_salt((self.id, "header")),
            );
            hui.spacing_mut().item_spacing.x = 6.0;
            hui.label(RichText::new(&self.sec.ticker).font(mono(12.0)).color(amber()));
            if hui.available_width() > Self::controls_width(&hui) {
                clicked |= self.controls(&mut hui, w);
            } else {
                // Narrow pane: the same controls as compact menus.
                hui.menu_button(RichText::new(self.tf.label()).font(ui_font(11.5)), |ui| {
                    for tf in Timeframe::ALL {
                        if ui.radio(self.tf == tf, tf.label()).clicked() {
                            self.set_timeframe(tf, w);
                            clicked = true;
                            ui.close();
                        }
                    }
                });
                hui.menu_button(RichText::new(self.chart_type.label()).font(ui_font(11.5)), |ui| {
                    for ct in ChartType::ALL {
                        if ui.radio(self.chart_type == ct, ct.label()).clicked() {
                            self.chart_type = ct;
                            clicked = true;
                            ui.close();
                        }
                    }
                });
            }
        }
        if self.last_live.is_some_and(|t| t.elapsed().as_secs() < 5) {
            // Pulsing live dot in the plot's top-right corner (clear of the price axis labels).
            let pulse = 0.6 + 0.4 * ((ui.input(|i| i.time) * 3.0).sin() as f32).abs();
            let c = pos2(rect.right() - AXIS_W + 12.0, header.bottom() + 10.0);
            painter.circle_filled(c, 3.0, theme::up().gamma_multiply(pulse));
            painter.text(c + vec2(7.0, 0.0), Align2::LEFT_CENTER, "LIVE", ui_font(10.0), theme::up());
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Bottom quick-range row.
        let range_row = Rect::from_min_max(pos2(rect.left(), rect.bottom() - RANGE_H), rect.max);
        let mut rx = range_row.left() + 6.0;
        for r in Range::ALL {
            let lr = Rect::from_min_size(pos2(rx, range_row.top()), vec2(r.label().len() as f32 * 7.4 + 8.0, RANGE_H));
            let resp = ui.interact(lr, ui.id().with((self.id, "range", r.label())), Sense::click());
            painter.text(
                lr.center(),
                Align2::CENTER_CENTER,
                r.label(),
                mono(11.0),
                if resp.hovered() { amber() } else { muted() },
            );
            if resp.clicked() {
                self.set_range(r, w);
                clicked = true;
            }
            rx = lr.right();
        }
        let status = match (&self.error, self.loading, self.paging) {
            (Some(e), _, _) => format!("ERR {e}"),
            (_, true, _) => "loading…".into(),
            (_, _, true) => "loading history…".into(),
            _ => format!(
                "{} bars{}{}",
                self.bars.len(),
                if self.log { "  LOG" } else { "" },
                if self.magnet { "  MAGNET" } else { "" }
            ),
        };
        painter.text(
            pos2(range_row.right() - 6.0, range_row.center().y),
            Align2::RIGHT_CENTER,
            status,
            mono(10.5),
            muted(),
        );

        let body = Rect::from_min_max(pos2(rect.left(), header.bottom()), pos2(rect.right(), range_row.top()));
        // The open drawing toolbar takes a strip on the left; the plot starts after it.
        let strip = Rect::from_min_size(body.min, vec2(toolbar::STRIP_W, body.height()));
        let plot_left = if self.toolbar_open { strip.right() } else { body.left() };
        let plot_all =
            Rect::from_min_max(pos2(plot_left, body.top()), pos2(body.right() - AXIS_W, body.bottom() - TIME_H));
        if plot_all.width() < 40.0 || plot_all.height() < 40.0 {
            return clicked;
        }
        if self.toolbar_open {
            clicked |= self.toolbar_strip(ui, strip);
        }

        let Some(gpu) = self.gpu.clone() else {
            let msg = self.error.clone().unwrap_or_else(|| format!("Loading {} {}…", self.symbol, self.tf));
            painter.text(plot_all.center(), Align2::CENTER_CENTER, msg, mono(13.0), amber_dim());
            self.toolbar_tab(ui, plot_all);
            return clicked;
        };

        // Split plot height between the price pane and oscillator sub-panes.
        let pane_specs: Vec<usize> = (0..self.built.len()).filter(|&i| self.built[i].spec.is_pane()).collect();
        let n_sub = pane_specs.len().min(3);
        let sub_h = if n_sub > 0 { (plot_all.height() * 0.2).clamp(50.0, 160.0) } else { 0.0 };
        let price_rect =
            Rect::from_min_max(plot_all.min, pos2(plot_all.right(), plot_all.bottom() - sub_h * n_sub as f32));

        let len = self.bars.len();
        let v =
            self.viewport.get_or_insert_with(|| Viewport::fit_last(len, DEFAULT_VISIBLE, price_rect.width() as f64));
        v.set_width(price_rect.width() as f64);

        // ---- input ----
        let plot_resp = ui.interact(plot_all, ui.id().with((self.id, "plot")), Sense::click_and_drag());
        let axis_rect =
            Rect::from_min_max(pos2(price_rect.right(), price_rect.top()), pos2(body.right(), price_rect.bottom()));
        let axis_resp = ui.interact(axis_rect, ui.id().with((self.id, "paxis")), Sense::click_and_drag());
        let time_rect =
            Rect::from_min_max(pos2(plot_all.left(), plot_all.bottom()), pos2(plot_all.right(), body.bottom()));
        let time_resp = ui.interact(time_rect, ui.id().with((self.id, "taxis")), Sense::click_and_drag());
        if plot_resp.clicked() || plot_resp.drag_started() || axis_resp.clicked() || time_resp.clicked() {
            clicked = true;
        }
        let hover = plot_resp.hover_pos().filter(|p| self.hover_enabled && plot_all.contains(*p));
        if self.draw.tool.is_none() {
            plot_resp.context_menu(|ui| self.context_menu(ui, w));
        }

        if plot_resp.hovered() || axis_resp.hovered() || time_resp.hovered() {
            let (scroll, zoom_delta) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            let v = self.viewport.as_mut().expect("viewport");
            if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                let anchor = (p.x - price_rect.left()).clamp(0.0, price_rect.width()) as f64;
                if scroll.y != 0.0 {
                    v.zoom((scroll.y as f64 * 0.003).exp(), anchor);
                }
                if zoom_delta != 1.0 {
                    v.zoom(zoom_delta as f64, anchor);
                }
            }
            if scroll.x != 0.0 {
                v.pan_px(scroll.x as f64);
            }
        }
        if self.draw.tool.is_none() && plot_resp.dragged() {
            let d = plot_resp.drag_delta();
            self.viewport.as_mut().expect("viewport").pan_px(d.x as f64);
            if let Some(s) = self.manual_scale.as_mut() {
                s.pan_px(d.y as f64);
            }
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
        if time_resp.dragged() {
            let v = self.viewport.as_mut().expect("viewport");
            let w_px = v.width_px;
            v.zoom((-time_resp.drag_delta().x as f64 * 0.005).exp(), w_px);
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if time_resp.double_clicked() || plot_resp.double_clicked() && self.draw.tool.is_none() {
            self.reset_view();
        }
        let v = *self.viewport.as_ref().expect("viewport");
        let mut v = v;
        v.clamp(len);
        self.viewport = Some(v);

        // Page older history when the left edge approaches loaded data.
        if !self.paging && !self.loading && !self.exhausted && v.left() < v.visible_bars() * 0.3 {
            self.paging = true;
            w.send(Request::MoreHistory {
                pane: self.id,
                generation: self.generation,
                symbol: self.symbol.clone(),
                tf: self.tf,
                existing: self.bars.clone(),
                limit: initial_limit(self.tf),
            });
        }

        // ---- price pane GPU frame ----
        let mut overlays: Vec<Overlay> = self
            .built
            .iter()
            .filter(|b| !b.spec.is_pane())
            .flat_map(|b| b.overlays.iter().map(|o| o.1.clone()))
            .collect();
        if let Some(c) = &self.compare_line {
            let (lo, hi) = self.viewport.as_ref().map(|v| v.visible_range(len)).unwrap_or((0, len));
            let scale = scene::rebase_factor(&gpu.closes, c, lo, hi);
            self.compare_scale = scale;
            overlays.push(Overlay {
                line: c.clone(),
                color: theme::rgba(theme::series(5)),
                kind: LineKind::Line,
                scale,
            });
        }
        let style = Style {
            up: theme::rgba(theme::candle_up()),
            down: theme::rgba(theme::candle_down()),
            color: theme::rgba(amber()),
            pixels_per_point: ui.ctx().pixels_per_point(),
            ..Style::default()
        };
        let out = scene::price_pane(&PricePane {
            series: &gpu,
            chart_type: self.chart_type,
            show_volume: self.show_volume,
            overlays: &overlays,
            viewport: &v,
            height_px: price_rect.height() as f64,
            log: self.log,
            manual_scale: self.manual_scale.map(|mut s| {
                s.height_px = price_rect.height() as f64;
                s
            }),
            style,
        });
        let mut scale = out.scale;
        scale.height_px = price_rect.height() as f64;
        if axis_resp.dragged() {
            let mut s = self.manual_scale.unwrap_or(scale);
            s.stretch((axis_resp.drag_delta().y as f64 * -0.01).exp());
            self.manual_scale = Some(s);
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        }
        if axis_resp.double_clicked() {
            self.manual_scale = None;
        }

        // Grid first (under the GPU geometry is not possible inside one pass, so draw it
        // faintly; candles are opaque on top visually).
        let price_painter = ui.painter_at(price_rect);
        let ticks = scale::price_ticks(scale.lo, scale.hi, (price_rect.height() / 42.0).max(2.0) as usize);
        let step = if ticks.len() > 1 { ticks[1] - ticks[0] } else { 1.0 };
        let dec = scale::decimals_for(step).max(self.sec.decimals(scale.hi).min(3));
        for t in &ticks {
            let y = price_rect.top() + scale.y_of(*t) as f32;
            price_painter.hline(price_rect.x_range(), y, Stroke::new(1.0, theme::grid()));
        }
        let frame_no = ui.ctx().cumulative_frame_nr();
        let frame = Frame { pane: self.id * 8, frame_no, cmds: out.cmds };
        ui.painter().add(tr_chart::egui_wgpu::Callback::new_paint_callback(price_rect, ChartCallback(Arc::new(frame))));

        // Sub panes.
        let mut sub_rects = Vec::new();
        for (k, &bi) in pane_specs.iter().take(n_sub).enumerate() {
            let b = &self.built[bi];
            let top = price_rect.bottom() + sub_h * k as f32;
            let r = Rect::from_min_max(pos2(plot_all.left(), top), pos2(plot_all.right(), top + sub_h));
            let fixed = matches!(b.spec, IndicatorSpec::Rsi(_)).then_some((0.0, 100.0));
            let ovs: Vec<Overlay> = b.overlays.iter().map(|o| o.1.clone()).collect();
            let mut s = style;
            s.thickness_px = 1.3;
            let so = scene::sub_pane(&ovs, &v, r.height() as f64, fixed, s);
            let p = ui.painter_at(r);
            p.hline(r.x_range(), r.top(), Stroke::new(1.0, theme::border()));
            if matches!(b.spec, IndicatorSpec::Rsi(_)) {
                for lvl in [30.0, 70.0] {
                    p.hline(
                        r.x_range(),
                        r.top() + so.scale.y_of(lvl) as f32,
                        Stroke::new(1.0, Color32::from_rgb(60, 45, 80)),
                    );
                }
            }
            let frame = Frame { pane: self.id * 8 + 1 + k as u64, frame_no, cmds: so.cmds };
            ui.painter().add(tr_chart::egui_wgpu::Callback::new_paint_callback(r, ChartCallback(Arc::new(frame))));
            sub_rects.push((r, bi, so.scale));
        }

        // ---- axes ----
        let axis_painter = ui.painter_at(Rect::from_min_max(pos2(plot_all.right(), body.top()), body.max));
        axis_painter.vline(plot_all.right(), body.y_range(), Stroke::new(1.0, theme::border()));
        let positive_prices = self.bars.last().is_some_and(|b| b.low > 0.0);
        for t in &ticks {
            let y = price_rect.top() + scale.y_of(*t) as f32;
            // The volume headroom can dip below zero; don't label impossible prices.
            if positive_prices && *t < 0.0 {
                continue;
            }
            if y > price_rect.top() + 6.0 && y < price_rect.bottom() - 6.0 {
                axis_painter.text(
                    pos2(plot_all.right() + 6.0, y),
                    Align2::LEFT_CENTER,
                    scale::format_price(*t, dec),
                    mono(11.0),
                    muted(),
                );
            }
        }
        for (r, bi, s) in &sub_rects {
            let label = self.built[*bi].spec.label();
            ui.painter_at(*r).text(r.left_top() + vec2(6.0, 3.0), Align2::LEFT_TOP, label, mono(10.5), muted());
            for t in scale::price_ticks(s.lo, s.hi, 3) {
                let y = r.top() + s.y_of(t) as f32;
                if y > r.top() + 8.0 && y < r.bottom() - 4.0 {
                    axis_painter.text(
                        pos2(plot_all.right() + 6.0, y),
                        Align2::LEFT_CENTER,
                        format!("{t:.1}"),
                        mono(10.5),
                        muted(),
                    );
                }
            }
        }
        // Time axis labels (sampled so 1M visible bars stays cheap).
        let (lo, hi) = v.visible_range(len);
        let time_painter = ui.painter_at(time_rect);
        time_painter.hline(time_rect.x_range(), time_rect.top(), Stroke::new(1.0, theme::border()));
        if hi > lo {
            let stride = ((hi - lo) / 600).max(1);
            let idxs: Vec<usize> = (lo..hi).step_by(stride).collect();
            let times: Vec<i64> = idxs.iter().map(|&i| self.bars[i].ts).collect();
            let gap = 90.0 / (v.bar_px * stride as f64);
            for (j, label) in scale::time_ticks(&times, 0, times.len(), gap, self.tf) {
                let x = price_rect.left() + v.x_center(idxs[j]) as f32;
                ui.painter_at(plot_all).vline(x, plot_all.y_range(), Stroke::new(1.0, theme::grid()));
                time_painter.text(pos2(x, time_rect.center().y), Align2::CENTER_CENTER, label, mono(10.5), muted());
            }
        }

        // Last price line + axis tag.
        if let Some(last) = self.bars.last() {
            let y = price_rect.top() + scale.y_of(last.close) as f32;
            if price_rect.y_range().contains(y) {
                let col = if last.is_up() { theme::up() } else { theme::down() };
                let mut xx = price_rect.left();
                while xx < price_rect.right() {
                    price_painter.hline(
                        xx..=(xx + 3.0).min(price_rect.right()),
                        y,
                        Stroke::new(1.0, col.gamma_multiply(0.6)),
                    );
                    xx += 6.0;
                }
                let tag = Rect::from_min_size(pos2(plot_all.right() + 1.0, y - 8.0), vec2(AXIS_W - 2.0, 16.0));
                axis_painter.rect_filled(tag, 0.0, col);
                axis_painter.text(
                    tag.left_center() + vec2(5.0, 0.0),
                    Align2::LEFT_CENTER,
                    scale::format_price(last.close, self.sec.decimals(last.close)),
                    mono(11.0),
                    Color32::BLACK,
                );
            }
        }

        // ---- drawings ----
        self.paint_drawings(ui, price_rect, &v, &scale, hover, &plot_resp);

        // ---- congress markers ----
        self.hover_marker = None;
        if self.show_congress {
            self.paint_markers(ui, price_rect, &v, &scale, hover);
        }

        // ---- crosshair + legend ----
        let hovered_bar = hover.and_then(|p| v.bar_at((p.x - price_rect.left()) as f64, len));
        if let Some(p) = hover {
            let ch = Stroke::new(1.0, Color32::from_gray(110));
            let plot_painter = ui.painter_at(plot_all);
            let xi = hovered_bar.map(|i| price_rect.left() + v.x_center(i) as f32).unwrap_or(p.x);
            dashed_v(&plot_painter, xi, plot_all.y_range(), ch);
            dashed_h(&plot_painter, plot_all.x_range(), p.y, ch);
            if price_rect.contains(p) {
                let price = scale.price_at((p.y - price_rect.top()) as f64);
                let tag = Rect::from_min_size(pos2(plot_all.right() + 1.0, p.y - 8.0), vec2(AXIS_W - 2.0, 16.0));
                axis_painter.rect_filled(tag, 0.0, Color32::from_gray(70));
                axis_painter.text(
                    tag.left_center() + vec2(5.0, 0.0),
                    Align2::LEFT_CENTER,
                    scale::format_price(price, self.sec.decimals(price)),
                    mono(11.0),
                    text(),
                );
            }
            if let Some(i) = hovered_bar {
                let label = scale::crosshair_label(self.bars[i].ts, self.tf);
                let gw = label.len() as f32 * 7.0 + 10.0;
                let tag = Rect::from_center_size(pos2(xi, time_rect.center().y), vec2(gw, TIME_H));
                time_painter.rect_filled(tag, 0.0, Color32::from_gray(70));
                time_painter.text(tag.center(), Align2::CENTER_CENTER, label, mono(10.5), text());
            }
            if self.draw.tool.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            }
        }
        self.paint_legend(ui, price_rect, hovered_bar.unwrap_or(len.saturating_sub(1)), &sub_rects);
        // Last, so it sits above the plot for clicks.
        self.toolbar_tab(ui, plot_all);

        let _ = focused; // the surrounding card shows focus
        clicked
    }

    fn context_menu(&mut self, ui: &mut Ui, w: &Worker) {
        ui.set_min_width(190.0);
        if ui.button("Reset View").clicked() {
            self.reset_view();
            ui.close();
        }
        ui.menu_button("Chart Type", |ui| {
            for ct in ChartType::ALL {
                if ui.radio(self.chart_type == ct, ct.label()).clicked() {
                    self.chart_type = ct;
                    ui.close();
                }
            }
        });
        ui.menu_button("Indicators", |ui| {
            for spec in [
                IndicatorSpec::Sma(20),
                IndicatorSpec::Sma(50),
                IndicatorSpec::Ema(20),
                IndicatorSpec::Vwap,
                IndicatorSpec::Bollinger(20, 2.0),
                IndicatorSpec::Rsi(14),
                IndicatorSpec::Macd(12, 26, 9),
            ] {
                let on = self.indicators.contains(&spec);
                if ui.checkbox(&mut on.clone(), spec.label()).clicked() {
                    self.toggle_indicator(spec, w);
                }
            }
            ui.separator();
            if ui.button("Remove All").clicked() {
                self.clear_indicators();
                ui.close();
            }
        });
        ui.menu_button("Draw", |ui| {
            for (name, tools) in toolbar::GROUPS {
                ui.menu_button(name, |ui| {
                    for &tool in tools {
                        let b = egui::Button::new(tool.label())
                            .shortcut_text(toolbar::shortcut_text(tool).unwrap_or_default());
                        if ui.add(b).clicked() {
                            self.start_tool(tool);
                            ui.close();
                        }
                    }
                });
            }
            for tool in [Tool::Measure, Tool::Zoom] {
                if ui.button(tool.label()).clicked() {
                    self.start_tool(tool);
                    ui.close();
                }
            }
            ui.separator();
            ui.checkbox(&mut self.keep_drawing, "Keep Drawing");
            ui.checkbox(&mut self.lock_drawings, "Lock Drawings");
            ui.checkbox(&mut self.hide_drawings, "Hide Drawings");
            if ui.button("Remove All Drawings").clicked() {
                self.drawings.clear();
                ui.close();
            }
        });
        ui.checkbox(&mut self.toolbar_open, "Drawing Toolbar");
        ui.checkbox(&mut self.log, "Log Scale");
        ui.checkbox(&mut self.magnet, "Magnet");
        ui.checkbox(&mut self.show_volume, "Volume");
        ui.separator();
        if ui.button("Add to Watchlist").clicked() {
            self.actions.push(Action::AddToWatchlist(self.symbol.clone()));
            ui.close();
        }
        if self.sec.has_congress_trades() {
            ui.checkbox(&mut self.show_congress, "Congress Trade Markers");
            if ui.button("Who in Congress Traded This…").clicked() {
                self.actions.push(Action::OpenTicker(self.symbol.clone()));
                ui.close();
            }
        }
    }

    fn paint_legend(&self, ui: &Ui, price_rect: Rect, i: usize, sub_rects: &[(Rect, usize, PriceScale)]) {
        let Some(b) = self.bars.get(i) else { return };
        let p = ui.painter_at(price_rect);
        let prev = if i > 0 { self.bars[i - 1].close } else { b.open };
        let chg = b.close - prev;
        let col = if chg >= 0.0 { theme::up() } else { theme::down() };
        let dec = self.sec.decimals(b.close);
        let mut x = price_rect.left() + 6.0;
        let y = price_rect.top() + 4.0;
        let put = |text: String, color: Color32, x: &mut f32| {
            let g = p.layout_no_wrap(text, mono(11.5), color);
            p.galley(pos2(*x, y), g.clone(), color);
            *x += g.size().x + 8.0;
        };
        put(format!("{} · {}", self.sec.ticker, self.tf), amber(), &mut x);
        for (k, val) in [("O", b.open), ("H", b.high), ("L", b.low), ("C", b.close)] {
            put(format!("{k}{val:.dec$}"), col, &mut x);
        }
        put(format!("{:+.dec$} ({:+.2}%)", chg, if prev != 0.0 { chg / prev * 100.0 } else { 0.0 }), col, &mut x);
        put(format!("Vol {}", scale::format_volume(b.volume)), muted(), &mut x);

        // Overlay indicator values, one per line.
        let mut ly = y + 16.0;
        let value_at = |l: &LineRef| l.pyramid.data.get(i).map(|p| p[1] as f64).filter(|v| v.is_finite());
        for bi in self.built.iter().filter(|b| !b.spec.is_pane()) {
            let vals: Vec<String> =
                bi.overlays.iter().filter_map(|(_, o)| value_at(&o.line).map(|v| format!("{v:.2}"))).collect();
            let c = bi
                .overlays
                .get(bi.overlays.len() / 2)
                .map(|o| {
                    Color32::from_rgba_unmultiplied(
                        (o.1.color[0] * 255.0) as u8,
                        (o.1.color[1] * 255.0) as u8,
                        (o.1.color[2] * 255.0) as u8,
                        255,
                    )
                })
                .unwrap_or(text());
            p.text(
                pos2(price_rect.left() + 6.0, ly),
                Align2::LEFT_TOP,
                format!("{}  {}", bi.spec.label(), vals.join(" ")),
                mono(11.0),
                c,
            );
            ly += 14.0;
        }
        if let (Some(sym), Some(line)) = (&self.compare, &self.compare_line) {
            // Raw price plus performance relative to the left edge of the view.
            let raw = value_at(line);
            let rel = raw.zip(self.bars.get(i)).map(|(r, b)| (r * self.compare_scale / b.close - 1.0) * 100.0);
            let text = match (raw, rel) {
                (Some(r), Some(rel)) => format!("CMP {sym}  {r:.2}  ({rel:+.2}% vs {})", self.symbol),
                _ => format!("CMP {sym}"),
            };
            p.text(pos2(price_rect.left() + 6.0, ly), Align2::LEFT_TOP, text, mono(11.0), theme::series(5));
            ly += 14.0;
        }
        if let Some(tool) = self.draw.tool {
            let keep = if self.keep_drawing { ", keep drawing on" } else { "" };
            p.text(
                pos2(price_rect.left() + 6.0, ly),
                Align2::LEFT_TOP,
                format!("{}: {} (Esc cancels{keep})", tool.label(), tool.step(self.draw.pending.len())),
                mono(11.0),
                amber(),
            );
        }
        for (r, bi, _) in sub_rects {
            let b = &self.built[*bi];
            let vals: Vec<String> =
                b.overlays.iter().filter_map(|(n, o)| value_at(&o.line).map(|v| format!("{n} {v:.2}"))).collect();
            ui.painter_at(*r).text(
                r.left_top() + vec2(90.0, 3.0),
                Align2::LEFT_TOP,
                vals.join("  "),
                mono(10.5),
                text(),
            );
        }
    }

    /// Screen geometry of a drawing: shapes for painting and hit testing, with labels.
    fn drawing_parts(&self, d: &Drawing, rect: Rect, v: &Viewport, s: &PriceScale) -> Vec<Part> {
        let px =
            |a: &Anchor| [rect.left() as f64 + v.x_of(self.index_of_ts(a.ts)), rect.top() as f64 + s.y_of(a.price)];
        let (pa, pb, pc) = (px(&d.a), px(&d.b), d.c.as_ref().map(px));
        let (min, max) = ([rect.left() as f64, rect.top() as f64], [rect.right() as f64, rect.bottom() as f64]);
        let price = |p: f64| scale::format_price(p, self.sec.decimals(p));
        let seg = |a: [f64; 2], b: [f64; 2]| Part::new(Shape::Segment(a, b));
        let hline_label = |y: f64, p: f64| (price(p), pos2(rect.right() - 4.0, y as f32 - 1.0), Align2::RIGHT_BOTTOM);
        let levels = |levels: Vec<(f64, f64)>, x0: f64, x1: f64| -> Vec<Part> {
            levels
                .into_iter()
                .map(|(r, p)| {
                    let y = rect.top() as f64 + s.y_of(p);
                    seg([x0, y], [x1, y]).label(
                        format!("{r:.3} ({})", price(p)),
                        pos2(x0 as f32 + 3.0, y as f32 - 1.0),
                        Align2::LEFT_BOTTOM,
                    )
                })
                .collect()
        };
        match d.tool {
            Tool::Trendline => vec![seg(pa, pb)],
            Tool::Ray | Tool::ExtendedLine => drawings::extend_line(pa, pb, min, max, d.tool == Tool::ExtendedLine)
                .map(|(a, b)| vec![seg(a, b)])
                .unwrap_or_default(),
            Tool::Horizontal => {
                let (t, p, al) = hline_label(pa[1], d.a.price);
                vec![Part::new(Shape::HLine(pa[1])).label(t, p, al)]
            }
            Tool::HorizontalRay => {
                let (t, p, al) = hline_label(pa[1], d.a.price);
                vec![seg(pa, [max[0], pa[1]]).label(t, p, al)]
            }
            Tool::Vertical => vec![Part::new(Shape::VLine(pa[0]))],
            Tool::CrossLine => {
                let (t, p, al) = hline_label(pa[1], d.a.price);
                vec![Part::new(Shape::HLine(pa[1])).label(t, p, al), Part::new(Shape::VLine(pa[0]))]
            }
            Tool::ParallelChannel => {
                let mut parts = vec![seg(pa, pb)];
                if let Some(c) = pc {
                    let dy = drawings::parallel_offset(pa, pb, c);
                    let shift = |q: [f64; 2], k: f64| [q[0], q[1] + dy * k];
                    parts.push(seg(shift(pa, 1.0), shift(pb, 1.0)));
                    parts.push(seg(shift(pa, 0.5), shift(pb, 0.5)).dashed());
                }
                parts
            }
            Tool::Fib => {
                let (x0, x1) = (pa[0].min(pb[0]), pa[0].max(pb[0]));
                levels(d.fib_levels(), x0, x1)
            }
            Tool::FibExtension => {
                let mut parts = vec![seg(pa, pb).dashed()];
                if let Some(c) = pc {
                    parts.push(seg(pb, c).dashed());
                }
                let x0 = pc.unwrap_or(pb)[0];
                let x1 = x0 + (pb[0] - pa[0]).abs().max(60.0);
                parts.extend(levels(d.fib_extension_levels(), x0, x1));
                parts
            }
            Tool::Rectangle => vec![Part::new(Shape::Rect(pa, pb)).fill(theme::cyan().gamma_multiply(0.12))],
            Tool::LongPosition | Tool::ShortPosition => {
                let Some(stats) = d.position() else { return Vec::new() };
                // Too narrow to read: give the box a default width to the right of the entry.
                let x1 = if (pb[0] - pa[0]).abs() < 24.0 { pa[0] + 120.0 } else { pb[0] };
                let (xl, xr) = (pa[0].min(x1), pa[0].max(x1));
                let inside = |y: f64, from: f64| {
                    let above = y < from;
                    let ly = if above { y + 2.0 } else { y - 2.0 };
                    (pos2(xl as f32 + 4.0, ly as f32), if above { Align2::LEFT_TOP } else { Align2::LEFT_BOTTOM })
                };
                let (tp, ta) = inside(pb[1], pa[1]);
                let mut parts = vec![
                    Part::new(Shape::Rect([xl, pa[1]], [xr, pb[1]]))
                        .fill(theme::up().gamma_multiply(0.18))
                        .color(theme::up())
                        .label(format!("Target {} ({:+.2}%)", price(d.b.price), stats.target_pct), tp, ta),
                ];
                if let (Some(c), Some(cp)) = (pc, d.c) {
                    let (sp, sa) = inside(c[1], pa[1]);
                    let pct = stats.stop_pct.unwrap_or(0.0);
                    parts.push(
                        Part::new(Shape::Rect([xl, pa[1]], [xr, c[1]]))
                            .fill(theme::down().gamma_multiply(0.18))
                            .color(theme::down())
                            .label(format!("Stop {} ({pct:+.2}%)", price(cp.price)), sp, sa),
                    );
                }
                let side = if d.tool == Tool::LongPosition { "LONG" } else { "SHORT" };
                let rr = stats.reward_risk.map(|r| format!("  R:R {r:.2}")).unwrap_or_default();
                parts.push(seg([xl, pa[1]], [xr, pa[1]]).color(text()).label(
                    format!("{side} {}{rr}", price(d.a.price)),
                    pos2(xr as f32 - 4.0, pa[1] as f32 - 2.0),
                    Align2::RIGHT_BOTTOM,
                ));
                parts
            }
            Tool::Measure | Tool::Zoom => {
                let wash = theme::accent().gamma_multiply(0.14);
                let part = Part::new(Shape::Rect(pa, pb)).fill(wash).color(theme::accent());
                if d.tool == Tool::Zoom {
                    return vec![part];
                }
                let (delta, pct) = drawings::price_change(d.a.price, d.b.price);
                let bars = (self.index_of_ts(d.b.ts) - self.index_of_ts(d.a.ts)).round();
                let label = format!(
                    "{}{} ({pct:+.2}%)  {bars:.0} bars, {}",
                    if delta >= 0.0 { "+" } else { "" },
                    price(delta),
                    drawings::format_span(d.b.ts - d.a.ts)
                );
                let top = pos2(((pa[0] + pb[0]) / 2.0) as f32, pa[1].min(pb[1]) as f32 - 3.0);
                vec![part.label(label, top, Align2::CENTER_BOTTOM)]
            }
        }
    }

    fn paint_drawings(
        &mut self,
        ui: &Ui,
        rect: Rect,
        v: &Viewport,
        s: &PriceScale,
        hover: Option<Pos2>,
        resp: &egui::Response,
    ) {
        let p = ui.painter_at(rect);
        let pointer_anchor = |this: &Self, pos: Pos2| -> Anchor {
            let idx = v.index_at((pos.x - rect.left()) as f64);
            let mut price = s.price_at((pos.y - rect.top()) as f64);
            if this.magnet
                && let Some(b) = this.bars.get(idx.floor().max(0.0) as usize)
            {
                price = drawings::magnet(price, [b.open, b.high, b.low, b.close]);
            }
            Anchor { ts: this.ts_of_index(idx), price }
        };

        // Placement: collect the tool's points, then store it (or zoom, for the zoom tool).
        if let (Some(tool), true, Some(pos)) = (self.draw.tool, resp.clicked(), resp.interact_pointer_pos()) {
            let a = pointer_anchor(self, pos);
            self.draw.pending.push(a);
            if self.draw.pending.len() >= tool.points() {
                let pts = std::mem::take(&mut self.draw.pending);
                if tool == Tool::Zoom {
                    self.zoom_to(&pts[0], &pts[1]);
                } else if let Some(d) = Drawing::from_points(tool, &pts) {
                    self.drawings.push(d);
                    self.hide_drawings = false;
                }
                if !self.keep_drawing || tool == Tool::Zoom {
                    self.draw.tool = None;
                }
            }
        } else if self.draw.tool.is_none() && resp.clicked() && !self.lock_drawings && !self.hide_drawings {
            // Selection by proximity.
            self.draw.selected = resp.interact_pointer_pos().and_then(|pos| {
                self.drawings.iter().enumerate().find_map(|(i, d)| {
                    let near = self
                        .drawing_parts(d, rect, v, s)
                        .iter()
                        .any(|part| drawings::distance(&part.shape, [pos.x as f64, pos.y as f64]) < 6.0);
                    near.then_some(i)
                })
            });
        }

        let mut all: Vec<(Drawing, bool)> = if self.hide_drawings {
            Vec::new()
        } else {
            self.drawings.iter().cloned().enumerate().map(|(i, d)| (d, self.draw.selected == Some(i))).collect()
        };
        if let (Some(tool), false, Some(h)) = (self.draw.tool, self.draw.pending.is_empty(), hover) {
            let mut pts = self.draw.pending.clone();
            pts.push(pointer_anchor(self, h));
            all.extend(Drawing::from_points(tool, &pts).map(|d| (d, true)));
        }
        for (d, selected) in all {
            for part in self.drawing_parts(&d, rect, v, s) {
                let color = if selected { amber() } else { part.color.unwrap_or_else(theme::cyan) };
                let stroke = Stroke::new(if selected { 2.0 } else { 1.3 }, color);
                match part.shape {
                    Shape::Segment(a, b) => {
                        let pts = [pos2(a[0] as f32, a[1] as f32), pos2(b[0] as f32, b[1] as f32)];
                        if part.dashed {
                            p.extend(egui::Shape::dashed_line(&pts, stroke, 5.0, 4.0));
                        } else {
                            p.line_segment(pts, stroke);
                        }
                    }
                    Shape::HLine(y) => {
                        p.hline(rect.x_range(), y as f32, stroke);
                    }
                    Shape::VLine(x) => {
                        p.vline(x as f32, rect.y_range(), stroke);
                    }
                    Shape::Rect(a, b) => {
                        let r = Rect::from_two_pos(pos2(a[0] as f32, a[1] as f32), pos2(b[0] as f32, b[1] as f32));
                        if let Some(f) = part.fill {
                            p.rect_filled(r, 0.0, f);
                        }
                        p.rect_stroke(r, 0.0, Stroke::new(stroke.width * 0.8, color), StrokeKind::Middle);
                    }
                }
                if let Some((label, pos, align)) = part.label {
                    p.text(pos, align, label, mono(10.0), color);
                }
            }
            if selected {
                // Handles on the placed points.
                for a in [Some(d.a), (d.tool.points() > 1).then_some(d.b), d.c].into_iter().flatten() {
                    let q =
                        pos2(rect.left() + v.x_of(self.index_of_ts(a.ts)) as f32, rect.top() + s.y_of(a.price) as f32);
                    p.circle_filled(q, 3.5, amber());
                }
            }
        }
    }

    fn paint_markers(&mut self, ui: &Ui, rect: Rect, v: &Viewport, s: &PriceScale, hover: Option<Pos2>) {
        if self.markers.is_empty() || self.bars.is_empty() {
            return;
        }
        let p = ui.painter_at(rect);
        let len = self.bars.len();
        let (lo, hi) = v.visible_range(len);
        // Stack multiple trades on the same bar.
        let mut stack: std::collections::HashMap<(usize, bool), u32> = std::collections::HashMap::new();
        let r = (v.bar_px as f32 * 0.45).clamp(4.0, 7.0);
        let mut best: Option<(f32, usize, Pos2)> = None;
        for (mi, t) in self.markers.iter().enumerate() {
            let Some(ts) = t.tx_date.and_hms_opt(12, 0, 0).map(|d| d.and_utc().timestamp()) else { continue };
            let idx = self.index_of_ts(ts).floor();
            if idx < lo as f64 || idx >= hi as f64 || idx < 0.0 || idx >= len as f64 {
                continue;
            }
            let i = idx as usize;
            let bar = self.bars[i];
            let sell = t.tx_type.is_sell();
            let n = stack.entry((i, sell)).or_insert(0);
            let x = rect.left() + v.x_center(i) as f32;
            let off = 6.0 + r + *n as f32 * (2.0 * r + 1.0);
            let y = if sell {
                rect.top() + s.y_of(bar.high) as f32 - off
            } else {
                rect.top() + s.y_of(bar.low) as f32 + off
            };
            *n += 1;
            let c = Pos2::new(x, y);
            let col = theme::party_color(t.party);
            p.circle_filled(c, r, col);
            if r >= 5.0 {
                let letter = match t.tx_type {
                    TxType::Purchase => "B",
                    TxType::Sale | TxType::SalePartial => "S",
                    _ => "E",
                };
                p.text(c, Align2::CENTER_CENTER, letter, mono(r * 1.6), Color32::WHITE);
            } else {
                p.circle_stroke(c, r, Stroke::new(1.0, Color32::WHITE));
            }
            if let Some(h) = hover {
                let d = h.distance(c);
                if d <= r + 3.0 && best.is_none_or(|b| d < b.0) {
                    best = Some((d, mi, c));
                }
            }
        }
        if let Some((_, mi, c)) = best {
            self.hover_marker = Some(mi);
            let t = &self.markers[mi];
            hover_card(ui, c, t);
        }
    }
}

fn dashed_h(p: &egui::Painter, x: egui::Rangef, y: f32, s: Stroke) {
    let mut a = x.min;
    while a < x.max {
        p.hline(a..=(a + 4.0).min(x.max), y, s);
        a += 7.0;
    }
}

fn dashed_v(p: &egui::Painter, x: f32, y: egui::Rangef, s: Stroke) {
    let mut a = y.min;
    while a < y.max {
        p.vline(x, a..=(a + 4.0).min(y.max), s);
        a += 7.0;
    }
}

/// Hover card for a congressional trade marker, including the disclosure lag.
fn hover_card(ui: &Ui, at: Pos2, t: &Trade) {
    let lines = [
        (
            format!("{} ({}-{})", t.politician, t.party.code(), t.state.as_deref().unwrap_or("??")),
            theme::party_color(t.party),
        ),
        (format!("{} · {}", t.chamber.as_str(), t.owner.as_deref().unwrap_or("Self")), muted()),
        (
            format!(
                "{} {}  {}",
                match t.tx_type {
                    TxType::Purchase => "BUY",
                    TxType::Sale => "SELL",
                    TxType::SalePartial => "SELL (partial)",
                    TxType::Exchange => "EXCHANGE",
                    TxType::Other => "OTHER",
                },
                t.ticker.as_deref().unwrap_or("-"),
                t.amount.map(|a| a.to_string()).unwrap_or_else(|| "amount n/a".into())
            ),
            if t.tx_type.is_buy() { theme::up() } else { theme::down() },
        ),
        (format!("Traded  {}", t.tx_date), text()),
        (format!("Filed   {}", t.filed_date), text()),
        (
            format!("Disclosure lag {} days", t.disclosure_lag_days()),
            if t.disclosure_lag_days() > 45 { theme::down() } else { amber() },
        ),
    ];
    let w = lines.iter().map(|(l, _)| l.len()).max().unwrap_or(10) as f32 * 7.2 + 14.0;
    let h = lines.len() as f32 * 14.0 + 8.0;
    let screen = ui.ctx().content_rect();
    let mut min = at + vec2(12.0, 12.0);
    if min.x + w > screen.right() {
        min.x = at.x - w - 12.0;
    }
    if min.y + h > screen.bottom() {
        min.y = at.y - h - 12.0;
    }
    let card = Rect::from_min_size(min, vec2(w, h));
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("congress-card"));
    let p = ui.ctx().layer_painter(layer);
    p.rect_filled(card, 0.0, Color32::from_rgb(16, 16, 16));
    p.rect_stroke(card, 0.0, Stroke::new(1.0, amber_dim()), StrokeKind::Inside);
    for (k, (l, c)) in lines.iter().enumerate() {
        p.text(card.left_top() + vec2(7.0, 4.0 + k as f32 * 14.0), Align2::LEFT_TOP, l, mono(11.5), *c);
    }
}
