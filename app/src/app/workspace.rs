//! The workspace: pane bookkeeping, the docked layout (tab groups, dividers, drag and
//! drop), pop-out windows, colour link groups and saved layouts.

use std::collections::HashMap;
use std::path::PathBuf;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Id, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, ViewportBuilder,
    ViewportId, pos2, vec2,
};

use super::{BookPane, NewsPane, Pane, PaneKind, TerminalApp};
use crate::anim;
use crate::congress_view::{Action, FeedFilter};
use crate::dock::{Dir, Node, PaneId, Zone};
use crate::layout::Layout;
use crate::layouts::{LayoutDoc, Link, SavedPane, WindowGeometry};
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets::{self, Icon};
use crate::worker::{Request, Worker};

/// Panels offered by the + menus: label and command word.
pub const PANELS: [(&str, &str); 14] = [
    ("Chart", "CHART"),
    ("Watchlist", "WL"),
    ("News", "NEWS"),
    ("Order Book", "BOOK"),
    ("Options", "OMON"),
    ("Portfolio", "PORT"),
    ("World Markets", "WEI"),
    ("Yield Curve", "GC"),
    ("Congress Feed", "CONG"),
    ("Politicians", "POL"),
    ("Alerts", "ALRT"),
    ("Security Search", "SECF"),
    ("Settings", "PREFS"),
    ("Help", "HELP"),
];

/// Length of a dock transition (panes added, closed, moved, or a layout switch).
const DOCK_SECS: f32 = 0.42;

/// Eases the dock between arrangements: when panes are added, closed or moved, groups
/// glide from where they were to their new rectangles and new groups rise and fade in
/// (staggered, for a layout switch). Window and divider resizes follow immediately.
#[derive(Default)]
pub(super) struct DockMotion {
    /// The arrangement last frame, to notice structural changes.
    shape: Vec<ShapePart>,
    /// Rectangles shown last frame, by group key.
    shown: HashMap<PaneId, Rect>,
    /// Where each surviving group started the current transition.
    from: HashMap<PaneId, Rect>,
    /// Groups new in the current transition, by key, with their stagger rank.
    new: HashMap<PaneId, usize>,
    start: Option<f64>,
}

/// The dock's arrangement without its proportions: split directions, child order and the
/// panes in each group. A layout switch or a swap changes it; dragging a divider does not.
#[derive(Debug, PartialEq)]
enum ShapePart {
    Split(Dir),
    End,
    Group(Vec<PaneId>),
}

fn shape_of(node: &Node, out: &mut Vec<ShapePart>) {
    match node {
        Node::Tabs { panes, .. } => {
            let mut p = panes.clone();
            p.sort_unstable();
            out.push(ShapePart::Group(p));
        }
        Node::Split { dir, children, .. } => {
            out.push(ShapePart::Split(*dir));
            children.iter().for_each(|c| shape_of(c, out));
            out.push(ShapePart::End);
        }
    }
}

impl DockMotion {
    /// A group keeps its key while it keeps its oldest pane.
    fn key(g: &crate::dock::Group) -> PaneId {
        g.panes.iter().copied().min().unwrap_or(0)
    }

    /// Move `groups` (laid out from `dock`) to where they are shown this frame; returns
    /// how far each has appeared (1 = fully).
    fn apply(&mut self, ctx: &egui::Context, dock: &Node, groups: &mut [crate::dock::Group]) -> Vec<f32> {
        let mut shape = Vec::new();
        shape_of(dock, &mut shape);
        let now = ctx.input(|i| i.time);
        if shape != self.shape {
            // The first arrangement shows as is.
            if !self.shape.is_empty() {
                self.from.clear();
                self.new.clear();
                for g in groups.iter() {
                    let k = Self::key(g);
                    match self.shown.get(&k) {
                        Some(r) => {
                            self.from.insert(k, *r);
                        }
                        None => {
                            let rank = self.new.len();
                            self.new.insert(k, rank);
                        }
                    }
                }
                self.start = Some(now);
            }
            self.shape = shape;
        }
        let t = self.start.map_or(1.0, |s| anim::since(ctx, now - s, DOCK_SECS));
        if t >= 1.0 {
            self.start = None;
        }
        self.shown.clear();
        groups
            .iter_mut()
            .map(|g| {
                let k = Self::key(g);
                let mut appear = 1.0;
                if t < 1.0 {
                    if let Some(from) = self.from.get(&k) {
                        let e = anim::ease_out(anim::span(t, 0.0, 0.8));
                        g.rect = Rect::from_min_max(from.min.lerp(g.rect.min, e), from.max.lerp(g.rect.max, e));
                    } else if let Some(&rank) = self.new.get(&k) {
                        let delay = 0.1 + (rank as f32 * 0.08).min(0.3);
                        appear = anim::ease_out(anim::span(t, delay, delay + 0.6));
                        g.rect = g.rect.translate(vec2(0.0, 14.0 * (1.0 - appear)));
                    }
                }
                self.shown.insert(k, g.rect);
                appear
            })
            .collect()
    }
}

/// Saved-layout operations from the sidebar, settings and command line.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutOp {
    Switch(String),
    /// A fresh default workspace under a new name.
    New(Option<String>),
    SaveAs(String),
    Rename(String, String),
    Duplicate(String, Option<String>),
    Delete(String),
    Export(String, Option<PathBuf>),
    Import(PathBuf),
    OpenFolder,
}

/// Structural edits requested while drawing tab groups, applied afterwards.
enum GroupCmd {
    Activate(PaneId),
    Close(PaneId),
    CloseOthers(PaneId),
    PopOut(PaneId),
    Add { target: PaneId, zone: Zone, spec: &'static str },
    Link(PaneId, Option<Link>),
    StartRename(PaneId),
    Rename(PaneId, String),
}

/// Pointer travel (points) before a press on a tab becomes a drag.
const DRAG_THRESHOLD: f32 = 5.0;

/// Where a dragged pane would land.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTarget {
    /// Dock path to insert relative to (`[]` = the whole window).
    pub path: Vec<usize>,
    pub zone: Zone,
    /// Highlighted landing area.
    pub preview: Rect,
    /// Index of the hovered group, if any (window-edge docking has none).
    pub group: Option<usize>,
    /// Dropping here would change nothing.
    pub noop: bool,
}

impl DropTarget {
    pub fn label(&self) -> &'static str {
        match (self.group.is_some(), self.zone) {
            (_, Zone::Center) => "Add as Tab",
            (true, Zone::Left) => "Split Left",
            (true, Zone::Right) => "Split Right",
            (true, Zone::Top) => "Split Above",
            (true, Zone::Bottom) => "Split Below",
            (false, Zone::Left) => "Dock at Left Edge",
            (false, Zone::Right) => "Dock at Right Edge",
            (false, Zone::Top) => "Dock at Top Edge",
            (false, Zone::Bottom) => "Dock at Bottom Edge",
        }
    }
}

/// The five compass targets drawn at the centre of a hovered pane.
fn compass(rect: Rect) -> [(Zone, Rect); 5] {
    let c = rect.center();
    let s = (rect.width().min(rect.height()) / 7.0).clamp(18.0, 30.0);
    let d = s + 6.0;
    let at = |dx: f32, dy: f32| Rect::from_center_size(c + vec2(dx, dy), vec2(s, s));
    [
        (Zone::Center, at(0.0, 0.0)),
        (Zone::Left, at(-d, 0.0)),
        (Zone::Right, at(d, 0.0)),
        (Zone::Top, at(0.0, -d)),
        (Zone::Bottom, at(0.0, d)),
    ]
}

/// Resolve the drop target under `pos`: a window edge, a compass button, or the nearest
/// zone of the pane under the pointer.
pub fn drop_target(area: Rect, groups: &[crate::dock::Group], pos: egui::Pos2, dragged: PaneId) -> Option<DropTarget> {
    let edge = 18.0;
    if area.expand(edge).contains(pos) {
        let z = if pos.x < area.left() + edge {
            Some(Zone::Left)
        } else if pos.x > area.right() - edge {
            Some(Zone::Right)
        } else if pos.y < area.top() + edge {
            Some(Zone::Top)
        } else if pos.y > area.bottom() - edge {
            Some(Zone::Bottom)
        } else {
            None
        };
        if let Some(z) = z {
            let alone = groups.len() == 1 && groups[0].panes == [dragged];
            return Some(DropTarget { path: Vec::new(), zone: z, preview: z.preview(area), group: None, noop: alone });
        }
    }
    let (gi, g) = groups.iter().enumerate().find(|(_, g)| g.rect.contains(pos))?;
    let zone = compass(g.rect)
        .into_iter()
        .find(|(_, r)| r.expand(3.0).contains(pos))
        .map(|(z, _)| z)
        .unwrap_or_else(|| Zone::at(g.rect, pos));
    // A pane alone in its group can't be docked relative to itself.
    let noop = g.panes == [dragged];
    Some(DropTarget { path: g.path.clone(), zone, preview: zone.preview(g.rect), group: Some(gi), noop })
}

