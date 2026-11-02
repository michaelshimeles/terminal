//! Command palette overlay (⌘K, Ctrl+K elsewhere): the live catalogue of items built
//! from the app's state, the centred search card, and running the picked item.

use egui::{Color32, Id, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use tr_core::indicators::registry;

use super::TerminalApp;
use super::settings_view::SettingsTab;
use crate::anim;
use crate::command::{self, Function, IndicatorSpec};
use crate::layout::Layout;
use crate::palette::{Item, Kind, Target};
use crate::prefs::Action;
use crate::theme::{self, dp, mono, muted, text, ui_font};
use crate::widgets;

const INPUT_ID: &str = "command-palette-input";
const ROW_H: f32 = 34.0;
const VISIBLE_ROWS: usize = 10;

/// Function codes offered by the palette: code, description (empty uses the command
/// line's own hint), extra search words, and the action whose shortcut is shown.
const COMMANDS: &[(&str, &str, &str, Option<Action>)] = &[
    ("PORT", "", "portfolio prtu brokerage holdings value", Some(Action::Portfolio)),
    ("ACCT", "", "accounts balances brokerage", None),
    ("POS", "", "positions holdings brokerage", Some(Action::Positions)),
    ("ORD", "", "orders blotter brokerage", Some(Action::Orders)),
    ("ACTV", "", "activity transactions dividends brokerage", None),
    ("TRADE", "", "order ticket buy sell emsx", None),
    ("PORT REFRESH", "", "brokerage sync reload", None),
    ("PORT CONNECT", "", "brokerage link snaptrade", None),
    ("OMON", "", "options chain calls puts greeks", Some(Action::NewOptions)),
    ("CONG", "Congressional trades feed", "congress disclosures", Some(Action::Congress)),
    ("POL", "Politicians", "congress members profile", Some(Action::Politicians)),
    ("ALRT", "", "alerts tracked filings", Some(Action::Alerts)),
    ("REFRESH", "", "congress disclosures sync", Some(Action::Refresh)),
    ("WL", "", "watchlist monitor", Some(Action::Watchlist)),
    ("N", "", "news headlines", None),
    ("OB", "", "order book depth dom", None),
    ("PERP", "", "hip-3 perpetual hyperliquid xyz perps stock perp", Some(Action::TogglePerp)),
    ("WEI", "", "world equity indices markets", None),
    ("GC", "", "yield curve treasury rates", None),
    ("SECF", "Security search", "search find", None),
    ("LAYOUTS", "", "saved layouts workspaces", None),
    ("LAYOUT NEW", "Start a new saved layout", "workspace", None),
    ("SPLIT", "", "split right pane", Some(Action::SplitRight)),
    ("SPLIT DOWN", "", "split below pane", Some(Action::SplitDown)),
    ("POP", "", "pop out window undock", Some(Action::PopOut)),
    ("CLOSE", "", "close pane", Some(Action::ClosePane)),
    ("LINK OFF", "", "unlink colour group", None),
    ("THEME NEXT", "Cycle to the next theme", "appearance", Some(Action::NextTheme)),
    ("THEME LIGHT", "Use the light theme", "appearance", None),
    ("THEME DARK", "Use the dark theme", "appearance", None),
    ("THEME SYSTEM", "Follow the OS appearance", "appearance auto", None),
    ("THEME COMPACT", "Compact density", "appearance spacing", None),
    ("THEME COMFORTABLE", "Comfortable density", "appearance spacing", None),
    ("SIDEBAR LEFT", "Sidebar on the left", "dock side", None),
    ("SIDEBAR RIGHT", "Sidebar on the right", "dock side", None),
    ("PREFS", "", "settings preferences config", Some(Action::Settings)),
    ("UPDATE", "", "check for updates new version upgrade", None),
    ("HELP", "", "help functions reference", Some(Action::Help)),
    ("CANDLE", "", "chart type candlesticks", None),
    ("BAR", "", "chart type ohlc", None),
    ("LINE", "", "chart type", None),
    ("AREA", "", "chart type mountain", None),
    ("LOG", "", "log scale chart", Some(Action::LogScale)),
    ("IND", "", "indicators studies technical analysis picker", None),
    ("CLEAR", "", "indicators remove", None),
    ("CMP", "", "compare comparison remove", None),
    ("TL", "", "trendline draw", Some(Action::DrawTrendline)),
    ("HL", "", "horizontal line draw", Some(Action::DrawHorizontal)),
    ("VL", "", "vertical line draw", Some(Action::DrawVertical)),
    ("FIB", "", "fibonacci retracement draw", Some(Action::DrawFib)),
    ("MAGNET", "", "snap ohlc drawings", Some(Action::Magnet)),
    ("CLEAR DRAW", "", "drawings remove", None),
    ("1", "", "interval timeframe minute", None),
    ("5", "", "interval timeframe minutes", None),
    ("15", "", "interval timeframe minutes", None),
    ("1H", "", "interval timeframe hour", None),
    ("4H", "", "interval timeframe hours", None),
    ("1D", "", "interval timeframe daily", None),
    ("1W", "", "interval timeframe weekly", None),
    ("1M", "", "interval timeframe monthly", None),
    ("RANGE 5D", "", "quick range history", None),
    ("RANGE 1M", "", "quick range history month", None),
    ("RANGE 3M", "", "quick range history months", None),
    ("RANGE 6M", "", "quick range history months", None),
    ("YTD", "", "quick range year to date", None),
    ("RANGE 1Y", "", "quick range history year", None),
    ("RANGE 5Y", "", "quick range history years", None),
    ("RANGE ALL", "", "quick range full history", None),
];

/// Workspace actions run through their hotkey handler, with extra search words.
const ACTIONS: &[(Action, &str)] = &[
    (Action::ToggleSidebar, "show hide sidebar"),
    (Action::SwapSidebarSide, "swap flip sidebar side left right"),
    (Action::NextPane, "focus cycle"),
    (Action::PrevPane, "focus cycle"),
    (Action::NewTab, "chart tab"),
    (Action::SaveLayout, "layout workspace"),
    (Action::ResetView, "chart zoom fit"),
    (Action::FrameTiming, "fps performance perf"),
];

fn settings_blurb(tab: SettingsTab) -> &'static str {
    match tab {
        SettingsTab::Appearance => "Theme, light and dark choices, sidebar, vibrancy",
        SettingsTab::Themes => "Edit, duplicate, import and export themes",
        SettingsTab::Hotkeys => "Rebind keyboard shortcuts",
        SettingsTab::Panels => "Defaults for new charts and order books",
        SettingsTab::Layouts => "Saved layouts and presets",
        SettingsTab::Brokerage => "SnapTrade keys and connected brokerages",
        SettingsTab::Trading => "Enable trading, order limits and confirmations",
    }
}

