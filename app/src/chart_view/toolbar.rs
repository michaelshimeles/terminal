//! Collapsible drawing toolbar on the chart's left edge: tool groups with flyouts, measure
//! and zoom, and drawing toggles (magnet, keep drawing, lock, hide, remove all). Closed, it
//! is a slim tab on the plot's edge. Icons are simple line glyphs drawn here.

use std::f32::consts::{FRAC_PI_4, PI};

use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Popup, Pos2, Rect, RectAlign, Response, Sense, Shape, Stroke,
    StrokeKind, Ui, UiBuilder, pos2, vec2,
};
use tr_chart::drawings::Tool;

use super::ChartState;
use crate::anim;
use crate::theme::{self, muted, text, ui_font};
use crate::widgets;

/// Width of the open toolbar strip.
pub const STRIP_W: f32 = 36.0;
const BUTTON: f32 = 30.0;

/// Tool groups, each one button that shows the last tool picked from its flyout.
pub const GROUPS: [(&str, &[Tool]); 4] = [
    (
        "Lines",
        &[
            Tool::Trendline,
            Tool::Ray,
            Tool::ExtendedLine,
            Tool::Horizontal,
            Tool::HorizontalRay,
            Tool::Vertical,
            Tool::CrossLine,
            Tool::ParallelChannel,
        ],
    ),
    ("Fibonacci", &[Tool::Fib, Tool::FibExtension]),
    ("Projection", &[Tool::LongPosition, Tool::ShortPosition]),
    ("Shapes", &[Tool::Rectangle]),
];

#[derive(Clone, Copy)]
enum Item {
    Cursor,
    Group(usize),
    Single(Tool),
    Magnet,
    KeepDrawing,
    Lock,
    Hide,
    RemoveAll,
    Separator,
}

const ITEMS: [Item; 15] = [
    Item::Cursor,
    Item::Group(0),
    Item::Group(1),
    Item::Group(2),
    Item::Group(3),
    Item::Separator,
    Item::Single(Tool::Measure),
    Item::Single(Tool::Zoom),
    Item::Separator,
    Item::Magnet,
    Item::KeepDrawing,
    Item::Lock,
    Item::Hide,
    Item::Separator,
    Item::RemoveAll,
];

/// Default hotkey of a tool, for tooltips and menus (the keymap is rebindable in Settings).
pub fn shortcut_text(tool: Tool) -> Option<String> {
    let key = match tool {
        Tool::Trendline => "T",
        Tool::Horizontal => "H",
        Tool::HorizontalRay => "J",
        Tool::Vertical => "V",
        Tool::CrossLine => "C",
        Tool::Fib => "F",
        _ => return None,
    };
    Some(format!("{}{key}", if cfg!(target_os = "macos") { "⌥" } else { "Alt+" }))
}

fn tooltip(tool: Tool) -> String {
    match shortcut_text(tool) {
        Some(k) => format!("{} ({k})", tool.label()),
        None => tool.label().to_string(),
    }
}

/// The group a tool belongs to, and its position there.
pub fn group_of(tool: Tool) -> Option<(usize, usize)> {
    GROUPS.iter().enumerate().find_map(|(g, (_, tools))| tools.iter().position(|t| *t == tool).map(|k| (g, k)))
}