fn draw_compass(layer: &egui::Painter, rect: Rect, current: Zone, noop: bool) {
    let accent = theme::accent();
    for (zone, r) in compass(rect) {
        let hot = zone == current && !noop;
        layer.rect_filled(r, 5.0, if hot { accent } else { theme::card().gamma_multiply(0.9) });
        layer.rect_stroke(
            r,
            5.0,
            Stroke::new(1.0, accent.gamma_multiply(if hot { 1.0 } else { 0.6 })),
            StrokeKind::Inside,
        );
        // A glyph showing the half (or whole) the pane would take.
        let inner = r.shrink(5.0);
        let fill = zone.preview(inner);
        let fg = if hot { Color32::WHITE } else { accent };
        layer.rect_stroke(inner, 2.0, Stroke::new(1.0, fg.gamma_multiply(0.7)), StrokeKind::Inside);
        layer.rect_filled(fill, 2.0, fg.gamma_multiply(if zone == Zone::Center { 0.35 } else { 0.8 }));
    }
}

fn tab_h() -> f32 {
    dp(26.0)
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(p),
    }
}

impl TerminalApp {
    // ---- bookkeeping ---------------------------------------------------------------------

    pub(super) fn idx(&self, id: PaneId) -> Option<usize> {
        self.panes.iter().position(|p| p.id == id)
    }

    pub(super) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    pub(super) fn focused_is_chart(&self) -> bool {
        self.idx(self.focus).is_some_and(|i| matches!(self.panes[i].kind, PaneKind::Chart(_)))
    }

    /// The focused pane if it is docked, else the first visible docked pane.
    pub(super) fn focused_visible(&self) -> Option<PaneId> {
        if self.dock.contains(self.focus) {
            return Some(self.focus);
        }
        self.dock.visible().first().copied()
    }

    /// The security the user is looking at: the focused pane's, its link group's, or the
    /// first chart's.
    pub(super) fn focused_symbol(&self) -> Option<String> {
        let f = self.idx(self.focus).map(|i| &self.panes[i]);
        if let Some(s) = f.and_then(|p| p.kind.symbol()) {
            return Some(s.to_string());
        }
        if let Some(link) = f.and_then(|p| p.link)
            && let Some(s) = self.panes.iter().filter(|p| p.link == Some(link)).find_map(|p| p.kind.symbol())
        {
            return Some(s.to_string());
        }
        self.panes.iter().find_map(|p| match &p.kind {
            PaneKind::Chart(c) => Some(c.symbol.clone()),
            _ => None,
        })
    }

    /// The focused security if it has listed options (US equity, ETF, index), else AAPL.
    pub(super) fn optionable_symbol(&self) -> String {
        use tr_core::AssetClass as C;
        self.focused_symbol()
            .filter(|s| matches!(tr_data::security(s).class, C::Equity | C::Etf | C::Index))
            .unwrap_or_else(|| "AAPL US Equity".into())
    }

    /// Indices of panes on screen: active tabs of the dock plus popped-out windows.
    pub(super) fn visible_panes(&self) -> Vec<usize> {
        let mut ids = self.dock.visible();
        ids.extend(self.panes.iter().filter(|p| p.window.is_some()).map(|p| p.id));
        ids.into_iter().filter_map(|id| self.idx(id)).collect()
    }

    fn focus_order(&self) -> Vec<PaneId> {
        let mut ids = self.dock.visible();
        ids.extend(self.panes.iter().filter(|p| p.window.is_some()).map(|p| p.id));
        ids
    }