/// The static part of the catalogue: commands, panels, actions, presets and settings tabs.
pub(super) fn static_items() -> Vec<Item> {
    let mut v = Vec::new();
    for &(code, desc, words, hotkey) in COMMANDS {
        let subtitle = if desc.is_empty() { command::hint(code) } else { desc.to_string() };
        let mut it = Item::new(Kind::Command, code, subtitle, Target::Run(code.into())).keywords(words);
        it.hotkey = hotkey;
        v.push(it);
    }
    for a in Action::ALL {
        if let Some(spec) = a.panel_spec() {
            let it = Item::new(Kind::Panel, a.label(), format!("NEW {spec}"), Target::Run(format!("NEW {spec}")));
            v.push(it.keywords("panel pane add open").hotkey(a));
        }
    }
    v.push(
        Item::new(Kind::Panel, "New Portfolio pane", "NEW PORT", Target::Run("NEW PORT".into()))
            .keywords("panel pane brokerage"),
    );
    v.push(
        Item::new(
            Kind::Action,
            "Open Brokerage settings",
            "BROKER · SnapTrade keys and connections",
            Target::Run("BROKER".into()),
        )
        .keywords("snaptrade keys connect broker"),
    );
    for (a, words) in
        [(Action::Buy, "trade order ticket purchase long"), (Action::Sell, "trade order ticket close exit")]
    {
        v.push(
            Item::new(Kind::Action, a.label(), "Trading · order ticket", Target::Hotkey(a)).keywords(words).hotkey(a),
        );
    }
    v.push(
        Item::new(
            Kind::Action,
            "Switch to the demo account",
            "DEMO · paper trading, virtual cash",
            Target::Run("DEMO".into()),
        )
        .keywords("paper practice simulated trading account mode"),
    );
    v.push(
        Item::new(
            Kind::Action,
            "Switch to live accounts",
            "MODE LIVE · real brokerage accounts (asks first)",
            Target::Run("MODE LIVE".into()),
        )
        .keywords("real money brokerage snaptrade trading account mode"),
    );
    v.push(
        Item::new(Kind::Action, "Reset the demo account", "Settings › Trading", Target::Settings("Trading".into()))
            .keywords("paper demo restart cash"),
    );
    for &(a, words) in ACTIONS {
        v.push(Item::new(Kind::Action, a.label(), a.category(), Target::Hotkey(a)).keywords(words).hotkey(a));
    }
    for l in Layout::ALL {
        let n = l.slots();
        v.push(
            Item::new(
                Kind::Layout,
                format!("Grid preset {}", l.label()),
                format!("{n} pane{} · LAYOUT {}", if n == 1 { "" } else { "s" }, l.label()),
                Target::Run(format!("LAYOUT {}", l.label())),
            )
            .keywords("grid arrange panes"),
        );
    }
    for tab in SettingsTab::ALL {
        v.push(
            Item::new(
                Kind::Settings,
                format!("Settings › {}", tab.label()),
                settings_blurb(tab),
                Target::Settings(tab.label().into()),
            )
            .keywords("settings preferences"),
        );
    }
    v
}