impl ChartState {
    /// The open toolbar in `strip`. Returns whether it was clicked (focus request).
    pub(super) fn toolbar_strip(&mut self, ui: &mut Ui, strip: Rect) -> bool {
        ui.painter().vline(strip.right() - 0.5, strip.y_range(), Stroke::new(1.0, theme::border()));
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(strip.shrink2(vec2(3.0, 4.0)))
                .layout(Layout::top_down(Align::Center))
                .id_salt((self.id, "toolbar")),
        );
        child.spacing_mut().item_spacing.y = 2.0;
        let mut clicked = false;
        egui::ScrollArea::vertical()
            .id_salt((self.id, "toolbar-scroll"))
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                for item in ITEMS {
                    clicked |= self.toolbar_item(ui, item);
                }
            });
        clicked
    }

    fn toolbar_item(&mut self, ui: &mut Ui, item: Item) -> bool {
        let toggle = |ui: &mut Ui, glyph: Glyph, on: &mut bool, tip_on: &str, tip_off: &str| {
            let resp = button(ui, glyph, if *on { tip_on } else { tip_off }, *on, false);
            if resp.clicked() {
                *on = !*on;
            }
            resp.clicked()
        };
        match item {
            Item::Separator => {
                let (r, _) = ui.allocate_exact_size(vec2(BUTTON, 7.0), Sense::hover());
                ui.painter().hline(
                    r.shrink2(vec2(4.0, 0.0)).x_range(),
                    r.center().y,
                    Stroke::new(1.0, theme::border()),
                );
                false
            }
            Item::Cursor => {
                let resp = button(ui, Glyph::Cursor, "Cursor (Esc)", self.draw.tool.is_none(), false);
                if resp.clicked() {
                    self.draw.tool = None;
                    self.draw.pending.clear();
                }
                resp.clicked()
            }
            Item::Group(g) => {
                let (name, tools) = GROUPS[g];
                let active = self.draw.tool.filter(|t| tools.contains(t));
                let shown = active.unwrap_or(tools[self.tool_picks[g].min(tools.len() - 1)]);
                let flyout = tools.len() > 1;
                let resp = button(ui, Glyph::Tool(shown), &tooltip(shown), active.is_some(), flyout);
                let mut clicked = resp.clicked();
                if clicked {
                    self.toggle_tool(shown);
                }
                if flyout {
                    // The corner chevron (or a right-click) opens the group's flyout.
                    let chev =
                        Rect::from_min_max(resp.rect.right_bottom() - vec2(11.0, 11.0), resp.rect.right_bottom());
                    let chev_resp = ui.interact(chev, resp.id.with("flyout"), Sense::click());
                    let open_by_click = Popup::menu(&chev_resp)
                        .align(RectAlign::RIGHT_START)
                        .gap(6.0)
                        .show(|ui| self.flyout(ui, g, name, active));
                    if open_by_click.is_none() {
                        Popup::context_menu(&resp).align(RectAlign::RIGHT_START).gap(6.0).show(|ui| {
                            self.flyout(ui, g, name, active);
                        });
                    }
                    clicked |= chev_resp.clicked();
                }
                clicked
            }
            Item::Single(tool) => {
                let active = self.draw.tool == Some(tool);
                let resp = button(ui, Glyph::Tool(tool), &tooltip(tool), active, false);
                if resp.clicked() {
                    self.toggle_tool(tool);
                }
                resp.clicked()
            }
            Item::Magnet => {
                toggle(ui, Glyph::Magnet, &mut self.magnet, "Magnet on: snap to OHLC (⌥M)", "Magnet: snap to OHLC (⌥M)")
            }
            Item::KeepDrawing => toggle(
                ui,
                Glyph::KeepDrawing,
                &mut self.keep_drawing,
                "Keep drawing on: the tool stays active",
                "Keep drawing: keep the tool after placing",
            ),
            Item::Lock => {
                let clicked = toggle(
                    ui,
                    Glyph::Lock(self.lock_drawings),
                    &mut self.lock_drawings,
                    "Unlock drawings",
                    "Lock all drawings",
                );
                if clicked {
                    self.draw.selected = None;
                }
                clicked
            }
            Item::Hide => {
                let clicked = toggle(
                    ui,
                    Glyph::Hide(self.hide_drawings),
                    &mut self.hide_drawings,
                    "Show all drawings",
                    "Hide all drawings",
                );
                if clicked {
                    self.draw.selected = None;
                }
                clicked
            }
            Item::RemoveAll => {
                let n = self.drawings.len();
                let tip = if n == 0 { "No drawings to remove".to_string() } else { format!("Remove all {n} drawings") };
                let resp = button(ui, Glyph::Trash, &tip, false, false);
                if resp.clicked() {
                    self.drawings.clear();
                    self.draw.selected = None;
                }
                resp.clicked()
            }
        }
    }

    fn toggle_tool(&mut self, tool: Tool) {
        if self.draw.tool == Some(tool) {
            self.draw.tool = None;
            self.draw.pending.clear();
        } else {
            self.start_tool(tool);
        }
    }

    fn flyout(&mut self, ui: &mut Ui, g: usize, name: &str, active: Option<Tool>) {
        ui.set_min_width(220.0);
        ui.label(egui::RichText::new(name.to_uppercase()).font(ui_font(10.5)).color(muted()));
        for &tool in GROUPS[g].1 {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
            widgets::hover_wash(ui, r, &resp, active == Some(tool));
            let color = if active == Some(tool) { theme::accent() } else { text() };
            let p = ui.painter();
            draw_glyph(
                p,
                Rect::from_center_size(pos2(r.left() + 14.0, r.center().y), vec2(18.0, 18.0)),
                Glyph::Tool(tool),
                color,
            );
            p.text(pos2(r.left() + 30.0, r.center().y), Align2::LEFT_CENTER, tool.label(), ui_font(12.5), color);
            if let Some(k) = shortcut_text(tool) {
                p.text(pos2(r.right() - 6.0, r.center().y), Align2::RIGHT_CENTER, k, ui_font(11.5), muted());
            }
            if resp.clicked() {
                self.start_tool(tool);
                ui.close();
            }
        }
    }

    /// The tab that opens or closes the toolbar, on the plot's left edge.
    pub(super) fn toolbar_tab(&mut self, ui: &mut Ui, plot: Rect) {
        let r = Rect::from_min_size(pos2(plot.left(), plot.bottom() - 46.0), vec2(13.0, 32.0));
        let resp = ui.interact(r, ui.id().with((self.id, "toolbar-tab")), Sense::click());
        let t = anim::spring_bool(ui.ctx(), resp.id.with("hover"), resp.hovered());
        let p = ui.painter();
        let radius = CornerRadius { nw: 0, sw: 0, ne: 6, se: 6 };
        p.rect_filled(r, radius, theme::accent().gamma_multiply(0.55 + 0.45 * t));
        let (c, d) = (r.center(), if self.toolbar_open { -1.0 } else { 1.0 });
        let s = Stroke::new(1.6, Color32::WHITE);
        p.line_segment([c + vec2(-2.0 * d, -4.0), c + vec2(2.0 * d, 0.0)], s);
        p.line_segment([c + vec2(2.0 * d, 0.0), c + vec2(-2.0 * d, 4.0)], s);
        let alt = if cfg!(target_os = "macos") { "⌥" } else { "Alt+" };
        let tip = if self.toolbar_open { "Hide drawing toolbar" } else { "Show drawing toolbar" };
        if resp.on_hover_text(format!("{tip} ({alt}D)")).clicked() {
            self.toolbar_open = !self.toolbar_open;
        }
    }
}