    pub(super) fn cycle_focus(&mut self, dir: i32) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let at = order.iter().position(|&p| p == self.focus).unwrap_or(0) as i32;
        let n = order.len() as i32;
        self.focus = order[(at + dir).rem_euclid(n) as usize];
    }

    fn ensure_focus(&mut self) {
        if self.idx(self.focus).is_none() {
            self.focus = self.focus_order().first().copied().unwrap_or(0);
        }
    }

    /// Create a pane (not yet docked), kicking off any loads it needs.
    pub(super) fn new_pane(&mut self, kind: PaneKind) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        let kind = self.init_kind(id, kind);
        self.panes.push(Pane { id, kind, link: None, title: None, window: None });
        id
    }

    fn init_kind(&mut self, id: PaneId, kind: PaneKind) -> PaneKind {
        match kind {
            PaneKind::Chart(mut c) => {
                c.id = id;
                c.load(&self.worker);
                PaneKind::Chart(c)
            }
            PaneKind::Ticker { symbol, .. } => {
                self.worker.send(Request::TickerTrades { pane: id, symbol: symbol.clone() });
                PaneKind::Ticker { symbol, trades: Vec::new(), loading: true }
            }
            PaneKind::Profile { query, .. } => {
                self.worker.send(Request::Profile { pane: id, query: query.clone() });
                PaneKind::Profile { query, data: None, loading: true }
            }
            k => k,
        }
    }

    /// Dock pane `id` relative to `target` (or the window when there is none) and focus it.
    pub(super) fn dock_pane(&mut self, id: PaneId, target: Option<PaneId>, zone: Zone) {
        match target.filter(|t| self.dock.contains(*t)) {
            Some(t) => {
                self.dock.insert(t, zone, id);
            }
            None => self.dock.insert_at(&[], zone, id),
        }
        self.focus = id;
        self.preset = None;
    }

    /// Swap a pane's content for `kind`, keeping its place, link and window. A fresh id
    /// keeps late worker responses for the old content from landing on the new one.
    pub(super) fn replace_pane(&mut self, id: PaneId, kind: PaneKind) -> PaneId {
        let new = self.next_id;
        self.next_id += 1;
        let kind = self.init_kind(new, kind);
        let Some(i) = self.idx(id) else { return new };
        let old = std::mem::replace(&mut self.panes[i], Pane { id: new, kind, link: None, title: None, window: None });
        self.panes[i].link = old.link;
        self.panes[i].window = old.window;
        self.dock.replace(id, new);
        self.worker.send(Request::Forget(id));
        if self.focus == id {
            self.focus = new;
        }
        new
    }

    pub(super) fn close_pane(&mut self, id: PaneId) {
        let Some(i) = self.idx(id) else { return };
        let p = self.panes.remove(i);
        self.dock.remove(id);
        self.worker.send(Request::Forget(id));
        self.preset = None;
        if self.renaming_tab.as_ref().is_some_and(|(r, _)| *r == id) {
            self.renaming_tab = None;
        }
        self.ensure_focus();
        self.status = format!("Closed {}", p.display_title());
    }

    pub(super) fn pop_out(&mut self, id: PaneId) {
        let Some(i) = self.idx(id) else { return };
        if self.panes[i].window.is_some() {
            return;
        }
        let n = self.panes.iter().filter(|p| p.window.is_some()).count() as f32;
        self.panes[i].window = Some(WindowGeometry { x: 120.0 + 30.0 * n, y: 120.0 + 30.0 * n, w: 760.0, h: 520.0 });
        self.dock.remove(id);
        self.preset = None;
        self.status = format!("Popped out {} (close its window to dock it back)", self.panes[i].display_title());
    }

    fn dock_back(&mut self, id: PaneId) {
        let Some(i) = self.idx(id) else { return };
        self.panes[i].window = None;
        let target = self.focused_visible();
        self.dock_pane(id, target, Zone::Right);
    }

    /// Show a pane: activate its tab (popped windows are always visible).
    fn reveal(&mut self, id: PaneId) {
        self.dock.activate(id);
    }

    // ---- symbols and link groups ---------------------------------------------------------

    /// Point a symbol-driven pane at `symbol`. Returns whether it follows symbols.
    pub(super) fn set_pane_symbol(&mut self, i: usize, symbol: &str) -> bool {
        let id = self.panes[i].id;
        let key = tr_data::security(symbol).key();
        match &mut self.panes[i].kind {
            PaneKind::Chart(c) => {
                if c.symbol != key {
                    c.set_symbol(&key, &self.worker);
                }
            }
            PaneKind::Ticker { symbol: s, trades, loading } => {
                if *s != key {
                    *s = key.clone();
                    trades.clear();
                    *loading = true;
                    self.worker.send(Request::TickerTrades { pane: id, symbol: key });
                }
            }
            PaneKind::News(n) => {
                if n.symbol.as_deref() != Some(key.as_str()) {
                    **n = NewsPane::new(Some(key));
                }
            }
            PaneKind::OrderBook(b) => {
                if b.symbol != key {
                    **b = BookPane::new(&key, b.depth, 0.0);
                }
            }
            PaneKind::Options(o) => {
                if o.symbol != key {
                    **o = super::OptionsPane::new(&key, o.strikes, o.show_greeks);
                }
            }
            _ => return false,
        }
        true
    }

    /// Load `symbol` into every symbol-driven pane of `link` (except `except`).
    pub(super) fn broadcast(&mut self, link: Link, symbol: &str, except: Option<PaneId>) {
        for i in 0..self.panes.len() {
            if self.panes[i].link == Some(link) && Some(self.panes[i].id) != except {
                self.set_pane_symbol(i, symbol);
            }
        }
        self.status = format!("{} ({} link)", tr_data::security(symbol).key(), link.label());
    }

    pub(super) fn set_link(&mut self, id: PaneId, link: Option<Link>) {
        let Some(i) = self.idx(id) else { return };
        self.panes[i].link = link;
        // Joining a group adopts the group's security.
        if let Some(l) = link
            && self.panes[i].kind.follows_symbol()
            && let Some(s) = self
                .panes
                .iter()
                .filter(|p| p.link == Some(l) && p.id != id)
                .find_map(|p| p.kind.symbol().map(str::to_string))
        {
            self.set_pane_symbol(i, &s);
        }
        self.status = match link {
            Some(l) => format!("{} linked to the {} group", self.panes[i].display_title(), l.label()),
            None => format!("{} unlinked", self.panes[i].display_title()),
        };
    }

    /// Load a security: into the focused pane when it follows symbols (and its link group),
    /// else into the focused pane's link group, else into a chart.
    pub(super) fn open_security(&mut self, symbol: &str) {
        let key = tr_data::security(symbol).key();
        if let Some(i) = self.idx(self.focus) {
            let link = self.panes[i].link;
            if self.panes[i].kind.follows_symbol() {
                self.set_pane_symbol(i, &key);
                match link {
                    Some(l) => self.broadcast(l, &key, Some(self.focus)),
                    None => self.status = key,
                }
                return;
            }
            if let Some(l) = link
                && self.panes.iter().any(|p| p.link == Some(l) && p.kind.follows_symbol())
            {
                self.broadcast(l, &key, None);
                return;
            }
        }
        let t = self.chart_target();
        if let Some(i) = self.idx(t) {
            self.set_pane_symbol(i, &key);
            self.focus = t;
            match self.panes[i].link {
                Some(l) => self.broadcast(l, &key, Some(t)),
                None => self.status = key,
            }
        }
    }

    // ---- choosing where things open ------------------------------------------------------

    /// Where to show a panel: an existing pane of the same type, else (to add a tab beside)
    /// a visible panel whose type is shown twice, else a visible non-chart panel that wasn't
    /// just opened. `None` means split in a new pane beside the focused one.
    fn target_pane(&self, same: impl Fn(&PaneKind) -> bool) -> Option<PaneId> {
        let visible = self.dock.visible();
        let kind_of = |id: PaneId| self.idx(id).map(|i| &self.panes[i].kind);
        if let Some(id) = visible.iter().copied().find(|&id| kind_of(id).is_some_and(&same)) {
            return Some(id);
        }
        if let Some(p) = self.panes.iter().find(|p| same(&p.kind)) {
            return Some(p.id);
        }
        let disc = |id: PaneId| kind_of(id).map(std::mem::discriminant);
        let non_chart: Vec<PaneId> = visible
            .iter()
            .copied()
            .filter(|&id| !matches!(kind_of(id), Some(PaneKind::Chart(_) | PaneKind::Settings)))
            .collect();
        if let Some(&id) = non_chart.iter().rev().find(|&&id| visible.iter().any(|&j| j != id && disc(j) == disc(id))) {
            return Some(id);
        }
        non_chart.iter().copied().find(|&id| Some(id) != self.last_opened && id != self.focus)
    }

    /// Show a panel: reuse a pane of the same type (replacing its content), else add it as a
    /// tab beside a suitable panel, else split it in next to the focused pane. Panels the
    /// user arranged are never discarded.
    pub(super) fn open_panel(&mut self, kind: PaneKind) {
        let disc = std::mem::discriminant(&kind);
        let same = self.panes.iter().any(|p| std::mem::discriminant(&p.kind) == disc);
        let id = match self.target_pane(|k| std::mem::discriminant(k) == disc) {
            Some(t) if same => {
                let id = self.replace_pane(t, kind);
                self.reveal(id);
                id
            }
            Some(t) => {
                let id = self.new_pane(kind);
                self.dock_pane(id, Some(t), Zone::Center);
                id
            }
            None => {
                let target = self.focused_visible();
                let id = self.new_pane(kind);
                self.dock_pane(id, target, Zone::Right);
                id
            }
        };
        self.focus = id;
        self.last_opened = Some(id);
    }

    /// The chart commands act on: the focused one, a visible one, any (revealed), or a new
    /// chart beside the focused pane.
    fn chart_target(&mut self) -> PaneId {
        if self.focused_is_chart() {
            return self.focus;
        }
        let is_chart = |p: &Pane| matches!(p.kind, PaneKind::Chart(_));
        let visible = self.dock.visible();
        if let Some(id) = visible.iter().copied().find(|&id| self.idx(id).is_some_and(|i| is_chart(&self.panes[i]))) {
            return id;
        }
        if let Some(id) = self.panes.iter().find(|p| is_chart(p)).map(|p| p.id) {
            self.reveal(id);
            return id;
        }
        let kind = self.panel_from_spec("CHART", None).expect("chart panel");
        let target = self.focused_visible();
        let id = self.new_pane(kind);
        self.dock_pane(id, target, Zone::Right);
        id
    }

    /// The existing chart [`Self::chart_target`] would pick, without revealing or creating one.
    pub(super) fn peek_chart(&self) -> Option<&crate::chart_view::ChartState> {
        let chart = |id: PaneId| {
            self.idx(id).and_then(|i| match &self.panes[i].kind {
                PaneKind::Chart(c) => Some(&**c),
                _ => None,
            })
        };
        chart(self.focus)
            .or_else(|| self.dock.visible().into_iter().find_map(chart))
            .or_else(|| self.panes.iter().find_map(|p| chart(p.id)))
    }

    pub(super) fn with_chart(&mut self, f: impl FnOnce(&mut crate::chart_view::ChartState, &Worker)) {
        let id = self.chart_target();
        self.focus = id;
        if let Some(i) = self.idx(id)
            && let PaneKind::Chart(c) = &mut self.panes[i].kind
        {
            f(c, &self.worker);
        }
    }

    /// A new panel from a command word (`CHART`, `NEWS AAPL`, `BOOK BTCUSD`…).
    pub(super) fn panel_from_spec(&self, spec: &str, symbol: Option<&str>) -> Option<PaneKind> {
        let words: Vec<&str> = spec.split_whitespace().collect();
        let head = words.first().map(|w| w.to_ascii_uppercase()).unwrap_or_else(|| "CHART".into());
        let given = (words.len() > 1)
            .then(|| tr_core::Security::resolve_tokens(&words[1..]).map(|(s, _)| s.key()))
            .flatten()
            .or(symbol.map(|s| tr_data::security(s).key()));
        let current = given.clone().or_else(|| self.focused_symbol());
        let d = &self.prefs.panels;
        Some(match head.as_str() {
            "CHART" | "GP" | "G" | "GRAPH" => PaneKind::from_settings(crate::layouts::chart_settings(
                current.as_deref().unwrap_or("AAPL US Equity"),
                d,
            )),
            "WL" | "WATCHLIST" | "W" => PaneKind::Watchlist,
            "NEWS" | "N" | "CN" | "TOP" => PaneKind::News(Box::new(NewsPane::new(current))),
            "BOOK" | "OB" | "DEPTH" | "DOM" => {
                // Default to a market with a public book.
                let sym = given
                    .or(current.filter(|s| tr_data::security(s).class == tr_core::AssetClass::Crypto))
                    .unwrap_or_else(|| "BTCUSD Curncy".into());
                PaneKind::OrderBook(Box::new(BookPane::new(&sym, d.book_depth, 0.0)))
            }
            "OMON" | "OPT" | "OPTIONS" | "CHAIN" => {
                let sym = given.unwrap_or_else(|| self.optionable_symbol());
                PaneKind::Options(Box::new(super::OptionsPane::new(&sym, 12, true)))
            }
            "PORT" | "PORTFOLIO" | "ACCT" | "POS" | "POSITIONS" | "ORD" | "ORDERS" | "ACTV" | "ACTIVITY" => {
                let view = match head.as_str() {
                    "ACCT" => crate::command::PortView::Accounts,
                    "POS" | "POSITIONS" => crate::command::PortView::Positions,
                    "ORD" | "ORDERS" => crate::command::PortView::Orders,
                    "ACTV" | "ACTIVITY" => crate::command::PortView::Activity,
                    _ => crate::command::PortView::Summary,
                };
                PaneKind::Portfolio(Box::new(super::broker::PortfolioPane::new(view)))
            }
            "WEI" | "MARKETS" => PaneKind::Wei,
            "GC" | "CURVE" | "YC" => PaneKind::Curve,
            "CONG" | "FEED" | "CONGRESS" => PaneKind::Feed { filter: FeedFilter::default() },
            "POL" | "POLITICIANS" => PaneKind::Politicians,
            "ALRT" | "ALERTS" => PaneKind::Alerts,
            "SECF" | "SEARCH" => PaneKind::Search { query: words[1..].join(" ") },
            "PREFS" | "SETTINGS" | "PDF" => PaneKind::Settings,
            "HELP" => PaneKind::Help,
            _ => return None,
        })
    }

    /// Replace the dock with a preset grid, keeping every pane (extras become tabs) and
    /// adding panels to fill empty slots.
    pub(super) fn apply_preset(&mut self, l: Layout) {
        let mut ids = self.dock.visible();
        for id in self.dock.panes() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let fillers = ["CONG", "WL", "WEI", "GC", "NEWS", "BOOK"];
        let mut k = 0;
        while ids.len() < l.slots() {
            let has = |w: &str| {
                ids.iter().any(|&id| {
                    self.idx(id).is_some_and(|i| {
                        self.panel_from_spec(w, None).map(|x| std::mem::discriminant(&x))
                            == Some(std::mem::discriminant(&self.panes[i].kind))
                    })
                })
            };
            let spec = fillers.iter().find(|w| !has(w)).copied().unwrap_or("CHART");
            let spec = if spec == "CHART" {
                ["CHART BTCUSD CRNCY", "CHART SPX INDEX", "CHART EURUSD CRNCY"][k % 3]
            } else {
                spec
            };
            k += 1;
            let kind = self.panel_from_spec(spec, None).expect("known panel");
            ids.push(self.new_pane(kind));
        }
        self.dock = Node::preset(l, &ids);
        self.preset = Some(l);
        self.ensure_focus();
    }

    // ---- saved layouts -------------------------------------------------------------------

    pub(super) fn current_doc(&self) -> LayoutDoc {
        LayoutDoc {
            version: crate::layouts::FORMAT_VERSION,
            name: self.layout_name.clone(),
            dock: self.dock.clone(),
            panes: self
                .panes
                .iter()
                .map(|p| SavedPane {
                    id: p.id,
                    settings: p.kind.settings(),
                    link: p.link,
                    title: p.title.clone(),
                    window: p.window,
                })
                .collect(),
            focus: Some(self.focus),
            sidebar_side: Some(self.prefs.appearance.sidebar_side),
            theme: self.layout_theme.then(|| self.prefs.appearance.theme.clone()),
        }
    }

    /// Replace the workspace with `doc`. Pane ids are reassigned so responses for the old
    /// panes can never reach the new ones.
    pub(super) fn apply_doc(&mut self, mut doc: LayoutDoc) {
        doc.repair();
        for p in std::mem::take(&mut self.panes) {
            self.worker.send(Request::Forget(p.id));
        }
        let mut map = HashMap::new();
        for sp in doc.panes {
            let id = self.new_pane(PaneKind::from_settings(sp.settings));
            map.insert(sp.id, id);
            if let Some(p) = self.pane_mut(id) {
                p.link = sp.link;
                p.title = sp.title;
                p.window = sp.window;
            }
        }
        doc.dock.remap(&|id| map.get(&id).copied());
        self.layout_theme = doc.theme.is_some();
        if let Some(name) = doc.theme.as_deref()
            && !name.eq_ignore_ascii_case(&self.prefs.appearance.theme)
            && (name.eq_ignore_ascii_case(crate::themes::SYSTEM) || crate::themes::find(&self.themes, name).is_some())
        {
            self.select_theme(name);
        }
        if let Some(side) = doc.sidebar_side
            && side != self.prefs.appearance.sidebar_side
        {
            self.prefs.appearance.sidebar_side = side;
            self.save_prefs();
        }
        self.dock = doc.dock;
        self.focus = doc.focus.and_then(|f| map.get(&f).copied()).unwrap_or(0);
        self.ensure_focus();
        self.layout_name = doc.name;
        self.preset = None;
        self.drag = None;
        self.renaming_tab = None;
        self.last_saved = self.current_doc().to_json();
    }

    pub(super) fn save_layout(&mut self) {
        let doc = self.current_doc();
        match self.store.save(&doc) {
            Ok(_) => self.last_saved = doc.to_json(),
            Err(e) => self.status = format!("Could not save layout: {e:#}"),
        }
        self.refresh_layout_names();
    }

    pub(super) fn refresh_layout_names(&mut self) {
        self.layout_names = self.store.list();
    }

    pub(super) fn switch_layout(&mut self, name: &str) {
        if name.eq_ignore_ascii_case(&self.layout_name) {
            self.status = format!("Already on layout {name:?}");
            return;
        }
        self.save_layout();
        match self.store.load(name) {
            Ok(doc) => {
                self.apply_doc(doc);
                self.save_settings();
                self.status = format!("Layout {:?}", self.layout_name);
            }
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    /// `LAYOUT …` from the command line.
    pub(super) fn layout_command(&mut self, arg: &str) {
        let words: Vec<&str> = arg.split_whitespace().collect();
        let rest = |n: usize| words.get(n..).map(|w| w.join(" ")).unwrap_or_default();
        let Some(first) = words.first().map(|w| w.to_ascii_uppercase()) else {
            self.apply(crate::command::Function::Layouts);
            return;
        };
        if words.len() == 1
            && let Some(l) = Layout::parse(&first)
        {
            self.apply_preset(l);
            return;
        }
        let op = match first.as_str() {
            "SAVE" if words.len() == 1 => {
                self.save_layout();
                self.status = format!("Saved layout {:?}", self.layout_name);
                return;
            }
            "SAVE" | "SAVEAS" | "AS" => LayoutOp::SaveAs(rest(1)),
            "NEW" => LayoutOp::New((words.len() > 1).then(|| rest(1))),
            "RENAME" | "MV" => LayoutOp::Rename(self.layout_name.clone(), rest(1)),
            "DUP" | "DUPLICATE" | "COPY" => {
                LayoutOp::Duplicate(self.layout_name.clone(), (words.len() > 1).then(|| rest(1)))
            }
            "DEL" | "DELETE" | "RM" => {
                LayoutOp::Delete(if words.len() > 1 { rest(1) } else { self.layout_name.clone() })
            }
            "EXPORT" => LayoutOp::Export(self.layout_name.clone(), (words.len() > 1).then(|| expand_home(&rest(1)))),
            "IMPORT" if words.len() > 1 => LayoutOp::Import(expand_home(&rest(1))),
            "FOLDER" | "DIR" => LayoutOp::OpenFolder,
            "THEME" => {
                self.layout_theme =
                    !matches!(words.get(1).map(|w| w.to_ascii_uppercase()).as_deref(), Some("OFF" | "NO"));
                self.save_layout();
                self.status = if self.layout_theme {
                    format!("Layout {:?} now switches to theme {:?}", self.layout_name, self.prefs.appearance.theme)
                } else {
                    format!("Layout {:?} no longer changes the theme", self.layout_name)
                };
                return;
            }
            _ => LayoutOp::Switch(arg.trim().to_string()),
        };
        self.layout_op(op);
    }

    pub(super) fn layout_op(&mut self, op: LayoutOp) {
        let result: anyhow::Result<String> = (|| match op {
            LayoutOp::Switch(name) => {
                self.switch_layout(&name);
                Ok(self.status.clone())
            }
            LayoutOp::New(name) => {
                self.save_layout();
                let name = self.store.unique_name(name.as_deref().unwrap_or("Layout").trim());
                self.apply_doc(LayoutDoc::default_workspace(&name, &self.prefs.panels));
                self.save_layout();
                self.save_settings();
                Ok(format!("New layout {name:?}"))
            }
            LayoutOp::SaveAs(name) => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    anyhow::bail!("LAYOUT SAVEAS needs a name");
                }
                if self.store.exists(&name) && !name.eq_ignore_ascii_case(&self.layout_name) {
                    anyhow::bail!("a layout named {name:?} already exists (LAYOUT DEL it first)");
                }
                self.save_layout();
                self.layout_name = name.clone();
                self.save_layout();
                self.save_settings();
                Ok(format!("Saved as {name:?}"))
            }
            LayoutOp::Rename(old, new) => {
                if old.eq_ignore_ascii_case(&self.layout_name) {
                    self.save_layout();
                }
                let doc = self.store.rename(&old, &new)?;
                if old.eq_ignore_ascii_case(&self.layout_name) {
                    self.layout_name = doc.name.clone();
                    self.save_settings();
                }
                self.refresh_layout_names();
                Ok(format!("Renamed {old:?} to {:?}", doc.name))
            }
            LayoutOp::Duplicate(name, new) => {
                if name.eq_ignore_ascii_case(&self.layout_name) {
                    self.save_layout();
                }
                let doc = self.store.duplicate(&name, new.as_deref())?;
                self.refresh_layout_names();
                Ok(format!("Duplicated {name:?} as {:?}", doc.name))
            }
            LayoutOp::Delete(name) => {
                self.store.delete(&name)?;
                self.refresh_layout_names();
                if name.eq_ignore_ascii_case(&self.layout_name) {
                    match self.layout_names.first().cloned() {
                        Some(next) => {
                            let doc = self.store.load(&next)?;
                            self.apply_doc(doc);
                        }
                        None => {
                            self.apply_doc(LayoutDoc::default_workspace("Default", &self.prefs.panels));
                            self.save_layout();
                        }
                    }
                    self.save_settings();
                }
                Ok(format!("Deleted layout {name:?}"))
            }
            LayoutOp::Export(name, path) => {
                if name.eq_ignore_ascii_case(&self.layout_name) {
                    self.save_layout();
                }
                let path = path.unwrap_or_else(|| {
                    let downloads = expand_home("~/Downloads");
                    let dir = if downloads.is_dir() { downloads } else { self.store.dir.join("exports") };
                    dir.join(format!("{}.json", crate::layouts::Store::file_stem(&name)))
                });
                let to = self.store.export(&name, &path)?;
                Ok(format!("Exported {name:?} to {to}"))
            }
            LayoutOp::Import(path) => {
                let name = self.store.import(&path)?;
                self.refresh_layout_names();
                self.switch_layout(&name);
                Ok(format!("Imported {} as {name:?}", path.display()))
            }
            LayoutOp::OpenFolder => {
                crate::storage::reveal(&self.store.dir)?;
                Ok(format!("Opened {}", self.store.dir.display()))
            }
        })();
        self.status = match result {
            Ok(s) => s,
            Err(e) => format!("Layout: {e:#}"),
        };
    }

    // ---- drawing the dock ----------------------------------------------------------------

    pub(super) fn render_dock(&mut self, ui: &mut Ui, area: Rect, gap: f32) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut cmds = Vec::new();
        if self.dock.is_empty() {
            self.empty_state(ui, area, &mut cmds);
        }
        let (mut groups, dividers) = self.dock.layout(area, gap);
        let appear = self.dock_motion.apply(ui.ctx(), &self.dock, &mut groups);
        for (g, a) in groups.iter().zip(appear) {
            let opacity = ui.opacity();
            ui.multiply_opacity(a);
            self.render_group(ui, g, &mut actions, &mut cmds);
            ui.set_opacity(opacity);
        }
        for d in &dividers {
            let hit = d.rect.expand(2.0);
            let r = ui.interact(hit, Id::new(("divider", &d.path, d.index)), Sense::click_and_drag());
            let icon = match d.dir {
                Dir::Row => CursorIcon::ResizeHorizontal,
                Dir::Column => CursorIcon::ResizeVertical,
            };
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(icon);
                let c = d.rect.center();
                let (a, b) = match d.dir {
                    Dir::Row => (pos2(c.x, d.rect.top() + 6.0), pos2(c.x, d.rect.bottom() - 6.0)),
                    Dir::Column => (pos2(d.rect.left() + 6.0, c.y), pos2(d.rect.right() - 6.0, c.y)),
                };
                ui.painter().line_segment([a, b], Stroke::new(2.0, theme::accent().gamma_multiply(0.7)));
            }
            if r.dragged() {
                let delta = match d.dir {
                    Dir::Row => r.drag_delta().x,
                    Dir::Column => r.drag_delta().y,
                };
                self.dock.resize(&d.path, d.index, delta, d.span);
            }
            if r.double_clicked() {
                self.dock.equalize(&d.path);
            }
        }
        self.drag_and_drop(ui, area, &groups);
        self.apply_group_cmds(cmds);
        actions
    }

    fn empty_state(&mut self, ui: &mut Ui, area: Rect, cmds: &mut Vec<GroupCmd>) {
        let p = ui.painter();
        p.text(area.center() - vec2(0.0, 28.0), Align2::CENTER_CENTER, "No panels", ui_font(16.0), text());
        p.text(
            area.center() - vec2(0.0, 6.0),
            Align2::CENTER_CENTER,
            "Add one below, type a ticker, or switch layouts (LAYOUT <name>)",
            ui_font(12.0),
            muted(),
        );
        let r = Rect::from_center_size(area.center() + vec2(0.0, 28.0), vec2(140.0, 28.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(r));
        child.menu_button("＋ Add Panel", |ui| {
            for (label, spec) in PANELS {
                if ui.button(label).clicked() {
                    cmds.push(GroupCmd::Add { target: 0, zone: Zone::Center, spec });
                    ui.close();
                }
            }
        });
    }

    fn render_group(
        &mut self,
        ui: &mut Ui,
        g: &crate::dock::Group,
        actions: &mut Vec<Action>,
        cmds: &mut Vec<GroupCmd>,
    ) {
        let Some(active) = g.active_pane() else { return };
        let focused = active == self.focus;
        let focus_t = anim::spring_bool(ui.ctx(), Id::new(("group-focus", active)), focused);
        let inner = widgets::card(ui, g.rect, focus_t);
        let th = tab_h();
        let strip = Rect::from_min_size(inner.min, vec2(inner.width(), th));
        let r = theme::CARD_RADIUS.saturating_sub(1);
        let p = ui.painter_at(g.rect);
        p.rect_filled(strip, CornerRadius { nw: r, ne: r, sw: 0, se: 0 }, theme::header());
        p.hline(strip.x_range(), strip.bottom() - 0.5, Stroke::new(1.0, theme::border()));

        // Trailing buttons: add panel (as tab) and the group menu.
        let btn =
            |i: f32| Rect::from_min_size(pos2(strip.right() - 26.0 * i - 2.0, strip.top() + 2.0), vec2(24.0, th - 4.0));
        let tabs_right = btn(2.0).left() - 4.0;
        // A chart's interval and type controls share the tab strip when there is room,
        // which saves the chart a whole header row.
        let chart_idx = self.idx(active).filter(|&i| matches!(self.panes[i].kind, PaneKind::Chart(_)));
        let ctrl_w = if chart_idx.is_some() { crate::chart_view::ChartState::controls_width(ui) } else { 0.0 };
        let min_tabs = 120.0 * g.panes.len().clamp(1, 3) as f32;
        let controls_fit = chart_idx.is_some() && tabs_right - strip.left() - ctrl_w - 10.0 >= min_tabs;
        let controls_left = tabs_right - ctrl_w;
        let tabs_right = if controls_fit { controls_left - 10.0 } else { tabs_right };
        let press = ui.input(|i| if i.pointer.primary_pressed() { i.pointer.press_origin() } else { None });
        let mut x = strip.left() + 4.0;
        let n = g.panes.len().max(1) as f32;
        let max_w = ((tabs_right - x) / n - 2.0).clamp(54.0, 220.0);
        for (ti, &pid) in g.panes.iter().enumerate() {
            let Some(i) = self.idx(pid) else { continue };
            let pane = &self.panes[i];
            let (title, code, link) = (pane.display_title(), pane.kind.code(), pane.link);
            let is_active = ti == g.active;
            let code_g = p.layout_no_wrap(code.to_string(), mono(10.0), amber());
            let title_g = p.layout_no_wrap(title.clone(), ui_font(12.0), text());
            let want = 10.0 + link.map_or(0.0, |_| 10.0) + code_g.size().x + 6.0 + title_g.size().x + 24.0;
            let w = want.min(max_w);
            if x + w > tabs_right + 1.0 && ti > 0 {
                // Out of room: the overflow is reachable from the group menu.
                break;
            }
            let tr = Rect::from_min_size(pos2(x, strip.top() + 3.0), vec2(w, th - 3.0));
            let resp = ui.interact(tr, Id::new(("tab", pid)), Sense::click_and_drag());
            let hover = anim::spring_bool(ui.ctx(), Id::new(("tab-hover", pid)), resp.hovered());
            let tp = ui.painter_at(tr);
            if is_active {
                tp.rect_filled(tr, CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, theme::card());
                if focused {
                    tp.hline(tr.shrink2(vec2(6.0, 0.0)).x_range(), tr.top() + 1.0, Stroke::new(2.0, theme::accent()));
                }
            } else if hover > 0.01 {
                tp.rect_filled(tr.shrink(1.0), 6, theme::control().gamma_multiply(hover * 1.5));
            }
            let mut tx = tr.left() + 8.0;
            if let Some(l) = link {
                tp.circle_filled(pos2(tx + 3.0, tr.center().y), 3.5, l.color());
                tx += 10.0;
            }
            tp.galley(pos2(tx, tr.center().y - code_g.size().y / 2.0), code_g.clone(), amber());
            tx += code_g.size().x + 6.0;
            let fg = if is_active { text() } else { muted() };
            let renaming = self.renaming_tab.as_ref().is_some_and(|(r, _)| *r == pid);
            // Close button.
            let close = Rect::from_center_size(pos2(tr.right() - 11.0, tr.center().y), vec2(16.0, 16.0));
            if !renaming {
                let clip = Rect::from_min_max(pos2(tx, tr.top()), pos2(close.left() - 2.0, tr.bottom()));
                tp.with_clip_rect(clip.intersect(tr)).text(
                    pos2(tx, tr.center().y),
                    Align2::LEFT_CENTER,
                    &title,
                    ui_font(12.0),
                    fg,
                );
            }
            if is_active || resp.hovered() {
                let cr = ui.interact(close, Id::new(("tab-close", pid)), Sense::click());
                if cr.hovered() {
                    tp.rect_filled(close, 4, theme::control_hover());
                }
                let c = close.center();
                let s = Stroke::new(1.3, if cr.hovered() { text() } else { muted() });
                tp.line_segment([c - vec2(3.5, 3.5), c + vec2(3.5, 3.5)], s);
                tp.line_segment([c + vec2(-3.5, 3.5), c + vec2(3.5, -3.5)], s);
                if cr.clicked() {
                    cmds.push(GroupCmd::Close(pid));
                }
            }
            if renaming {
                let edit =
                    Rect::from_min_max(pos2(tx - 2.0, tr.top() + 3.0), pos2(close.left() - 2.0, tr.bottom() - 3.0));
                let mut child = ui.new_child(UiBuilder::new().max_rect(edit));
                if let Some((_, buf)) = &mut self.renaming_tab {
                    let te = child.add(egui::TextEdit::singleline(buf).font(ui_font(12.0)).desired_width(edit.width()));
                    te.request_focus();
                    if te.lost_focus() {
                        let commit = !child.input(|i| i.key_pressed(egui::Key::Escape));
                        cmds.push(GroupCmd::Rename(pid, if commit { buf.clone() } else { "\u{0}".into() }));
                    }
                }
            }
            if resp.clicked() {
                cmds.push(GroupCmd::Activate(pid));
            }
            if resp.middle_clicked() {
                cmds.push(GroupCmd::Close(pid));
            }
            if resp.double_clicked() {
                cmds.push(GroupCmd::StartRename(pid));
            }
            // Drags are tracked from the press itself (see `drag_and_drop`) rather than
            // egui's widget drag state, so quick trackpad gestures whose press and first
            // movement land in the same frame still pick the tab up.
            if !renaming
                && let Some(p) = press
                && tr.contains(p)
                && !close.contains(p)
            {
                self.drag = Some(super::Drag { pane: pid, origin: p, active: false });
            }
            resp.on_hover_text(format!(
                "{title}\nDrag to move or dock · double-click to rename · middle-click to close"
            ))
            .context_menu(|ui| tab_menu(ui, pid, link, cmds));
            x += w + 2.0;
        }

        // The empty part of the header strip drags the active pane too.
        if let Some(p) = press
            && self.drag.is_none()
            && strip.contains(p)
            && p.x < tabs_right
            && self.renaming_tab.is_none()
        {
            self.drag = Some(super::Drag { pane: active, origin: p, active: false });
        }
        let strip_hover = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| strip.contains(p) && p.x < tabs_right);
        if strip_hover && self.drag.is_none() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }

        // Group buttons.
        let plus = btn(2.0);
        let mut b = ui.new_child(UiBuilder::new().max_rect(plus));
        let resp = widgets::icon_button(&mut b, Icon::Plus, "Add Panel as Tab", false);
        egui::Popup::menu(&resp).show(|ui| {
            ui.label(egui::RichText::new("Add as Tab").font(ui_font(11.0)).color(muted()));
            for (label, spec) in PANELS {
                if ui.button(label).clicked() {
                    cmds.push(GroupCmd::Add { target: active, zone: Zone::Center, spec });
                    ui.close();
                }
            }
        });
        let more = btn(1.0);
        let mut b = ui.new_child(UiBuilder::new().max_rect(more));
        let resp = widgets::icon_button(&mut b, Icon::More, "Pane Options", false);
        let link = self.idx(active).and_then(|i| self.panes[i].link);
        let tabs: Vec<(PaneId, String)> =
            g.panes.iter().filter_map(|&id| self.idx(id).map(|i| (id, self.panes[i].display_title()))).collect();
        egui::Popup::menu(&resp).show(|ui| {
            if tabs.len() > 1 {
                ui.menu_button("Tabs", |ui| {
                    for (id, t) in &tabs {
                        if ui.radio(*id == active, t).clicked() {
                            cmds.push(GroupCmd::Activate(*id));
                            ui.close();
                        }
                    }
                });
            }
            tab_menu(ui, active, link, cmds);
        });

        if let Some(ci) = chart_idx {
            let worker = &self.worker;
            if let PaneKind::Chart(c) = &mut self.panes[ci].kind {
                c.controls_in_strip = controls_fit;
                if controls_fit {
                    let r = Rect::from_min_size(
                        pos2(controls_left, strip.center().y - crate::chart_view::CONTROL_H / 2.0),
                        vec2(ctrl_w, crate::chart_view::CONTROL_H),
                    );
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(r)
                            .layout(egui::Layout::left_to_right(egui::Align::Center))
                            .id_salt(("strip-controls", active)),
                    );
                    if c.controls(&mut child, worker) {
                        self.focus = active;
                    }
                }
            }
        }

        // Content.
        let content = Rect::from_min_max(pos2(inner.left(), strip.bottom()), inner.max);
        let Some(i) = self.idx(active) else { return };
        // Switching tabs fades the new content in.
        let fade = anim::ease_out(anim::progress(ui.ctx(), Id::new(("tab-fade", DockMotion::key(g))), active, 0.22));
        let opacity = ui.opacity();
        ui.multiply_opacity(fade);
        let (mut acts, clicked) = self.render_content(ui, i, content, focused);
        ui.set_opacity(opacity);
        // Clicks in a linked list panel drive its link group.
        if let Some(l) = self.panes[i].link
            && !self.panes[i].kind.follows_symbol()
            && self.panes.iter().any(|p| p.link == Some(l) && p.kind.follows_symbol())
        {
            for a in &mut acts {
                if let Action::OpenChart(s) = a {
                    *a = Action::LinkedSymbol(l, std::mem::take(s));
                }
            }
        }
        actions.extend(acts);
        let pressed_here = ui.input(|inp| {
            inp.pointer.primary_pressed() && inp.pointer.interact_pos().is_some_and(|p| g.rect.contains(p))
        });
        if (clicked || pressed_here) && self.drag.is_none_or(|d| !d.active) {
            self.focus = active;
        }
    }

    /// Pane drag and drop. A press on a tab or header starts a pending drag; past a small
    /// threshold it becomes active: a ghost of the pane follows the pointer, the hovered
    /// pane shows a five-way dock compass, and the landing area is highlighted and labelled.
    /// Release drops; Esc or releasing over nothing cancels.
    fn drag_and_drop(&mut self, ui: &mut Ui, area: Rect, groups: &[crate::dock::Group]) {
        let Some(mut drag) = self.drag else { return };
        let (pos, down, esc) =
            ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down(), i.key_pressed(egui::Key::Escape)));
        if esc {
            self.drag = None;
            return;
        }
        if !drag.active {
            // Released before moving: that was a click.
            if !down {
                self.drag = None;
                return;
            }
            if pos.is_some_and(|p| p.distance(drag.origin) > DRAG_THRESHOLD) {
                drag.active = true;
                self.drag = Some(drag);
            } else {
                return;
            }
        }
        let Some(pos) = pos else {
            if !down {
                self.drag = None;
            }
            return;
        };
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let target = drop_target(area, groups, pos, drag.pane);
        let layer = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("dock-drop")));
        let accent = theme::accent();
        // Every pane is a potential target: outline them faintly.
        for g in groups {
            layer.rect_stroke(
                g.rect.shrink(1.0),
                theme::CARD_RADIUS,
                Stroke::new(1.0, accent.gamma_multiply(0.35)),
                StrokeKind::Inside,
            );
        }
        if let Some(t) = &target {
            if let Some(g) = t.group.and_then(|i| groups.get(i)) {
                draw_compass(&layer, g.rect, t.zone, t.noop);
            }
            if !t.noop {
                let r = t.preview.shrink(4.0);
                layer.rect_filled(r, theme::CARD_RADIUS, accent.gamma_multiply(0.20));
                layer.rect_stroke(r, theme::CARD_RADIUS, Stroke::new(2.0, accent), StrokeKind::Inside);
                let label = t.label();
                let lg = layer.layout_no_wrap(label.into(), ui_font(12.5), Color32::WHITE);
                let pill = Rect::from_center_size(pos2(r.center().x, r.top() + 22.0), lg.size() + vec2(18.0, 8.0));
                layer.rect_filled(pill, 8.0, accent);
                layer.galley(pill.min + vec2(9.0, 4.0), lg, Color32::WHITE);
            }
        }
        self.draw_ghost(&layer, drag.pane, pos);
        ui.ctx().request_repaint();
        if !down {
            self.drag = None;
            if let Some(t) = target.filter(|t| !t.noop) {
                self.dock.insert_at(&t.path, t.zone, drag.pane);
                self.focus = drag.pane;
                self.preset = None;
                self.status = format!(
                    "Moved {}: {}",
                    self.idx(drag.pane).map(|i| self.panes[i].display_title()).unwrap_or_default(),
                    t.label()
                );
            }
        }
    }

    /// A translucent miniature of the dragged pane following the pointer.
    fn draw_ghost(&self, layer: &egui::Painter, pane: PaneId, pos: egui::Pos2) {
        let Some(i) = self.idx(pane) else { return };
        let p = &self.panes[i];
        let r = Rect::from_min_size(pos + vec2(22.0, 20.0), vec2(168.0, 96.0));
        layer.rect_filled(r.translate(vec2(0.0, 4.0)), theme::CARD_RADIUS, Color32::from_black_alpha(60));
        layer.rect_filled(r, theme::CARD_RADIUS, theme::card().gamma_multiply(0.92));
        layer.rect_stroke(r, theme::CARD_RADIUS, Stroke::new(1.5, theme::accent()), StrokeKind::Inside);
        let strip = Rect::from_min_size(r.min, vec2(r.width(), 24.0));
        layer.rect_filled(
            strip,
            CornerRadius { nw: theme::CARD_RADIUS, ne: theme::CARD_RADIUS, sw: 0, se: 0 },
            theme::header(),
        );
        let mut x = strip.left() + 8.0;
        if let Some(l) = p.link {
            layer.circle_filled(pos2(x + 3.0, strip.center().y), 3.5, l.color());
            x += 10.0;
        }
        let code = layer.layout_no_wrap(p.kind.code().into(), mono(10.0), amber());
        let cw = code.size().x;
        layer.galley(pos2(x, strip.center().y - code.size().y / 2.0), code, amber());
        layer.with_clip_rect(strip.shrink(2.0)).text(
            pos2(x + cw + 6.0, strip.center().y),
            Align2::LEFT_CENTER,
            p.display_title(),
            ui_font(12.0),
            text(),
        );
        // Placeholder content lines.
        for k in 0..4 {
            let y = strip.bottom() + 12.0 + k as f32 * 15.0;
            let w = [130.0, 100.0, 140.0, 80.0][k];
            layer.rect_filled(
                Rect::from_min_size(pos2(r.left() + 10.0, y), vec2(w, 6.0)),
                3.0,
                theme::muted().gamma_multiply(0.25),
            );
        }
    }

    fn apply_group_cmds(&mut self, cmds: Vec<GroupCmd>) {
        for c in cmds {
            match c {
                GroupCmd::Activate(id) => {
                    self.dock.activate(id);
                    self.focus = id;
                }
                GroupCmd::Close(id) => self.close_pane(id),
                GroupCmd::CloseOthers(id) => {
                    if let Some(path) = self.dock.find(id)
                        && let Some(Node::Tabs { panes, .. }) = self.dock.get(&path)
                    {
                        for other in panes.clone().into_iter().filter(|&p| p != id) {
                            self.close_pane(other);
                        }
                    }
                }
                GroupCmd::PopOut(id) => self.pop_out(id),
                GroupCmd::Add { target, zone, spec } => {
                    if let Some(kind) = self.panel_from_spec(spec, None) {
                        let new = self.new_pane(kind);
                        let t = (target != 0).then_some(target);
                        self.dock_pane(new, t, zone);
                    }
                }
                GroupCmd::Link(id, l) => self.set_link(id, l),
                GroupCmd::StartRename(id) => {
                    let t = self.idx(id).map(|i| self.panes[i].display_title()).unwrap_or_default();
                    self.renaming_tab = Some((id, t));
                }
                GroupCmd::Rename(id, t) => {
                    self.renaming_tab = None;
                    if t != "\u{0}"
                        && let Some(p) = self.pane_mut(id)
                    {
                        let t = t.trim();
                        p.title = (!t.is_empty() && *t != p.kind.tab_title()).then(|| t.to_string());
                    }
                }
            }
        }
    }

    // ---- pop-out windows -----------------------------------------------------------------

    /// Draw each popped-out pane in its own native window.
    pub(super) fn render_windows(&mut self, ctx: &egui::Context) -> Vec<Action> {
        let mut actions = Vec::new();
        let ids: Vec<PaneId> = self.panes.iter().filter(|p| p.window.is_some()).map(|p| p.id).collect();
        for id in ids {
            let Some(i) = self.idx(id) else { continue };
            let Some(geom) = self.panes[i].window else { continue };
            let title = self.panes[i].display_title();
            let builder = ViewportBuilder::default()
                .with_title(format!("{title} — Terminal"))
                .with_inner_size([geom.w, geom.h])
                .with_position([geom.x, geom.y])
                .with_min_inner_size([320.0, 220.0]);
            let mut dock_back = false;
            let mut new_geom = None;
            ctx.show_viewport_immediate(ViewportId::from_hash_of(("pane-window", id)), builder, |ui, _class| {
                let rect = ui.max_rect();
                ui.painter().rect_filled(rect, 0.0, theme::window());
                let bar = Rect::from_min_size(rect.min, vec2(rect.width(), dp(30.0)));
                let p = ui.painter();
                p.rect_filled(bar, 0.0, theme::header());
                p.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, theme::border()));
                let mut x = bar.left() + 10.0;
                if let Some(l) = self.panes[i].link {
                    p.circle_filled(pos2(x + 3.0, bar.center().y), 3.5, l.color());
                    x += 12.0;
                }
                let code = p.layout_no_wrap(self.panes[i].kind.code().to_string(), mono(10.5), amber());
                let cw = code.size().x;
                p.galley(pos2(x, bar.center().y - code.size().y / 2.0), code, amber());
                p.text(pos2(x + cw + 8.0, bar.center().y), Align2::LEFT_CENTER, &title, ui_font(12.5), text());
                let dock_btn =
                    Rect::from_min_size(pos2(bar.right() - 92.0, bar.top() + 3.0), vec2(84.0, bar.height() - 6.0));
                let mut b = ui.new_child(UiBuilder::new().max_rect(dock_btn));
                if b.button("Dock Pane").on_hover_text("Return this pane to the main window").clicked() {
                    tracing::debug!("pane window {id}: dock button");
                    dock_back = true;
                }
                let content =
                    Rect::from_min_max(pos2(rect.left() + 6.0, bar.bottom() + 6.0), rect.max - vec2(6.0, 6.0));
                let inner = widgets::card(ui, content, 0.0);
                let (acts, clicked) = self.render_content(ui, i, inner, self.focus == id);
                actions.extend(acts);
                if clicked || ui.input(|inp| inp.pointer.primary_pressed()) {
                    self.focus = id;
                }
                let info = ui.input(|inp| inp.viewport().clone());
                if info.close_requested() {
                    tracing::debug!("pane window {id}: close requested");
                    dock_back = true;
                }
                if let (Some(outer), Some(inner)) = (info.outer_rect, info.inner_rect) {
                    new_geom = Some(WindowGeometry {
                        x: outer.min.x.round(),
                        y: outer.min.y.round(),
                        w: inner.width().round(),
                        h: inner.height().round(),
                    });
                }
            });
            if dock_back {
                self.dock_back(id);
            } else if let Some(g) = new_geom
                && let Some(p) = self.pane_mut(id)
            {
                p.window = Some(g);
            }
        }
        actions
    }
}