/// One row per catalogue indicator. Those already on the target chart say so and run
/// their exact label, so picking them removes them.
fn indicator_items(active: &[IndicatorSpec]) -> Vec<Item> {
    registry::all()
        .iter()
        .map(|d| {
            let on = active.iter().find(|s| s.code == d.code);
            let label = on.map_or_else(|| IndicatorSpec::of(d).label(), IndicatorSpec::label);
            let sub = format!("{label} · {}{}", d.category.label(), if on.is_some() { " · on chart" } else { "" });
            let words = format!("{} {} {} indicator study", d.code, d.aliases.join(" "), d.category.label());
            let line = if on.is_some() { label } else { d.code.to_string() };
            Item::new(Kind::Indicator, d.name, sub, Target::Run(line)).keywords(words)
        })
        .collect()
}

fn ticker_item(key: &str) -> Item {
    use tr_core::perps;
    if perps::is_perp(key) {
        // A perp's row says it is one, and finds it by the underlying's ticker and name too.
        let link = perps::link_for(key);
        let badge = perps::badge(key);
        let sub = match link {
            Some(l) => format!("{} · {badge}", l.name),
            None => badge.clone(),
        };
        let under = link.and_then(|l| tr_core::Security::parse(l.underlying)).map(|s| s.ticker).unwrap_or_default();
        let words = format!("{under} {} perp perpetual hyperliquid hip-3 {}", link.map_or("", |l| l.name), badge);
        return Item::new(Kind::Ticker, key, sub, Target::Run(key.into())).keywords(words);
    }
    let name = tr_core::Security::parse(key).and_then(|s| s.name()).unwrap_or("");
    Item::new(
        Kind::Ticker,
        key,
        if name.is_empty() { "Load in the focused chart" } else { name },
        Target::Run(key.into()),
    )
    .keywords(name)
}

fn kind_color(k: Kind) -> Color32 {
    match k {
        Kind::Command => theme::amber(),
        Kind::Ticker => theme::cyan(),
        Kind::Panel => theme::accent(),
        Kind::Layout => theme::series(2),
        Kind::Theme => theme::series(4),
        Kind::Settings => theme::muted(),
        Kind::Action => theme::up(),
        Kind::Indicator => theme::series(3),
    }
}

