//! Window chrome in the style of a SwiftUI macOS app: a unified toolbar holding the
//! command line, a translucent (vibrancy) sidebar, rounded pane cards and a status bar.

use std::time::Duration;

use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Id, Key, Modifiers, Rect, RichText, Sense, Shadow, Stroke, StrokeKind,
    Ui, UiBuilder, ViewportCommand, pos2, vec2,
};
use tr_data::SearchHit;
use web_time::Instant;

use super::{PaneKind, TerminalApp};
use crate::anim;
use crate::command::{self, Function};
use crate::congress_view::Action;
use crate::layout::Layout;
use crate::markets_view;
use crate::theme::{self, amber, mono, muted, text, ui_font};
use crate::widgets::{self, Icon};

/// Default toolbar height; on macOS it matches a unified `NSToolbar` so the traffic lights
/// centre in it. The live value comes from [`TerminalApp::toolbar_h`].
pub const TOOLBAR_H: f32 = if cfg!(target_os = "macos") { tr_macos::TITLEBAR_HEIGHT } else { 34.0 };
/// Default space kept clear for the traffic lights when the sidebar is collapsed.
pub const TRAFFIC_INSET: f32 = if cfg!(target_os = "macos") { tr_macos::TRAFFIC_LIGHTS_WIDTH } else { 0.0 };
pub const STATUS_H: f32 = 22.0;
pub const SIDEBAR_W: f32 = 232.0;
const ROW_H: f32 = 24.0;
const MAC: bool = cfg!(target_os = "macos");

/// What a sidebar click asks the app to do.
pub enum SidebarCmd {
    Action(Action),
    Function(Function),
    Layout(super::workspace::LayoutOp),
}

/// Make `rect` behave like a title bar: drag moves the window, double-click zooms it.
fn window_drag_region(ui: &Ui, rect: Rect, salt: &str) {
    let r = ui.interact(rect, Id::new(("drag-region", salt)), Sense::click_and_drag());
    if r.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if r.double_clicked() {
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }
}

impl TerminalApp {
    /// Toolbar height: at least the system title bar region on macOS, so nothing interactive
    /// sits under the window's native drag area (which would swallow clicks and drags).
    pub(super) fn toolbar_h(&self) -> f32 {
        match self.chrome {
            Some(c) if c.titlebar_height > 0.0 => c.titlebar_height.max(TOOLBAR_H),
            // Full screen: no title bar strip.
            Some(_) => 34.0,
            None => TOOLBAR_H,
        }
    }

    pub(super) fn traffic_inset(&self) -> f32 {
        match self.chrome {
            Some(c) if c.titlebar_height > 0.0 => c.traffic_lights_width.max(0.0),
            Some(_) => 0.0,
            None => TRAFFIC_INSET,
        }
    }

    // ---- toolbar -------------------------------------------------------------------------