/// Shared tab / pane options menu.
fn tab_menu(ui: &mut Ui, pid: PaneId, link: Option<Link>, cmds: &mut Vec<GroupCmd>) {
    ui.set_min_width(200.0);
    for (label, zone) in [("Split Right", Zone::Right), ("Split Down", Zone::Bottom)] {
        ui.menu_button(label, |ui| {
            for (name, spec) in PANELS {
                if ui.button(name).clicked() {
                    cmds.push(GroupCmd::Add { target: pid, zone, spec });
                    ui.close();
                }
            }
        });
    }
    ui.menu_button("Link Group", |ui| {
        if ui.radio(link.is_none(), "None").clicked() {
            cmds.push(GroupCmd::Link(pid, None));
            ui.close();
        }
        for l in Link::ALL {
            let label = egui::RichText::new(format!("● {}", l.label())).color(l.color());
            if ui.radio(link == Some(l), label).clicked() {
                cmds.push(GroupCmd::Link(pid, Some(l)));
                ui.close();
            }
        }
    });
    if ui.button("Rename Tab…").clicked() {
        cmds.push(GroupCmd::StartRename(pid));
        ui.close();
    }
    if ui.button("Pop Out to Window").clicked() {
        cmds.push(GroupCmd::PopOut(pid));
        ui.close();
    }
    ui.separator();
    if ui.button("Close Other Tabs").clicked() {
        cmds.push(GroupCmd::CloseOthers(pid));
        ui.close();
    }
    if ui.button("Close").clicked() {
        cmds.push(GroupCmd::Close(pid));
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::Node;

    fn groups(n: &Node) -> (Rect, Vec<crate::dock::Group>) {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 600.0));
        (area, n.layout(area, 6.0).0)
    }

    #[test]
    fn drop_targets_cover_edges_compass_and_zones() {
        let n = Node::preset(Layout::TwoCols, &[1, 2]);
        let (area, gs) = groups(&n);
        // Window edges dock at the root.
        let t = drop_target(area, &gs, pos2(5.0, 300.0), 2).unwrap();
        assert_eq!((t.path.clone(), t.zone, t.group, t.noop), (vec![], Zone::Left, None, false));
        assert_eq!(t.label(), "Dock at Left Edge");
        // The compass centre of the right pane adds a tab; its left button splits left.
        let right = gs[1].rect;
        let t = drop_target(area, &gs, right.center(), 1).unwrap();
        assert_eq!((t.zone, t.group, t.noop), (Zone::Center, Some(1), false));
        let (_, left_btn) = compass(right)[1];
        let t = drop_target(area, &gs, left_btn.center(), 1).unwrap();
        assert_eq!(t.zone, Zone::Left);
        assert_eq!(t.label(), "Split Left");
        // Away from the compass, the nearest edge wins.
        let t = drop_target(area, &gs, pos2(right.left() + 30.0, right.top() + 300.0), 1).unwrap();
        assert_eq!(t.zone, Zone::Left);
        // A lone pane over its own group is a no-op; outside every pane there is no target.
        assert!(drop_target(area, &gs, right.center(), 2).unwrap().noop);
        assert!(drop_target(area, &gs, pos2(500.0, 300.0), 1).is_none(), "the gap between panes");
        // Applying a target moves the pane.
        let mut m = n.clone();
        let t = drop_target(area, &gs, left_btn.center(), 1).unwrap();
        m.insert_at(&t.path, t.zone, 1);
        assert_eq!(m.visible(), vec![1, 2]);
        let t = drop_target(area, &gs, pos2(500.0, 595.0), 1).unwrap();
        m.insert_at(&t.path, t.zone, 1);
        assert!(matches!(&m, Node::Split { dir: Dir::Column, .. }));
    }

    #[test]
    fn dock_motion_glides_splits_and_follows_resizes() {
        let ctx = egui::Context::default();
        let mut motion = DockMotion::default();
        let mut step = |n: &Node, t: f64, width: f32| {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 600.0));
            let mut gs = n.layout(area, 6.0).0;
            let target: Vec<Rect> = gs.iter().map(|g| g.rect).collect();
            let mut appear = Vec::new();
            let input = egui::RawInput { time: Some(t), ..Default::default() };
            ctx.run_ui(input, |ui| appear = motion.apply(ui.ctx(), n, &mut gs)).textures_delta.clear();
            (gs.iter().map(|g| g.rect).collect::<Vec<_>>(), target, appear)
        };
        // The first arrangement and plain resizes show as laid out.
        let one = Node::preset(Layout::One, &[1]);
        let (shown, target, appear) = step(&one, 0.0, 1000.0);
        assert_eq!((shown, appear), (target, vec![1.0]));
        let (shown, target, _) = step(&one, 0.1, 900.0);
        assert_eq!(shown, target);

        // Splitting: the old pane starts where it was and glides; the new one rises in.
        let mut two = one.clone();
        two.insert(1, Zone::Right, 2);
        let (shown, target, appear) = step(&two, 1.0, 900.0);
        assert_eq!(shown[0].width(), 900.0, "starts at its old size");
        assert_eq!(appear[1], 0.0);
        assert!(shown[1].top() > target[1].top());
        let (shown, target, appear) = step(&two, 1.0 + DOCK_SECS as f64 * 0.5, 900.0);
        assert!(shown[0].width() < 900.0 && shown[0].width() > target[0].width());
        assert!(appear[1] > 0.0 && appear[1] < 1.0);
        let (shown, target, appear) = step(&two, 2.0, 900.0);
        assert_eq!((shown, appear), (target, vec![1.0, 1.0]));

        // Switching between layouts of the same groups glides, and so does a swap.
        let rows = Node::preset(Layout::TwoRows, &[1, 2]);
        let (shown, target, _) = step(&rows, 3.0, 900.0);
        assert_ne!(shown, target, "starts from the columns");
        let (shown, target, _) = step(&rows, 4.0, 900.0);
        assert_eq!(shown, target);
        let swapped = Node::split(Dir::Column, vec![Node::leaf(2), Node::leaf(1)]);
        let (shown, target, _) = step(&swapped, 5.0, 900.0);
        assert_ne!(shown, target, "starts from the old order");
        step(&swapped, 6.0, 900.0);

        // Dragging a divider follows immediately.
        let dragged = Node::split_with(Dir::Column, vec![Node::leaf(2), Node::leaf(1)], vec![0.3, 0.7]);
        let (shown, target, _) = step(&dragged, 6.1, 900.0);
        assert_eq!(shown, target);
    }
}