impl TerminalApp {
    /// Every item the palette can find right now, including tickers for the query.
    pub(super) fn palette_items(&self) -> Vec<Item> {
        let mut v = static_items();
        for (i, name) in self.layout_names.iter().enumerate() {
            let current = name.eq_ignore_ascii_case(&self.layout_name);
            let mut it = Item::new(
                Kind::Layout,
                name.clone(),
                if current { "Saved layout · current" } else { "Saved layout" },
                Target::SwitchLayout(name.clone()),
            )
            .keywords("layout workspace switch");
            it.hotkey = Action::ALL.into_iter().find(|a| a.layout_slot() == Some(i));
            v.push(it);
        }
        for name in self.theme_choices() {
            let mut sub = match crate::themes::find(&self.themes, &name) {
                _ if name.eq_ignore_ascii_case(crate::themes::SYSTEM) => "Follow the OS appearance".to_string(),
                Some(t) if t.dark => "Dark theme".into(),
                Some(_) => "Light theme".into(),
                None => "Theme".into(),
            };
            if name.eq_ignore_ascii_case(&self.prefs.appearance.theme) {
                sub.push_str(" · current");
            }
            v.push(Item::new(Kind::Theme, name.clone(), sub, Target::Theme(name)).keywords("theme appearance colours"));
        }
        for key in &self.watchlist {
            v.push(ticker_item(key).keywords("watchlist"));
        }
        v.extend(indicator_items(self.peek_chart().map_or(&[][..], |c| c.indicators.as_slice())));
        let q = self.palette.query.trim();
        let offered = |v: &[Item]| v.iter().any(|it| matches!(&it.target, Target::Run(l) if l.eq_ignore_ascii_case(q)));
        if !q.is_empty() {
            for l in tr_core::catalog::search(q, 12) {
                v.push(ticker_item(&l.key()));
            }
            for key in tr_core::perps::search(q, self.perp_candidates(), 8) {
                v.push(ticker_item(key));
            }
            // Whatever the command line would do with the query, offered as-is.
            match command::parse(q) {
                // An indicator with custom parameters (`SMA 200`, `BB 20 3`).
                Some(command::Command::Function(Function::Indicator(spec)))
                    if !offered(&v) && spec.def().is_some_and(|d| IndicatorSpec::of(d) != spec) =>
                {
                    let sub = format!("{} · toggle on the chart", spec.name());
                    v.push(Item::new(Kind::Indicator, spec.label(), sub, Target::Run(q.into())));
                }
                Some(command::Command::Symbol(key)) => {
                    v.push(ticker_item(&key));
                    let sec = tr_data::security(&key);
                    if matches!(sec.class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf) {
                        let t = &sec.ticker;
                        let mode = if self.demo_mode() { "demo" } else { "LIVE" };
                        v.push(
                            Item::new(
                                Kind::Command,
                                format!("Buy {t}"),
                                format!("Order ticket · {mode}"),
                                Target::Run(format!("BUY {key}")),
                            )
                            .keywords("trade order"),
                        );
                        v.push(
                            Item::new(
                                Kind::Command,
                                format!("Sell {t}"),
                                format!("Order ticket · {mode}"),
                                Target::Run(format!("SELL {key}")),
                            )
                            .keywords("trade order"),
                        );
                        let held = self.shown_portfolio().is_some_and(|pf| {
                            pf.positions.iter().any(|p| p.units > 0.0 && p.symbol.eq_ignore_ascii_case(t))
                        });
                        if held {
                            v.push(
                                Item::new(
                                    Kind::Command,
                                    format!("Sell all {t}"),
                                    format!("Close the whole position · {mode}"),
                                    Target::Run(format!("SELL ALL {key}")),
                                )
                                .keywords("close exit position"),
                            );
                        }
                    }
                }
                // Kept as typed: arguments such as paths and titles are case-sensitive.
                Some(_) if !offered(&v) => {
                    v.push(Item::new(Kind::Command, q, command::hint(q), Target::Run(q.into())));
                }
                Some(_) => {}
                None => {}
            }
        }
        v
    }