    /// Unified toolbar: sidebar toggle, title, command line, layout picker, actions, clock.
    /// Returns whether the command line has focus.
    pub(super) fn toolbar(&mut self, ui: &mut Ui, rect: Rect, left_inset: f32) -> bool {
        window_drag_region(ui, rect, "toolbar");
        let p = ui.painter_at(rect);
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::border()));
        let cy = rect.center().y;
        let mut x = rect.left() + left_inset + 8.0;

        // The sidebar toggle sits on the sidebar's side, its glyph pointing at it.
        // Right-click it for the View options (sidebar side, theme, density).
        let on_right = self.prefs.appearance.sidebar_side == crate::prefs::SidebarSide::Right;
        let mut right = rect.right() - 10.0;
        let toggle_r = if on_right {
            right -= 26.0;
            let r = Rect::from_min_size(pos2(right, cy - 11.0), vec2(26.0, 22.0));
            right -= 8.0;
            r
        } else {
            let r = Rect::from_min_size(pos2(x, cy - 11.0), vec2(26.0, 22.0));
            x += 32.0;
            r
        };
        let mut tb = ui.new_child(UiBuilder::new().max_rect(toggle_r));
        let icon = if on_right { Icon::SidebarRight } else { Icon::Sidebar };
        let resp = widgets::icon_button(&mut tb, icon, "Toggle Sidebar (right-click for View options)", false);
        if resp.clicked() {
            self.sidebar_open = !self.sidebar_open;
        }
        resp.context_menu(|ui| self.view_menu(ui));

        // Data source chip (the window title itself is redundant in a compact toolbar).
        let (label, live) = self.source_chip();
        let chip_text = p.layout_no_wrap(label.into(), ui_font(11.0), muted());
        let chip = Rect::from_min_size(pos2(x, cy - 9.0), vec2(chip_text.size().x + 20.0, 18.0));
        p.rect_filled(chip, 9.0, theme::control());
        p.circle_filled(pos2(chip.left() + 8.0, cy), 3.0, if live { theme::up() } else { amber() });
        p.galley(pos2(chip.left() + 14.0, cy - chip_text.size().y / 2.0), chip_text, muted());
        ui.interact(chip, Id::new("source-chip"), Sense::hover()).on_hover_text(self.provider_label());
        x = chip.right() + 8.0;

        // Which account trades: DEMO (paper) or LIVE (real money, in red). Click to switch.
        let live_mode = !self.demo_mode();
        let (label, fill) = if live_mode { ("LIVE", theme::down()) } else { ("DEMO", theme::accent()) };
        let badge_text = p.layout_no_wrap(label.into(), mono(11.0), Color32::WHITE);
        let badge = Rect::from_min_size(pos2(x, cy - 10.0), vec2(badge_text.size().x + 18.0, 20.0));
        let resp = ui.interact(badge, Id::new("account-mode-badge"), Sense::click());
        let fill = if resp.hovered() { fill } else { fill.gamma_multiply(0.9) };
        p.rect_filled(badge, 10.0, fill);
        p.galley(badge.center() - badge_text.size() / 2.0, badge_text, Color32::WHITE);
        let tip = if live_mode {
            "LIVE: portfolio panes and orders use your real brokerage accounts. Click to switch to the demo account."
        } else {
            "DEMO: paper trading with virtual cash. Click to switch to your live brokerage accounts."
        };
        if resp.on_hover_text(tip).clicked() {
            let next = if live_mode { crate::prefs::AccountMode::Demo } else { crate::prefs::AccountMode::Live };
            self.set_account_mode(next, false);
        }
        if live_mode {
            // A red rule along the toolbar so live mode is never mistaken for the demo.
            p.hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(2.0, theme::down()));
        }
        x = badge.right() + 10.0;

        // Right-hand cluster, laid out from the right edge: one-line clock (UTC on hover).
        let now = chrono::Local::now();
        let clock = p.layout_no_wrap(now.format("%a %d %b %H:%M:%S").to_string(), mono(11.5), text());
        let cw = clock.size().x;
        let clock_r = Rect::from_min_size(pos2(right - cw, cy - clock.size().y / 2.0), clock.size());
        p.galley(clock_r.min, clock, text());
        ui.interact(clock_r, Id::new("clock"), Sense::hover())
            .on_hover_text(format!("{} UTC", chrono::Utc::now().format("%H:%M:%S")));
        right -= cw + 10.0;

        let unread = self.alerts.iter().filter(|a| !a.seen).count();
        let buttons: [(Icon, &str, bool); 3] = [
            (Icon::Gauge, "Frame Timing (F12)", self.show_perf),
            (Icon::Bell, "Alerts (F4)", unread > 0),
            (Icon::Refresh, "Refresh Congress Filings (F6)", false),
        ];
        for (icon, tip, active) in buttons {
            right -= 28.0;
            let r = Rect::from_min_size(pos2(right, cy - 11.0), vec2(26.0, 22.0));
            let mut b = ui.new_child(UiBuilder::new().max_rect(r));
            let resp = widgets::icon_button(&mut b, icon, tip, active);
            if icon == Icon::Bell && unread > 0 {
                let c = pos2(r.right() - 5.0, r.top() + 5.0);
                p.circle_filled(c, 6.0, theme::down());
                p.text(c, Align2::CENTER_CENTER, unread.min(99).to_string(), ui_font(8.5), Color32::WHITE);
            }
            if resp.clicked() {
                match icon {
                    Icon::Gauge => self.show_perf = !self.show_perf,
                    Icon::Bell => self.apply(Function::Alerts),
                    _ => {
                        self.apply(Function::Refresh);
                        self.status = "Refreshing congressional filings…".into();
                    }
                }
            }
        }
        right -= 10.0;

        // Layout picker.
        let labels: Vec<&str> = Layout::ALL.iter().map(|l| l.label()).collect();
        let seg_w = widgets::segmented_width(ui, &labels, 20.0);
        let show_layout = right - seg_w - x > 320.0;
        if show_layout {
            right -= seg_w;
            let r = Rect::from_min_size(pos2(right, cy - 10.0), vec2(seg_w, 20.0));
            let mut s = ui.new_child(UiBuilder::new().max_rect(r));
            let sel = self.preset.and_then(|p| Layout::ALL.iter().position(|l| *l == p));
            if let Some(i) = widgets::segmented(&mut s, "layout", &labels, sel, 20.0) {
                self.apply_preset(Layout::ALL[i]);
            }
            s.interact(r, Id::new("layout-tip"), Sense::hover()).on_hover_text("Pane layout (LAYOUT 1|2|2H|3|4|6)");
            right -= 12.0;
        }

        let field = Rect::from_min_max(pos2(x, cy - 11.0), pos2(right.max(x + 160.0).min(x + 860.0), cy + 11.0));
        self.command_field(ui, field)
    }

    /// The View menu: sidebar visibility and side, theme, density, readouts.
    fn view_menu(&mut self, ui: &mut Ui) {
        use crate::prefs::{Action as A, Density, SidebarSide};
        ui.set_min_width(220.0);
        let key = |a: A| self.prefs.hotkeys.chord(a).map(|c| c.display()).unwrap_or_default();
        let (k_sb, k_swap, k_perf) = (key(A::ToggleSidebar), key(A::SwapSidebarSide), key(A::FrameTiming));
        let mut changed = false;
        if ui
            .add(egui::Button::new(if self.sidebar_open { "Hide Sidebar" } else { "Show Sidebar" }).shortcut_text(k_sb))
            .clicked()
        {
            self.sidebar_open = !self.sidebar_open;
            ui.close();
        }
        let a = &mut self.prefs.appearance;
        ui.label(RichText::new("Sidebar Position").font(ui_font(11.0)).color(muted()));
        for (side, label) in [(SidebarSide::Left, "Left"), (SidebarSide::Right, "Right")] {
            if ui.radio(a.sidebar_side == side, label).clicked() {
                a.sidebar_side = side;
                self.sidebar_open = true;
                changed = true;
                ui.close();
            }
        }
        if ui.add(egui::Button::new("Move Sidebar to Other Side").shortcut_text(k_swap)).clicked() {
            a.sidebar_side = a.sidebar_side.other();
            changed = true;
            ui.close();
        }
        ui.separator();
        let choices = self.theme_choices();
        let current = self.prefs.appearance.theme.clone();
        let mut pick = None;
        ui.menu_button("Theme", |ui| {
            for name in &choices {
                if ui.radio(name.eq_ignore_ascii_case(&current), name).clicked() {
                    pick = Some(name.clone());
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Edit Themes…").clicked() {
                pick = Some("EDIT".into());
                ui.close();
            }
        });
        let mut density = None;
        ui.menu_button("Density", |ui| {
            for d in Density::ALL {
                if ui.button(d.label()).clicked() {
                    density = Some(d);
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.add(egui::Button::new("Frame Timing").selected(self.show_perf).shortcut_text(k_perf)).clicked() {
            self.show_perf = !self.show_perf;
            ui.close();
        }
        if ui.button("Settings…").clicked() {
            self.apply(Function::Settings);
            ui.close();
        }
        if ui.button("Brokerage Accounts…").on_hover_text("Connect brokers through SnapTrade (read-only)").clicked() {
            self.open_brokerage_settings();
            ui.close();
        }
        if ui.button("Commands & Hotkeys").clicked() {
            self.apply(Function::Help);
            ui.close();
        }
        if changed {
            self.save_prefs();
        }
        let system = self.system_theme;
        match pick.as_deref() {
            Some("EDIT") => self.theme_command("EDIT", system),
            Some(name) => {
                self.select_theme(name);
            }
            None => {}
        }
        if let Some(d) = density {
            self.theme_command(&d.label().to_ascii_uppercase(), system);
        }
    }

    /// The data-source chip's label, and whether any of the data is live.
    fn source_chip(&self) -> (&'static str, bool) {
        let n = self.provider.to_lowercase();
        if n == "sample" {
            ("Offline", false)
        } else if n.contains("sample") {
            ("Crypto live", true)
        } else {
            ("Live", true)
        }
    }

    fn provider_label(&self) -> String {
        let n = self.provider.to_lowercase();
        if n == "sample" {
            "Offline · sample data".into()
        } else if n.contains("sample") {
            // The browser build (tr_data::web::WebMarkets).
            "Live crypto from Binance and Coinbase; every other market is sample data in the browser".into()
        } else {
            format!("Live · {}", self.provider)
        }
    }

    /// The Bloomberg command line, drawn as a rounded macOS search field.
    fn command_field(&mut self, ui: &mut Ui, field: Rect) -> bool {
        let suggestions = self.suggestions();
        // Claim navigation keys for the dropdown before the text field sees them.
        let (sel_down, sel_up, complete) = if suggestions.is_empty() {
            (false, false, false)
        } else {
            ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::Tab),
                )
            })
        };
        if sel_down {
            self.suggestion = Some(self.suggestion.map_or(0, |i| (i + 1).min(suggestions.len() - 1)));
        }
        if sel_up {
            self.suggestion = self.suggestion.and_then(|i| i.checked_sub(1));
        }
        if complete {
            let pick = &suggestions[self.suggestion.unwrap_or(0)];
            self.cmd = format!("{} ", pick.key);
            self.suggestion = None;
            self.focus_cmd = true;
        }

        let id = Id::new("command-line");
        let had_focus = ui.ctx().memory(|m| m.has_focus(id));
        let ring = anim::spring_bool(ui.ctx(), id.with("ring"), had_focus);
        let p = ui.painter();
        let radius = CornerRadius::same(6);
        if ring > 0.01 {
            p.rect_stroke(
                field.expand(2.0),
                CornerRadius::same(8),
                Stroke::new(3.0, theme::accent().gamma_multiply(0.45 * ring)),
                StrokeKind::Outside,
            );
        }
        p.rect_filled(field, radius, theme::control());
        p.rect_stroke(field, radius, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
        widgets::draw_icon(
            p,
            Rect::from_center_size(pos2(field.left() + 15.0, field.center().y), vec2(16.0, 16.0)),
            Icon::Search,
            muted(),
        );
        // A small "GO" key cap at the trailing edge, Bloomberg-style. It lights up once
        // there is something to run and presses in on a click or Enter.
        let go = Rect::from_center_size(pos2(field.right() - 18.0, field.center().y), vec2(28.0, 15.0));
        let go_resp = ui.interact(go, id.with("go"), Sense::click()).on_hover_text("<GO> (Enter)");
        let now = ui.input(|i| i.time);
        let entered = ui.ctx().data(|d| d.get_temp::<f64>(id.with("go-entered"))).is_some_and(|t| now - t < 0.09);
        let held = go_resp.is_pointer_button_down_on() || entered;
        let press = anim::spring_with(ui.ctx(), id.with("go-press"), if held { 1.0 } else { 0.0 }, 0.3, 0.55);
        let lit = anim::spring_bool(ui.ctx(), id.with("go-lit"), !self.cmd.trim().is_empty());
        let cap = Rect::from_center_size(go.center(), go.size() * (1.0 - 0.14 * press));
        p.rect_filled(cap, 4.0, widgets::lerp_color(theme::control_hover(), amber(), lit));
        p.text(
            cap.center(),
            Align2::CENTER_CENTER,
            "GO",
            mono(10.5),
            widgets::lerp_color(muted(), Color32::BLACK, lit),
        );
        let go_clicked = go_resp.clicked();

        let inner = Rect::from_min_max(pos2(field.left() + 28.0, field.top()), pos2(go.left() - 6.0, field.bottom()));
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner));
        let te = egui::TextEdit::singleline(&mut self.cmd)
            .id(id)
            .font(mono(12.5))
            .text_color(amber())
            .vertical_align(egui::Align::Center)
            .hint_text(
                RichText::new("Search or type a command — BTCUSD <CRNCY> · US10Y GOVT · CL1 CMDTY · AAPL · WEI · GC")
                    .font(ui_font(12.0))
                    .color(muted()),
            )
            .desired_width(inner.width())
            .min_size(vec2(inner.width(), inner.height()))
            .frame(egui::Frame::NONE);
        let resp = child.add(te.lock_focus(true));
        if resp.changed() {
            self.suggestion = None;
            let q = self.cmd.trim().to_string();
            if q.len() >= 2 && !self.suggestions().is_empty() {
                self.search_pending = Some((q, Instant::now()));
            }
        }
        if self.focus_cmd {
            resp.request_focus();
            // Place the cursor at the end after type-to-command.
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                let ccursor = egui::text::CCursor::new(self.cmd.chars().count());
                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(ccursor)));
                state.store(ui.ctx(), id);
            }
            self.focus_cmd = false;
        }
        let focused = resp.has_focus();
        let (enter, esc, up, down) = ui.input(|i| {
            (
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
            )
        });
        if (resp.lost_focus() && enter) || go_clicked {
            if enter {
                ui.ctx().data_mut(|d| d.insert_temp(id.with("go-entered"), now));
                ui.ctx().request_repaint();
            }
            let input = std::mem::take(&mut self.cmd);
            match self.suggestion.take().and_then(|i| suggestions.get(i)) {
                Some(pick) => self.execute(&pick.key),
                None if !input.trim().is_empty() => self.execute(&input),
                None => {}
            }
            self.cmd_hist_pos = None;
            // The command line stays live, Bloomberg-style, unless the command opened the
            // order ticket (which takes the keyboard for its quantity) or the palette.
            self.focus_cmd = !self.palette.open && !self.trading.ticket.as_ref().is_some_and(|t| t.focus_qty > 0);
            if !self.focus_cmd {
                resp.surrender_focus();
            }
        } else if had_focus && esc {
            self.cmd.clear();
            resp.surrender_focus();
        } else if focused && (up || down) && !self.cmd_history.is_empty() {
            let n = self.cmd_history.len();
            let pos = match (self.cmd_hist_pos, up) {
                (None, true) => n - 1,
                (Some(p), true) => p.saturating_sub(1),
                (Some(p), false) if p + 1 < n => p + 1,
                _ => {
                    self.cmd_hist_pos = None;
                    self.cmd.clear();
                    return focused;
                }
            };
            self.cmd_hist_pos = Some(pos);
            self.cmd = self.cmd_history[pos].clone();
        }
        if focused && !self.cmd.is_empty() {
            self.dropdown(ui.ctx(), field, &suggestions);
        }
        focused
    }

    /// Security suggestions for what is typed, unless it is a function command.
    pub(super) fn suggestions(&self) -> Vec<SearchHit> {
        let q = self.cmd.trim();
        if q.is_empty()
            || matches!(command::parse(q), Some(command::Command::Function(_) | command::Command::SymbolFunction(..)))
        {
            return Vec::new();
        }
        let net: &[SearchHit] = if self.net_hits.0 == q { &self.net_hits.1 } else { &[] };
        let mut v = markets_view::merged_hits(q, net);
        v.truncate(8);
        v
    }

    /// Autocomplete popover under the command field.
    fn dropdown(&mut self, ctx: &egui::Context, below: Rect, hits: &[SearchHit]) {
        if hits.is_empty() {
            return;
        }
        let mut clicked = None;
        let width = below.width().clamp(420.0, 680.0);
        let shown = anim::ease_out(anim::appear(ctx, Id::new("cmd-suggestions-in"), 0.18));
        egui::Area::new(Id::new("cmd-suggestions"))
            .order(egui::Order::Foreground)
            .fixed_pos(below.left_bottom() + vec2(0.0, 6.0 - 6.0 * (1.0 - shown)))
            .show(ctx, |ui| {
                ui.multiply_opacity(shown);
                egui::Frame::new()
                    .fill(theme::card())
                    .stroke(Stroke::new(1.0, theme::border()))
                    .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
                    .shadow(Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(70) })
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.set_width(width);
                        let glider = widgets::Glider::new(ui, Id::new("cmd-suggestions-sel"));
                        for (i, h) in hits.iter().enumerate() {
                            let (r, resp) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click());
                            if self.suggestion == Some(i) {
                                glider.on(ui, r);
                            }
                            widgets::hover_wash(ui, r, &resp, false);
                            let p = ui.painter();
                            let class_c = markets_view::class_color(h.class);
                            let pill = Rect::from_min_size(pos2(r.left() + 6.0, r.center().y - 8.0), vec2(52.0, 16.0));
                            p.rect_filled(pill, 8.0, class_c.gamma_multiply(0.18));
                            p.text(pill.center(), Align2::CENTER_CENTER, h.class.label(), ui_font(10.0), class_c);
                            p.text(
                                pos2(pill.right() + 10.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &h.key,
                                mono(12.5),
                                text(),
                            );
                            p.text(
                                pos2(pill.right() + 190.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &h.name,
                                ui_font(12.5),
                                muted(),
                            );
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                            }
                            if resp.clicked() {
                                clicked = Some(h.key.clone());
                            }
                        }
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(if MAC {
                                "↑ ↓ select   ⇥ complete   ↩ load"
                            } else {
                                "Up/Down select   Tab complete   Enter load"
                            })
                            .font(ui_font(10.5))
                            .color(muted()),
                        );
                    });
            });
        if let Some(key) = clicked {
            self.cmd.clear();
            self.suggestion = None;
            self.execute(&key);
            self.focus_cmd = true;
        }
    }

    // ---- sidebar -------------------------------------------------------------------------

    /// Source-list sidebar: watchlist, markets and Congress sections. `rect` is the full
    /// sidebar (it slides in from the left, so it may extend past the window edge).
    pub(super) fn sidebar(&mut self, ui: &mut Ui, rect: Rect) -> Vec<SidebarCmd> {
        let mut out = Vec::new();
        let p = ui.painter();
        p.rect_filled(rect, 0.0, theme::sidebar());
        let on_right = self.prefs.appearance.sidebar_side == crate::prefs::SidebarSide::Right;
        let edge = if on_right { rect.left() + 0.5 } else { rect.right() - 0.5 };
        p.vline(edge, rect.y_range(), Stroke::new(1.0, theme::border()));
        let head = Rect::from_min_size(rect.min, vec2(rect.width(), self.toolbar_h()));
        window_drag_region(ui, head, "sidebar");
        if !cfg!(target_os = "macos") {
            p.text(head.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, "TERMINAL", mono(12.5), amber());
        }

        let body = Rect::from_min_max(pos2(rect.left(), head.bottom()), rect.max);
        let mut child = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(8.0, 0.0))).id_salt("sidebar"));
        child.set_clip_rect(body.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            let focused = self.idx(self.focus).map(|i| std::mem::discriminant(&self.panes[i].kind));
            let is_focused = |k: &PaneKind| focused == Some(std::mem::discriminant(k));

            section(ui, "Watchlist");
            let mut remove = None;
            for key in &self.watchlist {
                let sec = tr_data::security(key);
                let resp = row_frame(ui, false);
                let r = resp.rect;
                let pt = ui.painter();
                pt.text(pos2(r.left() + 8.0, r.center().y), Align2::LEFT_CENTER, &sec.ticker, mono(12.0), text());
                if let Some(q) = self.quotes.get(key) {
                    let up = q.change() >= 0.0;
                    let c = if up { theme::up() } else { theme::down() };
                    let pct = format!("{:+.2}%", q.change_pct());
                    let pill = Rect::from_min_size(pos2(r.right() - 62.0, r.center().y - 8.0), vec2(58.0, 16.0));
                    pt.rect_filled(pill, 4.0, c.gamma_multiply(0.16));
                    pt.text(pill.right_center() - vec2(5.0, 0.0), Align2::RIGHT_CENTER, pct, mono(11.0), c);
                    let d = sec.decimals(q.last);
                    pt.text(
                        pos2(pill.left() - 8.0, r.center().y),
                        Align2::RIGHT_CENTER,
                        format!("{:.d$}", q.last),
                        mono(11.5),
                        text(),
                    );
                } else {
                    pt.text(
                        pos2(r.right() - 8.0, r.center().y),
                        Align2::RIGHT_CENTER,
                        sec.class.yellow_key().label(),
                        ui_font(10.5),
                        muted(),
                    );
                }
                if resp.clicked() {
                    out.push(SidebarCmd::Action(Action::OpenChart(key.clone())));
                }
                let resp = resp.on_hover_text(format!("{} — {}", sec.key(), sec.name().unwrap_or(&sec.ticker)));
                resp.context_menu(|ui| {
                    if ui.button("Open Chart").clicked() {
                        out.push(SidebarCmd::Action(Action::OpenChart(key.clone())));
                        ui.close();
                    }
                    if sec.has_congress_trades() && ui.button("Who in Congress Traded This").clicked() {
                        out.push(SidebarCmd::Action(Action::OpenTicker(sec.ticker.clone())));
                        ui.close();
                    }
                    if ui.button("Compare on Chart").clicked() {
                        out.push(SidebarCmd::Function(Function::Compare(Some(key.clone()))));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Remove from Watchlist").clicked() {
                        remove = Some(key.clone());
                        ui.close();
                    }
                });
            }
            if let Some(k) = remove {
                self.watchlist.retain(|x| *x != k);
            }
            if nav_row(ui, Icon::List, "Open Watchlist Pane", None, is_focused(&PaneKind::Watchlist)).clicked() {
                out.push(SidebarCmd::Function(Function::Watchlist));
            }

            section(ui, "Markets");
            let items = [
                (Icon::Globe, "World Markets", "WEI", Function::Wei, PaneKind::Wei),
                (Icon::Curve, "Yield Curve", "GC", Function::Curve, PaneKind::Curve),
                (
                    Icon::Chart,
                    "Options",
                    "OMON",
                    Function::Options,
                    PaneKind::Options(Box::new(super::OptionsPane::new("", 0, true))),
                ),
                (
                    Icon::Search,
                    "Security Search",
                    "SECF",
                    Function::Search(String::new()),
                    PaneKind::Search { query: String::new() },
                ),
            ];
            for (icon, label, code, f, kind) in items {
                if nav_row(ui, icon, label, Some(code), is_focused(&kind)).clicked() {
                    out.push(SidebarCmd::Function(f));
                }
            }

            section(ui, "Portfolio");
            let focused_view = self.idx(self.focus).and_then(|i| match &self.panes[i].kind {
                PaneKind::Portfolio(pp) => Some(pp.view),
                _ => None,
            });
            let pf = self.shown_portfolio();
            let value_badge = pf.filter(|p| !p.accounts.is_empty()).map(|p| {
                let t = p.totals(None, |pos| self.broker.live_price(&self.quotes, pos));
                compact_money(t.value)
            });
            let open_orders = pf.map_or(0, |p| p.orders.iter().filter(|o| o.is_open()).count());
            let orders_badge = (open_orders > 0).then(|| format!("{open_orders} open"));
            use crate::command::PortView as V;
            let setup = self.broker.mode() == super::broker::BrokerMode::Setup;
            let rows: [(Icon, &str, V, Option<String>); 4] = [
                (Icon::Grid, "Portfolio", V::Summary, if setup { Some("Set up".into()) } else { value_badge }),
                (Icon::List, "Positions", V::Positions, None),
                (Icon::Chart, "Orders", V::Orders, orders_badge),
                (Icon::People, "Accounts", V::Accounts, None),
            ];
            for (icon, label, view, badge) in rows {
                let trailing = badge.unwrap_or_else(|| view.code().to_string());
                let resp = nav_row(ui, icon, label, Some(&trailing), focused_view == Some(view));
                if resp.clicked() {
                    out.push(SidebarCmd::Function(Function::Portfolio(view, String::new())));
                }
            }
            let settings_open = self.idx(self.focus).is_some_and(|i| matches!(self.panes[i].kind, PaneKind::Settings))
                && self.settings_tab == super::settings_view::SettingsTab::Brokerage;
            let resp = nav_row(ui, Icon::Gear, "Brokerage Settings", None, settings_open)
                .on_hover_text("Connect brokers through SnapTrade: keys, connections, test");
            if resp.clicked() {
                out.push(SidebarCmd::Function(Function::Portfolio(V::Summary, "SETUP".into())));
            }

            section(ui, "Congress");
            let unread = self.alerts.iter().filter(|a| !a.seen).count();
            if nav_row(
                ui,
                Icon::List,
                "Latest Trades",
                Some("CONG"),
                is_focused(&PaneKind::Feed { filter: Default::default() }),
            )
            .clicked()
            {
                out.push(SidebarCmd::Function(Function::Congress));
            }
            if nav_row(ui, Icon::People, "Politicians", Some("POL"), is_focused(&PaneKind::Politicians)).clicked() {
                out.push(SidebarCmd::Function(Function::Politician(String::new())));
            }
            let badge = (unread > 0).then(|| unread.to_string());
            if nav_row(ui, Icon::Bell, "Alerts", badge.as_deref().or(Some("ALRT")), is_focused(&PaneKind::Alerts))
                .clicked()
            {
                out.push(SidebarCmd::Function(Function::Alerts));
            }
            let tracked: Vec<_> = self.politicians.iter().filter(|s| s.tracked).collect();
            if !tracked.is_empty() {
                section(ui, "Tracked");
                for s in tracked {
                    let resp = row_frame(ui, false);
                    let r = resp.rect;
                    let pt = ui.painter();
                    let pol = &s.politician;
                    pt.circle_filled(pos2(r.left() + 12.0, r.center().y), 4.0, theme::party_color(pol.party));
                    pt.text(pos2(r.left() + 24.0, r.center().y), Align2::LEFT_CENTER, &pol.name, ui_font(12.5), text());
                    pt.text(
                        pos2(r.right() - 8.0, r.center().y),
                        Align2::RIGHT_CENTER,
                        s.trades.to_string(),
                        mono(11.0),
                        muted(),
                    );
                    if resp.clicked() {
                        out.push(SidebarCmd::Action(Action::OpenProfile(pol.name.clone())));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Open Profile").clicked() {
                            out.push(SidebarCmd::Action(Action::OpenProfile(pol.name.clone())));
                            ui.close();
                        }
                        if ui.button("Stop Tracking").clicked() {
                            out.push(SidebarCmd::Action(Action::Track(pol.name.clone(), false)));
                            ui.close();
                        }
                    });
                }
            }

            self.sidebar_layouts(ui, &mut out);

            section(ui, "More");
            if nav_row(ui, Icon::Gear, "Settings", Some("PDF"), is_focused(&PaneKind::Settings)).clicked() {
                out.push(SidebarCmd::Function(Function::Settings));
            }
            if nav_row(ui, Icon::Star, "Commands & Hotkeys", Some("HELP"), is_focused(&PaneKind::Help)).clicked() {
                out.push(SidebarCmd::Function(Function::Help));
            }
            ui.add_space(12.0);
        });
        out
    }

    // ---- layouts and status --------------------------------------------------------------

    /// Saved layouts: click to switch, double-click or context menu to rename, ⌘1…⌘9 hints.
    fn sidebar_layouts(&mut self, ui: &mut Ui, out: &mut Vec<SidebarCmd>) {
        use super::workspace::LayoutOp;
        ui.horizontal(|ui| {
            section(ui, "Layouts");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(RichText::new("+").font(ui_font(14.0)).color(muted())).frame(false))
                    .on_hover_text("New layout")
                    .clicked()
                {
                    out.push(SidebarCmd::Layout(LayoutOp::New(None)));
                }
            });
        });
        let slots: Vec<crate::prefs::Action> =
            crate::prefs::Action::ALL.iter().copied().filter(|a| a.layout_slot().is_some()).collect();
        for (i, name) in self.layout_names.clone().iter().enumerate() {
            let current = name.eq_ignore_ascii_case(&self.layout_name);
            if let Some((old, buf)) = &mut self.renaming_layout
                && old == name
            {
                let r = ui.add(egui::TextEdit::singleline(buf).font(ui_font(12.5)).desired_width(f32::INFINITY));
                r.request_focus();
                if r.lost_focus() {
                    let commit = !ui.input(|i| i.key_pressed(Key::Escape)) && !buf.trim().is_empty() && buf != old;
                    if commit {
                        out.push(SidebarCmd::Layout(LayoutOp::Rename(old.clone(), buf.trim().to_string())));
                    }
                    self.renaming_layout = None;
                }
                continue;
            }
            let hint = slots.get(i).and_then(|a| self.prefs.hotkeys.chord(*a)).map(|c| c.display());
            let resp = nav_row(ui, Icon::Grid, name, hint.as_deref(), current);
            if resp.clicked() && !current {
                out.push(SidebarCmd::Layout(LayoutOp::Switch(name.clone())));
            }
            if resp.double_clicked() {
                self.renaming_layout = Some((name.clone(), name.clone()));
            }
            resp.context_menu(|ui| {
                if !current && ui.button("Switch To").clicked() {
                    out.push(SidebarCmd::Layout(LayoutOp::Switch(name.clone())));
                    ui.close();
                }
                if ui.button("Rename…").clicked() {
                    self.renaming_layout = Some((name.clone(), name.clone()));
                    ui.close();
                }
                if ui.button("Duplicate").clicked() {
                    out.push(SidebarCmd::Layout(LayoutOp::Duplicate(name.clone(), None)));
                    ui.close();
                }
                if ui.button("Export to Downloads").clicked() {
                    out.push(SidebarCmd::Layout(LayoutOp::Export(name.clone(), None)));
                    ui.close();
                }
                ui.separator();
                if ui.button("Delete").clicked() {
                    out.push(SidebarCmd::Layout(LayoutOp::Delete(name.clone())));
                    ui.close();
                }
            });
        }
        if nav_row(ui, Icon::Plus, "Save Current As New…", None, false).clicked() {
            let name = self.store.unique_name(&format!("{} copy", self.layout_name));
            out.push(SidebarCmd::Layout(LayoutOp::SaveAs(name.clone())));
            self.renaming_layout = Some((name.clone(), name));
        }
    }

    pub(super) fn status_bar(&self, ui: &Ui, rect: Rect) {
        let p = ui.painter_at(rect);
        p.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, theme::border()));
        let cy = rect.center().y;
        let mut x = rect.left() + 10.0;
        if self.cmd.trim().is_empty() {
            use crate::prefs::Action as A;
            let keys: Vec<(String, &str)> = [
                (A::Help, "Help"),
                (A::Congress, "Congress"),
                (A::Watchlist, "Watchlist"),
                (A::NextPane, "Next Pane"),
                (A::SplitRight, "Split"),
                (A::PopOut, "Pop Out"),
                (A::SaveLayout, "Save Layout"),
                (A::Settings, "Settings"),
            ]
            .into_iter()
            .filter_map(|(a, l)| Some((self.prefs.hotkeys.chord(a)?.display(), l)))
            .collect();
            for (k, label) in keys {
                if x > rect.center().x {
                    break;
                }
                let g = p.layout_no_wrap(k.to_string(), mono(10.0), text());
                let cap = Rect::from_min_size(pos2(x, cy - 8.0), vec2(g.size().x + 8.0, 16.0));
                p.rect_filled(cap, 4.0, theme::control());
                p.rect_stroke(cap, 4.0, Stroke::new(1.0, theme::border()), StrokeKind::Inside);
                p.galley(pos2(x + 4.0, cy - g.size().y / 2.0), g, text());
                x = cap.right() + 5.0;
                let g = p.layout_no_wrap(label.to_string(), ui_font(11.0), muted());
                p.galley(pos2(x, cy - g.size().y / 2.0), g.clone(), muted());
                x += g.size().x + 12.0;
            }
        } else {
            p.text(pos2(x, cy), Align2::LEFT_CENTER, command::hint(&self.cmd), mono(11.0), text());
        }
        let status = if self.show_perf {
            format!(
                "UI {:.2} ms · frame {:.1} ms ({:.0} fps) · {}",
                self.perf_cpu_ms,
                self.perf_dt_ms,
                1000.0 / self.perf_dt_ms.max(0.1),
                self.status
            )
        } else {
            self.status.clone()
        };
        p.text(pos2(rect.right() - 10.0, cy), Align2::RIGHT_CENTER, status, ui_font(11.5), muted());
    }

    /// Alerts stacked at the top right: each slides in from the edge with a slight
    /// overshoot, the stack glides up as older ones leave, and they fade out at the end.
    pub(super) fn toasts(&self, ctx: &egui::Context) {
        let screen = ctx.content_rect();
        let layer = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("toasts")));
        let life = super::TOAST_SECS as f32;
        for (k, (msg, at)) in self.toasts.iter().enumerate() {
            let age = at.elapsed().as_secs_f32();
            let enter = anim::ease_out_back(anim::since(ctx, age as f64, 0.55));
            // Reduce Motion skips the fade: the toast stays until it is removed.
            let fade = if anim::reduced() { 0.0 } else { 0.35 };
            let leave = if fade > 0.0 { anim::span(age, life - fade, life) } else { 0.0 };
            // Wake for the fade-out (and the removal at the end of it).
            ctx.request_repaint_after(Duration::from_secs_f32((life - fade - age).max(0.0)));
            let y =
                anim::spring(ctx, Id::new(("toast-y", *at)), screen.top() + self.toolbar_h() + 10.0 + k as f32 * 58.0);
            let x = screen.right() - 432.0 + (1.0 - enter) * 444.0;
            let r = Rect::from_min_size(pos2(x, y), vec2(420.0, 50.0));
            let mut p = layer.clone();
            p.multiply_opacity(1.0 - leave);
            widgets::card_on(&p, r, 0.0);
            // Order entry prefixes its toasts (`+` filled/ok, `!` rejected); everything else
            // is a Congress alert.
            let (title, msg, color) = match msg.strip_prefix(super::trading::TOAST_PREFIX) {
                Some(rest) => match rest.split_at_checked(1) {
                    Some(("+", m)) => ("Trading", m, theme::up()),
                    Some(("!", m)) => ("Trading · not done", m, theme::down()),
                    _ => ("Trading", rest, amber()),
                },
                None => ("Congress Alert", msg.as_str(), amber()),
            };
            p.rect_filled(Rect::from_min_size(r.min + vec2(1.0, 6.0), vec2(3.0, r.height() - 12.0)), 1.5, color);
            widgets::draw_icon(
                &p,
                Rect::from_center_size(pos2(r.left() + 22.0, r.center().y), vec2(18.0, 18.0)),
                Icon::Bell,
                color,
            );
            p.text(pos2(r.left() + 42.0, r.top() + 15.0), Align2::LEFT_CENTER, title, ui_font(12.5), text());
            p.text(pos2(r.left() + 42.0, r.top() + 33.0), Align2::LEFT_CENTER, msg, ui_font(11.5), muted());
        }
    }
}