/// Toolbar button with an animated hover/active wash and an optional flyout chevron.
fn button(ui: &mut Ui, glyph: Glyph, tip: &str, active: bool, flyout: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(BUTTON, BUTTON), Sense::click());
    let t = anim::spring_bool(ui.ctx(), resp.id.with("hover"), resp.hovered() || active);
    let p = ui.painter();
    if t > 0.01 {
        let base = if active { theme::accent_wash() } else { theme::control_hover() };
        p.rect_filled(rect, theme::CONTROL_RADIUS, base.gamma_multiply(t));
    }
    let color = if active { theme::accent() } else { text().gamma_multiply(0.85) };
    draw_glyph(p, rect.shrink(6.0), glyph, color);
    if flyout {
        let (x, y) = (rect.right() - 3.0, rect.bottom() - 3.0);
        p.add(Shape::convex_polygon(vec![pos2(x, y - 4.0), pos2(x, y), pos2(x - 4.0, y)], muted(), Stroke::NONE));
    }
    resp.on_hover_text(tip)
}

#[derive(Clone, Copy)]
enum Glyph {
    Cursor,
    Tool(Tool),
    Magnet,
    KeepDrawing,
    Lock(bool),
    Hide(bool),
    Trash,
}

/// Line glyph centred in `r`, on a 16-unit grid like [`widgets::draw_icon`].
fn draw_glyph(p: &egui::Painter, r: Rect, glyph: Glyph, color: Color32) {
    let s = Stroke::new(1.3, color);
    let c = r.center();
    let u = r.height().min(r.width()) / 16.0;
    let pt = |x: f32, y: f32| pos2(c.x + x * u, c.y + y * u);
    let line = |a: (f32, f32), b: (f32, f32)| p.line_segment([pt(a.0, a.1), pt(b.0, b.1)], s);
    let poly = |pts: &[(f32, f32)]| p.add(Shape::line(pts.iter().map(|q| pt(q.0, q.1)).collect(), s));
    let handle = |x: f32, y: f32| {
        p.circle_filled(pt(x, y), 2.0 * u, theme::card());
        p.circle_stroke(pt(x, y), 2.0 * u, s);
    };
    let arc = |cx: f32, cy: f32, rad: f32, from: f32, to: f32| -> Vec<Pos2> {
        (0..=16)
            .map(|i| from + (to - from) * i as f32 / 16.0)
            .map(|a| pt(cx + rad * a.cos(), cy + rad * a.sin()))
            .collect()
    };
    let arrow = |x: f32, y: f32, dx: f32, dy: f32| {
        // Open arrowhead at (x, y) pointing along (dx, dy).
        let (len, a) = (3.5, dy.atan2(dx));
        for side in [-0.5f32, 0.5] {
            let b = a + PI + side;
            line((x, y), (x + len * b.cos(), y + len * b.sin()));
        }
    };
    match glyph {
        Glyph::Cursor => {
            line((-7.0, 0.0), (-2.5, 0.0));
            line((2.5, 0.0), (7.0, 0.0));
            line((0.0, -7.0), (0.0, -2.5));
            line((0.0, 2.5), (0.0, 7.0));
        }
        Glyph::Tool(tool) => match tool {
            Tool::Trendline => {
                line((-5.0, 5.0), (5.0, -5.0));
                handle(-5.0, 5.0);
                handle(5.0, -5.0);
            }
            Tool::Ray => {
                line((-5.0, 5.0), (7.0, -7.0));
                arrow(7.0, -7.0, 1.0, -1.0);
                handle(-5.0, 5.0);
            }
            Tool::ExtendedLine => {
                line((-7.5, 7.5), (7.5, -7.5));
                handle(-2.5, 2.5);
                handle(2.5, -2.5);
            }
            Tool::Horizontal => {
                line((-7.5, 0.0), (7.5, 0.0));
                handle(0.0, 0.0);
            }
            Tool::HorizontalRay => {
                line((-5.0, 0.0), (7.5, 0.0));
                arrow(7.5, 0.0, 1.0, 0.0);
                handle(-5.0, 0.0);
            }
            Tool::Vertical => {
                line((0.0, -7.5), (0.0, 7.5));
                handle(0.0, 0.0);
            }
            Tool::CrossLine => {
                line((-7.5, 0.0), (7.5, 0.0));
                line((0.0, -7.5), (0.0, 7.5));
                handle(0.0, 0.0);
            }
            Tool::ParallelChannel => {
                line((-7.0, 1.5), (2.0, -6.5));
                line((-2.0, 6.5), (7.0, -1.5));
                handle(-7.0, 1.5);
                handle(2.0, -6.5);
                handle(7.0, -1.5);
            }
            Tool::Fib => {
                for y in [-6.0, -2.0, 2.0, 6.0] {
                    line((-3.0, y), (7.5, y));
                }
                handle(-6.0, -6.0);
                handle(-6.0, 6.0);
            }
            Tool::FibExtension => {
                poly(&[(-7.0, 6.0), (-4.0, -2.0), (-1.0, 3.0)]);
                for y in [-6.5, -3.0, 0.5] {
                    line((1.5, y), (7.5, y));
                }
                handle(-7.0, 6.0);
                handle(-4.0, -2.0);
                handle(-1.0, 3.0);
            }
            Tool::Rectangle => {
                p.rect_stroke(Rect::from_min_max(pt(-6.5, -4.5), pt(6.5, 4.5)), 0.0, s, StrokeKind::Middle);
                handle(-6.5, -4.5);
                handle(6.5, 4.5);
            }
            Tool::LongPosition | Tool::ShortPosition => {
                let up = tool == Tool::LongPosition;
                let e = if up { 2.0 } else { -2.0 };
                p.rect_stroke(Rect::from_min_max(pt(-6.5, -6.5), pt(6.5, 6.5)), 1.5, s, StrokeKind::Middle);
                line((-6.5, e), (6.5, e));
                let (from, to) = if up { (-0.5, -5.0) } else { (0.5, 5.0) };
                line((0.0, from), (0.0, to));
                arrow(0.0, to, 0.0, to - from);
            }
            Tool::Measure => {
                // A ruler at 45°, with ticks along one edge.
                let rot = |x: f32, y: f32| {
                    let (sa, ca) = (-FRAC_PI_4).sin_cos();
                    (x * ca - y * sa, x * sa + y * ca)
                };
                let corners: Vec<(f32, f32)> =
                    [(-8.0, -2.8), (8.0, -2.8), (8.0, 2.8), (-8.0, 2.8), (-8.0, -2.8)].map(|q| rot(q.0, q.1)).to_vec();
                poly(&corners);
                for (k, x) in [-5.0f32, -2.5, 0.0, 2.5, 5.0].into_iter().enumerate() {
                    let len = if k % 2 == 0 { 2.2 } else { 1.2 };
                    line(rot(x, -2.8), rot(x, -2.8 + len));
                }
            }
            Tool::Zoom => {
                p.circle_stroke(pt(-1.5, -1.5), 5.0 * u, s);
                line((2.2, 2.2), (6.5, 6.5));
                line((-4.0, -1.5), (1.0, -1.5));
                line((-1.5, -4.0), (-1.5, 1.0));
            }
        },
        Glyph::Magnet => {
            // A U opening upwards; screen y grows down, so angles PI→0 sweep the bottom.
            let mut pts = vec![pt(-5.0, -6.0)];
            pts.extend(arc(0.0, 0.0, 5.0, PI, 0.0));
            pts.push(pt(5.0, -6.0));
            p.add(Shape::line(pts, s));
            line((-6.8, -3.2), (-3.2, -3.2));
            line((3.2, -3.2), (6.8, -3.2));
        }
        Glyph::KeepDrawing => {
            // A pencil and a small loop: the tool keeps going.
            poly(&[(-6.5, 6.5), (-6.0, 3.0), (2.5, -5.5), (5.5, -2.5), (-3.0, 6.0), (-6.5, 6.5)]);
            line((0.5, -3.5), (3.5, -0.5));
            p.add(Shape::line(arc(4.5, 4.5, 2.5, -PI * 0.5, PI), s));
            arrow(4.5 - 2.5, 4.5, 0.0, -1.0);
        }
        Glyph::Lock(locked) => {
            p.rect_stroke(Rect::from_min_max(pt(-5.5, -0.5), pt(5.5, 6.5)), 1.5, s, StrokeKind::Middle);
            // Open: the shackle is lifted and its left leg is free.
            let top = if locked { -3.5 } else { -6.0 };
            p.add(Shape::line(arc(0.0, top, 3.5, PI, 2.0 * PI), s));
            line((3.5, top), (3.5, -0.5));
            line((-3.5, top), (-3.5, if locked { -0.5 } else { top + 1.5 }));
            p.circle_filled(pt(0.0, 3.0), 1.1 * u, color);
        }
        Glyph::Hide(hidden) => {
            let lid = |sign: f32| -> Vec<Pos2> {
                (0..=16)
                    .map(|i| {
                        let x = -7.5 + 15.0 * i as f32 / 16.0;
                        pt(x, sign * 4.5 * (1.0 - (x / 7.5).powi(2)))
                    })
                    .collect()
            };
            p.add(Shape::line(lid(-1.0), s));
            p.add(Shape::line(lid(1.0), s));
            p.circle_stroke(c, 2.2 * u, s);
            if hidden {
                line((-6.5, 6.5), (6.5, -6.5));
            }
        }
        Glyph::Trash => {
            line((-6.5, -4.5), (6.5, -4.5));
            poly(&[(-2.0, -4.5), (-2.0, -6.5), (2.0, -6.5), (2.0, -4.5)]);
            poly(&[(-5.0, -4.5), (-4.0, 6.5), (4.0, 6.5), (5.0, -4.5)]);
            line((-1.3, -1.5), (-1.3, 3.5));
            line((1.3, -1.5), (1.3, 3.5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_drawing_tool_is_reachable_from_the_toolbar() {
        let all = [
            Tool::Trendline,
            Tool::Horizontal,
            Tool::Vertical,
            Tool::Fib,
            Tool::Ray,
            Tool::ExtendedLine,
            Tool::HorizontalRay,
            Tool::CrossLine,
            Tool::ParallelChannel,
            Tool::FibExtension,
            Tool::Rectangle,
            Tool::LongPosition,
            Tool::ShortPosition,
            Tool::Measure,
            Tool::Zoom,
        ];
        let singles: Vec<Tool> =
            ITEMS.iter().filter_map(|i| if let Item::Single(t) = i { Some(*t) } else { None }).collect();
        for t in all {
            assert!(group_of(t).is_some() || singles.contains(&t), "{t:?} has no toolbar button");
        }
        assert_eq!(group_of(Tool::HorizontalRay), Some((0, 4)));
        assert_eq!(group_of(Tool::Measure), None);
        assert_eq!(tooltip(Tool::Ray), "Ray");
        assert!(tooltip(Tool::Fib).ends_with("F)"));
    }

    /// Click at `at` over three frames (hover, press, release), running `ui` each frame.
    fn click(chart: &mut ChartState, at: Pos2, mut ui_fn: impl FnMut(&mut ChartState, &mut Ui)) {
        let ctx = egui::Context::default();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        for events in [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)]] {
            let input = egui::RawInput {
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| ui_fn(chart, ui));
            out.textures_delta.clear();
        }
    }

    #[test]
    fn the_tab_opens_and_closes_the_toolbar() {
        let mut chart = ChartState::new(1, "AAPL", tr_core::Timeframe::D1);
        let plot = Rect::from_min_max(pos2(100.0, 0.0), pos2(500.0, 300.0));
        assert!(!chart.toolbar_open);
        click(&mut chart, pos2(106.0, 270.0), |c, ui| c.toolbar_tab(ui, plot));
        assert!(chart.toolbar_open);
        click(&mut chart, pos2(106.0, 270.0), |c, ui| c.toolbar_tab(ui, plot));
        assert!(!chart.toolbar_open);
    }

    #[test]
    fn group_buttons_start_and_stop_the_last_picked_tool() {
        let mut chart = ChartState::new(1, "AAPL", tr_core::Timeframe::D1);
        let strip = Rect::from_min_size(Pos2::ZERO, vec2(STRIP_W, 600.0));
        // Second button from the top: the lines group (4 px inset, 30 px buttons, 2 px gaps).
        let lines = pos2(STRIP_W / 2.0, 4.0 + BUTTON + 2.0 + BUTTON / 2.0);
        click(&mut chart, lines, |c, ui| {
            c.toolbar_strip(ui, strip);
        });
        assert_eq!(chart.draw.tool, Some(Tool::Trendline));
        click(&mut chart, lines, |c, ui| {
            c.toolbar_strip(ui, strip);
        });
        assert_eq!(chart.draw.tool, None);
        // A hotkey picks the horizontal ray; the group button now starts that tool.
        chart.start_tool(Tool::HorizontalRay);
        assert!(chart.cancel());
        click(&mut chart, lines, |c, ui| {
            c.toolbar_strip(ui, strip);
        });
        assert_eq!(chart.draw.tool, Some(Tool::HorizontalRay));
        // The cursor button above it cancels the tool.
        click(&mut chart, pos2(STRIP_W / 2.0, 4.0 + BUTTON / 2.0), |c, ui| {
            c.toolbar_strip(ui, strip);
        });
        assert_eq!(chart.draw.tool, None);
    }

    #[test]
    fn glyphs_paint_without_panicking() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(18.0, 18.0));
            let glyphs = [Glyph::Cursor, Glyph::Magnet, Glyph::KeepDrawing, Glyph::Trash]
                .into_iter()
                .chain([true, false].into_iter().flat_map(|b| [Glyph::Lock(b), Glyph::Hide(b)]))
                .chain(GROUPS.iter().flat_map(|(_, ts)| ts.iter().map(|t| Glyph::Tool(*t))))
                .chain([Glyph::Tool(Tool::Measure), Glyph::Tool(Tool::Zoom)]);
            for g in glyphs {
                draw_glyph(ui.painter(), r, g, Color32::WHITE);
            }
        });
        out.textures_delta.clear();
    }
}