    /// Run a picked item.
    fn run_palette_target(&mut self, t: Target) {
        match t {
            Target::Run(line) => self.execute(&line),
            Target::Hotkey(a) => self.run_hotkey(a),
            Target::Settings(label) => {
                if let Some(tab) = SettingsTab::ALL.into_iter().find(|t| t.label() == label) {
                    self.settings_tab = tab;
                }
                self.apply(Function::Settings);
            }
            Target::SwitchLayout(name) => self.switch_layout(&name),
            Target::Theme(name) => {
                if !self.select_theme(&name) {
                    self.status = format!("No theme {name:?}");
                }
            }
        }
    }

    /// Keyboard handling, the overlay, and running the pick. Runs before the panes so
    /// arrows, Enter and Esc go to the palette while it is open.
    pub(super) fn command_palette(&mut self, ctx: &egui::Context) {
        let input_id = Id::new(INPUT_ID);
        if !self.palette.open {
            // Give the keyboard back once closed (by ⌘K, a pick or Esc).
            if ctx.memory(|m| m.has_focus(input_id)) {
                ctx.memory_mut(|m| m.surrender_focus(input_id));
            }
            return;
        }
        let (up, down, enter, esc) = ctx.input_mut(|i| {
            let up = i.consume_key(Modifiers::NONE, Key::ArrowUp) || i.consume_key(Modifiers::SHIFT, Key::Tab);
            let down = i.consume_key(Modifiers::NONE, Key::ArrowDown) || i.consume_key(Modifiers::NONE, Key::Tab);
            let enter = i.consume_key(Modifiers::NONE, Key::Enter);
            let esc = i.consume_key(Modifiers::NONE, Key::Escape);
            (up, down, enter, esc)
        });
        let keyed = up || down;
        let scoped = self.palette.scope.is_some();
        let screen = ctx.content_rect();
        let mut picked = None;
        let mut close = esc;
        // Opening fades the dim in and floats the card up into place.
        let shown = anim::ease_out(anim::appear(ctx, Id::new("command-palette-in"), 0.2));
        let results = egui::Area::new(Id::new("command-palette"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.multiply_opacity(shown);
                let (_, bg) = ui.allocate_exact_size(screen.size(), Sense::click());
                let w = (screen.width() - dp(40.0)).clamp(320.0, 660.0);
                let top = screen.top() + (screen.height() * 0.14).clamp(dp(16.0), dp(160.0)) + dp(10.0) * (1.0 - shown);
                let left = screen.center().x - w / 2.0;
                let pad = vec2(dp(10.0), dp(6.0));
                let field_h = dp(46.0);
                let footer_h = dp(28.0);
                let field = Rect::from_min_size(pos2(left, top) + pad, vec2(w - 2.0 * pad.x, field_h));

                // The query first, so results (and Enter) see this frame's typing.
                let edit = egui::TextEdit::singleline(&mut self.palette.query)
                    .id(input_id)
                    .font(ui_font(16.0))
                    .frame(egui::Frame::NONE)
                    .text_color(text())
                    .hint_text(if scoped {
                        "Search indicators by name, code or category…"
                    } else {
                        "Search commands, tickers, panels, layouts, themes, settings…"
                    })
                    .desired_width(field.width() - dp(36.0));
                let field_rect = Rect::from_min_max(pos2(field.left() + dp(28.0), field.top()), field.max);
                let resp = ui
                    .scope_builder(
                        UiBuilder::new().max_rect(field_rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| ui.add(edit),
                    )
                    .inner;
                if self.palette.just_opened || !resp.has_focus() {
                    resp.request_focus();
                    self.palette.just_opened = false;
                }
                if resp.changed() {
                    self.palette.selected = 0;
                }

                let items = self.palette_items();
                let results = self.palette.results(&items);
                let recents = self.palette.recent_count(&items);
                if up {
                    self.palette.step(-1, results.len());
                }
                if down {
                    self.palette.step(1, results.len());
                }
                self.palette.selected = self.palette.selected.min(results.len().saturating_sub(1));
                if enter {
                    picked = Some(self.palette.selected);
                }

                // Size the list to the results, but keep the card inside the window.
                let rows = results.len().clamp(1, VISIBLE_ROWS);
                let headers = if recents > 0 { 2.0 * dp(22.0) } else { 0.0 };
                let room = screen.bottom() - dp(12.0) - field.bottom() - footer_h - 2.0 * pad.y;
                let list_h =
                    (rows as f32 * dp(ROW_H) + headers).min(VISIBLE_ROWS as f32 * dp(ROW_H)).min(room).max(dp(ROW_H));
                let card =
                    Rect::from_min_size(pos2(left, top), vec2(w, field_h + list_h + footer_h + 2.0 * pad.y + dp(4.0)));
                if bg.clicked() && bg.interact_pointer_pos().is_some_and(|p| !card.contains(p)) {
                    close = true;
                }
                // Dim everything beneath, then the card, on a layer above the panes and
                // below this one.
                let mut under =
                    ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Middle, Id::new("command-palette-dim")));
                under.multiply_opacity(shown);
                let dark = theme::palette().dark;
                under.rect_filled(screen, 0.0, Color32::from_black_alpha(if dark { 120 } else { 70 }));
                widgets::card_on(&under, card, 1.0);

                let inner = card.shrink2(pad);
                widgets::draw_icon(
                    ui.painter(),
                    Rect::from_center_size(pos2(field.left() + dp(12.0), field.center().y), vec2(16.0, 16.0)),
                    widgets::Icon::Search,
                    muted(),
                );
                ui.painter().hline(inner.x_range(), field.bottom(), Stroke::new(1.0, theme::border()));

                let list =
                    Rect::from_min_size(pos2(inner.left(), field.bottom() + dp(4.0)), vec2(inner.width(), list_h));
                let mut list_ui = ui.new_child(UiBuilder::new().max_rect(list).id_salt("palette-list"));
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, true]).show(&mut list_ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let glider = widgets::Glider::new(ui, Id::new("palette-selection"));
                    if results.is_empty() {
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), dp(ROW_H)), Sense::hover());
                        ui.painter().text(
                            pos2(r.left() + dp(10.0), r.center().y),
                            egui::Align2::LEFT_CENTER,
                            "No matches. Enter runs it as a command if it parses.",
                            ui_font(13.0),
                            muted(),
                        );
                    }
                    for (i, it) in results.iter().enumerate() {
                        if recents > 0 && (i == 0 || i == recents) {
                            section_header(ui, if i == 0 { "Recent" } else { "All" });
                        }
                        if self.palette_row(ui, it, (i == self.palette.selected).then_some(&glider), keyed) {
                            picked = Some(i);
                        }
                    }
                });

                let foot = Rect::from_min_max(
                    pos2(inner.left(), inner.bottom() - footer_h),
                    pos2(inner.right(), inner.bottom()),
                );
                let p = ui.painter();
                p.hline(inner.x_range(), foot.top(), Stroke::new(1.0, theme::border()));
                p.text(
                    pos2(foot.left() + dp(4.0), foot.center().y),
                    egui::Align2::LEFT_CENTER,
                    if scoped {
                        "↑↓ select   ↵ add/remove   esc close"
                    } else {
                        "↑↓ select   ↵ open   esc close"
                    },
                    mono(11.0),
                    muted(),
                );
                p.text(
                    pos2(foot.right() - dp(4.0), foot.center().y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{} result{}", results.len(), if results.len() == 1 { "" } else { "s" }),
                    mono(11.0),
                    muted(),
                );
                results
            })
            .inner;

        if let Some(item) = picked.and_then(|i| results.get(i)).cloned() {
            // The indicator picker stays open to toggle several, and leaves recents alone
            // so rows do not reorder under the selection.
            if !scoped {
                self.palette.close();
                self.palette.record(&item);
            }
            self.run_palette_target(item.target);
        } else if picked.is_some() && !self.palette.query.trim().is_empty() {
            // Nothing matched: behave like the command line.
            let line = self.palette.query.trim().to_string();
            self.palette.close();
            self.execute(&line);
        } else if close {
            self.palette.close();
        }
        if !self.palette.open {
            ctx.memory_mut(|m| m.surrender_focus(input_id));
        }
    }

    /// One result row: kind badge, title, subtitle and shortcut caps. `selected` carries
    /// the selection highlight when this row has it. Returns whether it was clicked.
    fn palette_row(&self, ui: &mut Ui, it: &Item, selected: Option<&widgets::Glider>, keyed: bool) -> bool {
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), dp(ROW_H)), Sense::click());
        if let Some(glider) = selected {
            if keyed {
                resp.scroll_to_me(None);
            }
            glider.on(ui, r);
        }
        widgets::hover_wash(ui, r, &resp, false);
        let p = ui.painter();
        let cy = r.center().y;

        // Kind badge.
        let kc = kind_color(it.kind);
        let badge_g = p.layout_no_wrap(it.kind.label().to_string(), mono(9.5), kc);
        let badge = Rect::from_min_size(pos2(r.left() + dp(8.0), cy - 9.0), vec2(dp(64.0), 18.0));
        p.rect_filled(badge, 4.0, kc.gamma_multiply(0.14));
        p.rect_stroke(badge, 4.0, Stroke::new(1.0, kc.gamma_multiply(0.5)), StrokeKind::Inside);
        p.galley(pos2(badge.center().x - badge_g.size().x / 2.0, cy - badge_g.size().y / 2.0), badge_g, kc);

        // Shortcut hint on the right.
        let mut right = r.right() - dp(8.0);
        if let Some(chord) = it.hotkey.and_then(|a| self.prefs.hotkeys.chord(a)) {
            right -= super::hotkeys_view::paint_key_caps(p, right, cy, &chord.tokens()) + dp(10.0);
        }

        // Title then subtitle, clipped before the shortcut.
        let x = badge.right() + dp(12.0);
        let clip = Rect::from_min_max(pos2(x, r.top()), pos2(right.max(x), r.bottom()));
        let p = p.with_clip_rect(clip.intersect(p.clip_rect()));
        let title_font = if it.kind == Kind::Command { mono(13.0) } else { ui_font(13.5) };
        let title = p.layout_no_wrap(it.title.clone(), title_font, text());
        let tw = title.size().x;
        p.galley(pos2(x, cy - title.size().y / 2.0), title, text());
        if !it.subtitle.is_empty() {
            p.text(pos2(x + tw + dp(10.0), cy), egui::Align2::LEFT_CENTER, &it.subtitle, ui_font(12.0), muted());
        }
        resp.clicked()
    }
}