/// Small grey section header, as in a macOS source list.
fn section(ui: &mut Ui, label: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(label).font(ui_font(11.0)).color(muted()));
    ui.add_space(2.0);
}

/// A full-width clickable sidebar row with hover wash; the caller paints its content.
fn row_frame(ui: &mut Ui, selected: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), theme::dp(ROW_H)), Sense::click());
    widgets::hover_wash(ui, r, &resp, selected);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    resp
}

/// `$214.3K`, `$1.25M` for sidebar badges.
fn compact_money(x: f64) -> String {
    let a = x.abs();
    let s = if a >= 1e9 {
        format!("{:.2}B", a / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", a / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}K", a / 1e3)
    } else {
        format!("{a:.0}")
    };
    if x < 0.0 { format!("-${s}") } else { format!("${s}") }
}

fn nav_row(ui: &mut Ui, icon: Icon, label: &str, trailing: Option<&str>, selected: bool) -> egui::Response {
    let resp = row_frame(ui, selected);
    let r = resp.rect;
    let p = ui.painter();
    let ic = if selected { theme::accent() } else { theme::text().gamma_multiply(0.8) };
    widgets::draw_icon(p, Rect::from_center_size(pos2(r.left() + 14.0, r.center().y), vec2(16.0, 16.0)), icon, ic);
    p.text(pos2(r.left() + 30.0, r.center().y), Align2::LEFT_CENTER, label, ui_font(13.0), text());
    if let Some(t) = trailing {
        let badge = t.chars().all(|c| c.is_ascii_digit());
        if badge {
            let g = p.layout_no_wrap(t.to_string(), ui_font(10.5), Color32::WHITE);
            let pill = Rect::from_min_size(
                pos2(r.right() - 10.0 - g.size().x - 10.0, r.center().y - 8.0),
                vec2(g.size().x + 10.0, 16.0),
            );
            p.rect_filled(pill, 8.0, theme::down());
            p.galley(pos2(pill.left() + 5.0, r.center().y - g.size().y / 2.0), g, Color32::WHITE);
        } else {
            p.text(pos2(r.right() - 8.0, r.center().y), Align2::RIGHT_CENTER, t, mono(10.5), muted());
        }
    }
    resp
}