fn section_header(ui: &mut Ui, label: &str) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), dp(22.0)), Sense::hover());
    ui.painter().text(
        pos2(r.left() + dp(8.0), r.center().y + 2.0),
        egui::Align2::LEFT_CENTER,
        label.to_uppercase(),
        mono(10.0),
        muted(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::rank;

    #[test]
    fn every_palette_command_parses() {
        for &(code, ..) in COMMANDS {
            assert!(command::parse(code).is_some(), "{code} does not parse");
        }
        for it in static_items() {
            if let Target::Run(line) = &it.target {
                assert!(command::parse(line).is_some(), "{line} does not parse");
            }
        }
    }

    #[test]
    fn catalogue_covers_commands_panels_actions_and_settings() {
        let items = static_items();
        let has = |kind: Kind, title: &str| items.iter().any(|i| i.kind == kind && i.title == title);
        for code in ["PORT", "POS", "ORD", "OMON", "THEME NEXT", "CONG", "WEI", "GC", "HELP"] {
            assert!(has(Kind::Command, code), "{code}");
        }
        assert!(has(Kind::Panel, "New Options chain pane"));
        assert!(has(Kind::Action, "Move sidebar to the other side"));
        assert!(has(Kind::Action, "Open Brokerage settings"));
        assert!(has(Kind::Action, "Buy the current security") && has(Kind::Action, "Switch to live accounts"));
        assert!(has(Kind::Layout, "Grid preset 4"));
        for tab in SettingsTab::ALL {
            assert!(has(Kind::Settings, &format!("Settings › {}", tab.label())));
        }
        assert!(
            items.iter().all(|i| i.target != Target::Hotkey(Action::CommandPalette)),
            "the palette does not list itself"
        );
        // Function codes and their shortcuts.
        let pos = items.iter().find(|i| i.title == "POS").unwrap();
        assert_eq!(pos.hotkey, Some(Action::Positions));
        assert!(pos.subtitle.contains("positions"), "{}", pos.subtitle);
    }

    #[test]
    fn typical_queries_find_their_item() {
        let items = static_items();
        let top = |q: &str| rank(&items, &[], q).first().map(|i| i.title.clone()).unwrap_or_default();
        assert_eq!(top("omon"), "OMON");
        assert_eq!(top("port"), "PORT");
        assert_eq!(top("swap sidebar"), "Move sidebar to the other side");
        assert_eq!(top("hotkeys"), "Settings › Hotkeys");
        assert_eq!(top("theme"), "THEME NEXT");
        let best2 = |q: &str| rank(&items, &[], q).into_iter().take(2).map(|i| i.title).collect::<Vec<_>>();
        let b = best2("brokerage settings");
        assert!(b.contains(&"Open Brokerage settings".to_string()) && b.contains(&"Settings › Brokerage".to_string()));
        let b = best2("yield curve");
        assert!(b.contains(&"GC".to_string()), "{b:?}");
    }

    /// A perp's row is labelled as one and found by its underlying, after the listed stock.
    #[test]
    fn perp_rows_say_what_they_are() {
        let perp = ticker_item("xyz:AAPL");
        assert_eq!((perp.title.as_str(), perp.subtitle.as_str()), ("xyz:AAPL", "Apple Inc perp · PERP · HL xyz"));
        assert_eq!(perp.target, Target::Run("xyz:AAPL".into()));
        assert_eq!(command::parse("xyz:AAPL"), Some(command::Command::Symbol("xyz:AAPL".into())));
        assert_eq!(ticker_item("xyz:SKHX").subtitle, "PERP · HL xyz");
        let items = vec![ticker_item("AAPL US Equity"), perp];
        let titles = |q: &str| rank(&items, &[], q).into_iter().map(|i| i.title).collect::<Vec<_>>();
        assert_eq!(titles("aapl"), ["AAPL US Equity", "xyz:AAPL"], "the listed stock first");
        assert_eq!(titles("apple perp"), ["xyz:AAPL"]);
        assert_eq!(titles("hyperliquid"), ["xyz:AAPL"]);
        let perp_cmd = static_items().into_iter().find(|i| i.title == "PERP").expect("PERP is offered");
        assert_eq!(perp_cmd.hotkey, Some(Action::TogglePerp));
    }

    #[test]
    fn indicators_are_found_by_code_name_and_category() {
        let active = [IndicatorSpec::new("RSI", &[21.0]).unwrap()];
        let items: Vec<Item> = static_items().into_iter().chain(indicator_items(&active)).collect();
        let top = |q: &str| rank(&items, &[], q).first().map(|i| i.title.clone()).unwrap_or_default();
        assert_eq!(top("rsi"), "Relative Strength Index");
        assert_eq!(top("macd"), "Moving Average Convergence Divergence");
        assert_eq!(top("bollinger"), "Bollinger Bands");
        assert_eq!(top("psar"), "Parabolic SAR");
        assert_eq!(top("ichimoku"), "Ichimoku Cloud");
        assert_eq!(top("supertrend"), "SuperTrend");
        assert_eq!(top("on balance volume"), "On-Balance Volume");
        assert_eq!(top("ind"), "IND");
        let rsi = items.iter().find(|i| i.title == "Relative Strength Index").unwrap();
        assert_eq!(rsi.subtitle, "RSI 21 · Momentum · on chart");
        assert_eq!(rsi.target, Target::Run("RSI 21".into()), "picking an active indicator removes it");
        let n = indicator_items(&[]).len();
        assert_eq!(n, registry::all().len());
        let volume = rank(&items, &[], "volume").iter().filter(|i| i.kind == Kind::Indicator).count();
        assert!(volume >= 10, "category words match: {volume}");
        for it in indicator_items(&[]) {
            let Target::Run(line) = &it.target else { panic!() };
            assert!(matches!(command::parse(line), Some(command::Command::Function(Function::Indicator(_)))), "{line}");
        }
    }
}
