//! Brokerage portfolio panes (`PORT`, `ACCT`, `POS`, `ORD`, `ACTV`): setup screen, summary
//! with value/cash/P&L and allocation, accounts, positions, orders and activity tables.

use std::collections::HashMap;

use egui::{Align2, Color32, Rect, RichText, Sense, Ui, pos2, vec2};
use tr_broker::{Account, Portfolio, Position};
use tr_data::Quote;

use super::broker::{BrokerMode, BrokerState, PortfolioPane, PosSort};
use crate::command::PortView;
use crate::congress_view::Action;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;

/// What a portfolio pane asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum PortAction {
    Core(Action),
    Refresh,
    Connect,
    Setup,
    Open(&'static str),
    /// `OMON` for a market key.
    Options(String),
    News(String),
    /// Open the order ticket for a market key, optionally in an account with a quantity.
    Trade {
        key: String,
        buy: bool,
        account: Option<String>,
        units: Option<f64>,
    },
    /// Ask to cancel an open order (confirmed in a dialog for live accounts).
    CancelOrder {
        account: String,
        id: String,
    },
    /// Open Settings › Trading.
    TradingSettings,
    /// Sell a whole position (at once in the demo account; through the ticket live).
    ClosePosition {
        account: String,
        key: String,
    },
}

// ---- formatting ---------------------------------------------------------------------------

/// `1234567.891` → `1,234,567.89` (with `dp` decimals).
pub fn group(x: f64, dp: usize) -> String {
    let s = format!("{:.dp$}", x.abs());
    let (int, frac) = s.split_once('.').map_or((s.as_str(), None), |(a, b)| (a, Some(b)));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    if x < 0.0 && x.abs() >= 0.5 * 10f64.powi(-(dp as i32)) { format!("-{out}") } else { out }
}

pub fn money(x: f64, ccy: &str) -> String {
    match ccy {
        "" | "USD" => {
            let g = group(x, 2);
            match g.strip_prefix('-') {
                Some(rest) => format!("-${rest}"),
                None => format!("${g}"),
            }
        }
        c => format!("{} {c}", group(x, 2)),
    }
}

pub fn signed_money(x: f64, ccy: &str) -> String {
    let m = money(x, ccy);
    if m.starts_with('-') || x.abs() < 0.005 { m } else { format!("+{m}") }
}

/// Quantity with up to six decimals, trailing zeros trimmed.
pub fn qty(x: f64) -> String {
    if (x - x.round()).abs() < 1e-9 {
        return group(x, 0);
    }
    let s = group(x, 6);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn price(x: f64) -> String {
    group(x, if x.abs() >= 1.0 || x == 0.0 { 2 } else { 4 })
}

fn pct(x: f64) -> String {
    format!("{x:+.2}%")
}

fn pnl_color(x: f64) -> Color32 {
    if x > 0.0 {
        theme::up()
    } else if x < 0.0 {
        theme::down()
    } else {
        muted()
    }
}

// ---- tables -------------------------------------------------------------------------------

#[derive(Clone)]
struct Col {
    title: &'static str,
    /// Share of the row width (0 = hidden).
    w: f32,
    right: bool,
    sort: Option<PosSort>,
    /// Dropped first when the pane is narrow (higher goes first); 0 = always shown.
    prio: u8,
}

const fn col(title: &'static str, w: f32, right: bool) -> Col {
    Col { title, w, right, sort: None, prio: 0 }
}

fn opt(c: Col, prio: u8) -> Col {
    Col { prio, ..c }
}

/// Space between a right-aligned column and a left-aligned neighbour.
const RIGHT_GAP: f32 = 10.0;

/// Points each unit of column weight needs to stay legible.
const PX_PER_WEIGHT: f32 = 72.0;

/// Hide optional columns, least important first, until the rest fit `width`.
fn fit(mut cols: Vec<Col>, width: f32) -> Vec<Col> {
    loop {
        let need: f32 = cols.iter().map(|c| c.w * PX_PER_WEIGHT).sum();
        if need <= width {
            return cols;
        }
        match cols.iter_mut().filter(|c| c.w > 0.0 && c.prio > 0).max_by_key(|c| c.prio) {
            Some(c) => c.w = 0.0,
            None => return cols,
        }
    }
}

/// Left and right x of column `k` within `r`.
fn span(r: Rect, cols: &[Col], k: usize) -> (f32, f32) {
    let total: f32 = cols.iter().map(|c| c.w).sum();
    let inner = r.width() - 16.0;
    let start: f32 = cols[..k].iter().map(|c| c.w).sum();
    let l = r.left() + 8.0 + inner * start / total;
    (l, l + inner * cols[k].w / total)
}

fn cell(p: &egui::Painter, r: Rect, cols: &[Col], k: usize, s: impl ToString, font: egui::FontId, c: Color32) {
    if cols.get(k).is_none_or(|c| c.w <= 0.0) {
        return;
    }
    let (l, rt) = span(r, cols, k);
    let s = s.to_string();
    // Truncate left-aligned text that would spill into the next column.
    let galley = p.layout_no_wrap(s.clone(), font.clone(), c);
    let room = rt - l - 6.0;
    let text = if !cols[k].right && galley.size().x > room && room > 12.0 {
        let per = galley.size().x / s.chars().count().max(1) as f32;
        let keep = ((room / per) as usize).saturating_sub(1).max(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    } else {
        s
    };
    if cols[k].right {
        p.text(pos2(rt - RIGHT_GAP, r.center().y), Align2::RIGHT_CENTER, text, font, c);
    } else {
        p.text(pos2(l, r.center().y), Align2::LEFT_CENTER, text, font, c);
    }
}

/// Header row; returns a clicked sort key.
fn header(ui: &mut Ui, cols: &[Col], sort: Option<PosSort>) -> Option<PosSort> {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, dp(22.0)), Sense::hover());
    ui.painter().rect_filled(r, theme::CONTROL_RADIUS, theme::header());
    let mut clicked = None;
    for (k, c) in cols.iter().enumerate() {
        if c.w <= 0.0 {
            continue;
        }
        let (l, rt) = span(r, cols, k);
        let active = c.sort.is_some() && c.sort == sort;
        let color = if active { text() } else { muted() };
        let painter = ui.painter();
        let label = if c.right {
            painter.text(pos2(rt - RIGHT_GAP, r.center().y), Align2::RIGHT_CENTER, c.title, ui_font(11.5), color)
        } else {
            painter.text(pos2(l, r.center().y), Align2::LEFT_CENTER, c.title, ui_font(11.5), color)
        };
        if active {
            // A small downward triangle after the sorted column's title.
            let (x, y) = if c.right { (label.left() - 8.0, r.center().y) } else { (label.right() + 7.0, r.center().y) };
            let pts = vec![pos2(x - 3.5, y - 2.0), pos2(x + 3.5, y - 2.0), pos2(x, y + 2.5)];
            painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
        }
        if let Some(s) = c.sort {
            let hit = Rect::from_min_max(pos2(l, r.top()), pos2(rt, r.bottom()));
            if ui.interact(hit, ui.id().with(("sort", k)), Sense::click()).on_hover_text("Sort").clicked() {
                clicked = Some(s);
            }
        }
    }
    clicked
}

fn row(ui: &mut Ui) -> (Rect, egui::Response) {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, dp(22.0)), Sense::click());
    widgets::hover_wash(ui, r, &resp, false);
    (r, resp)
}

fn note(ui: &mut Ui, s: impl Into<String>) {
    ui.label(RichText::new(s.into()).font(ui_font(11.5)).color(muted()));
}

/// Buy/Sell items for a security row (the ticket says whether trading is enabled).
fn trade_items(ui: &mut Ui, key: &str, account: Option<&str>, held: Option<f64>, out: &mut Vec<PortAction>) {
    let sec = tr_data::security(key);
    if !matches!(sec.class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf) {
        return;
    }
    let account = account.map(str::to_string);
    if ui.button(format!("Buy {}…", sec.ticker)).clicked() {
        out.push(PortAction::Trade { key: key.to_string(), buy: true, account: account.clone(), units: None });
        ui.close();
    }
    let units = held.filter(|u| *u > 0.0);
    let sell = match units {
        Some(u) => format!("Sell {} {}…", qty(u), sec.ticker),
        None => format!("Sell {}…", sec.ticker),
    };
    if ui.button(sell).clicked() {
        out.push(PortAction::Trade { key: key.to_string(), buy: false, account, units });
        ui.close();
    }
    ui.separator();
}

/// Right-click menu for a security row.
fn security_menu(ui: &mut Ui, key: &str, symbol: &str, out: &mut Vec<PortAction>) {
    if ui.button("Open Chart").clicked() {
        out.push(PortAction::Core(Action::OpenChart(key.to_string())));
        ui.close();
    }
    let sec = tr_data::security(key);
    if matches!(sec.class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf)
        && ui.button("Option Chain (OMON)").clicked()
    {
        out.push(PortAction::Options(key.to_string()));
        ui.close();
    }
    if ui.button("News").clicked() {
        out.push(PortAction::News(key.to_string()));
        ui.close();
    }
    if sec.has_congress_trades() && ui.button("Who in Congress Traded This").clicked() {
        out.push(PortAction::Core(Action::OpenTicker(sec.ticker.clone())));
        ui.close();
    }
    if ui.button("Add to Watchlist").clicked() {
        out.push(PortAction::Core(Action::AddToWatchlist(key.to_string())));
        ui.close();
    }
    ui.separator();
    if ui.button(format!("Copy {symbol}")).clicked() {
        ui.ctx().copy_text(symbol.to_string());
        ui.close();
    }
}

// ---- pane ---------------------------------------------------------------------------------

/// A portfolio pane. `demo` is the demo account's portfolio when it is the selected account;
/// otherwise the pane shows the brokerage (live or sample) from `b`.
pub fn portfolio(
    ui: &mut Ui,
    pane: &mut PortfolioPane,
    b: &BrokerState,
    demo: Option<&Portfolio>,
    quotes: &HashMap<String, Quote>,
) -> Vec<PortAction> {
    let mut out = Vec::new();
    toolbar(ui, pane, b, demo, &mut out);
    ui.add_space(dp(4.0));
    let (mode, pf, error) = match demo {
        Some(d) => (BrokerMode::Simulated, Some(d), None),
        None => (b.mode(), b.portfolio.as_deref(), b.error.as_ref()),
    };
    match (mode, pf) {
        (BrokerMode::Setup, _) => setup_screen(ui, b, &mut out),
        (_, None) if b.loading => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new("Loading brokerage accounts…").color(muted()));
            });
        }
        (_, None) => match error {
            Some(e) => error_card(ui, e, &mut out),
            None => note(ui, "Waiting for the first sync…"),
        },
        (_, Some(pf)) if pf.accounts.is_empty() => {
            if let Some(e) = error {
                error_card(ui, e, &mut out);
            }
            empty_state(ui, &mut out);
        }
        (_, Some(pf)) => {
            if let Some(e) = error {
                ui.label(RichText::new(format!("Last refresh failed: {e}")).font(ui_font(11.5)).color(theme::down()));
            }
            if pane.account.as_ref().is_some_and(|a| pf.account(a).is_none()) {
                pane.account = None;
            }
            let price_of = |p: &Position| b.live_price(quotes, p);
            match pane.view {
                PortView::Summary => {
                    egui::ScrollArea::vertical()
                        .id_salt("port-summary")
                        .auto_shrink([false, false])
                        .show(ui, |ui| summary(ui, pane, pf, &price_of, &mut out));
                }
                PortView::Accounts => accounts(ui, pane, pf, &price_of),
                PortView::Positions => positions(ui, pane, pf, &price_of, &mut out),
                PortView::Orders => orders(ui, pane, pf, &mut out),
                PortView::Activity => activity(ui, pane, pf, &mut out),
            }
            if !pf.warnings.is_empty() {
                ui.add_space(dp(6.0));
                ui.collapsing(
                    RichText::new(format!("{} sync warning(s)", pf.warnings.len())).font(ui_font(11.5)).color(amber()),
                    |ui| {
                        for w in &pf.warnings {
                            note(ui, w);
                        }
                    },
                );
            }
        }
    }
    out
}

fn toolbar(
    ui: &mut Ui,
    pane: &mut PortfolioPane,
    b: &BrokerState,
    demo: Option<&Portfolio>,
    out: &mut Vec<PortAction>,
) {
    ui.horizontal(|ui| {
        let labels: Vec<&str> = PortView::ALL.iter().map(|v| v.label()).collect();
        let sel = PortView::ALL.iter().position(|v| *v == pane.view);
        if let Some(i) = widgets::segmented(ui, "port-view", &labels, sel, 22.0) {
            pane.view = PortView::ALL[i];
        }
        if demo.is_some() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, widgets::Icon::Gear, "Trading settings: demo account, live accounts", false)
                    .clicked()
                {
                    out.push(PortAction::TradingSettings);
                }
                ui.label(RichText::new("Paper trading · virtual cash").font(ui_font(11.0)).color(muted()));
                super::trading::mode_badge(ui, false)
                    .on_hover_text("The demo account is selected. Switch to live accounts from the toolbar badge.");
            });
            return;
        }
        if let Some(pf) = b.portfolio.as_deref()
            && pf.accounts.len() > 1
        {
            let current =
                pane.account.as_ref().and_then(|a| pf.account(a)).map_or("All accounts".to_string(), Account::label);
            egui::ComboBox::from_id_salt(ui.id().with("port-account")).selected_text(current).width(170.0).show_ui(
                ui,
                |ui| {
                    ui.selectable_value(&mut pane.account, None, "All accounts");
                    for a in &pf.accounts {
                        ui.selectable_value(
                            &mut pane.account,
                            Some(a.id.clone()),
                            format!("{} · {}", a.label(), a.institution),
                        );
                    }
                },
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, widgets::Icon::Gear, "Brokerage settings: keys, connections, test", false)
                .clicked()
            {
                out.push(PortAction::Setup);
            }
            if b.mode() == BrokerMode::Setup {
                return;
            }
            if ui.small_button("⟳").on_hover_text("Reload accounts (PORT REFRESH)").clicked() {
                out.push(PortAction::Refresh);
            }
            if b.mode() == BrokerMode::Live
                && ui.small_button("Connect…").on_hover_text("Connect another brokerage (read-only)").clicked()
            {
                out.push(PortAction::Connect);
            }
            if b.loading {
                ui.add(egui::Spinner::new().size(12.0));
            }
            if let Some(pf) = b.portfolio.as_deref() {
                if pf.simulated {
                    ui.label(RichText::new("SIMULATED").font(mono(10.5)).color(amber())).on_hover_text(
                        "Offline sample portfolio. Set up SnapTrade in Settings › Brokerage to see real accounts.",
                    );
                }
                if let Some(t) = pf.fetched {
                    let local = t.with_timezone(&chrono::Local);
                    ui.label(
                        RichText::new(format!("updated {}", local.format("%H:%M"))).font(ui_font(11.0)).color(muted()),
                    );
                }
            }
        });
    });
    // A security filter (`AAPL POS`) on its own line, so it never collides with the status.
    if let Some(s) = pane.symbol.clone() {
        ui.horizontal(|ui| {
            let t = tr_data::security(&s).ticker;
            note(ui, format!("Showing {t} only"));
            if ui.small_button("Show all").on_hover_text("Remove the security filter").clicked() {
                pane.symbol = None;
            }
        });
    }
}

fn setup_screen(ui: &mut Ui, b: &BrokerState, out: &mut Vec<PortAction>) {
    ui.add_space(dp(8.0));
    ui.label(RichText::new("Connect your brokerage accounts").font(ui_font(16.0)).color(text()));
    ui.add_space(dp(4.0));
    note(
        ui,
        "Terminal reads accounts, balances, positions, orders and activity through SnapTrade. Access is read-only \
         unless you turn on trading in Settings › Trading.",
    );
    ui.add_space(dp(8.0));
    let step = |ui: &mut Ui, n: &str, title: &str, body: &str, done: bool| {
        ui.horizontal(|ui| {
            // Completed steps keep their number, in green.
            let mark = n;
            let color = if done { theme::up() } else { theme::accent() };
            ui.label(RichText::new(mark).font(mono(13.0)).color(color));
            ui.vertical(|ui| {
                ui.label(RichText::new(title).font(ui_font(13.0)).color(text()));
                note(ui, body);
            });
        });
        ui.add_space(dp(4.0));
    };
    let c = &b.creds;
    step(
        ui,
        "1",
        "Get SnapTrade API keys",
        "Create a client ID and consumer key in the SnapTrade dashboard (personal keys work too).",
        c.has_keys(),
    );
    step(
        ui,
        "2",
        "Enter them in Brokerage Settings and Save",
        "Stored in the macOS Keychain (or an owner-only file on Linux), never in the repository.",
        c.has_keys(),
    );
    step(
        ui,
        "3",
        "Register User, then Connect Brokerage…",
        "Both buttons are in Brokerage Settings; the second opens SnapTrade's Connection Portal in your browser.",
        c.has_user(),
    );
    ui.add_space(dp(6.0));
    ui.horizontal(|ui| {
        if ui.button(RichText::new("Open Brokerage Settings…").strong()).clicked() {
            out.push(PortAction::Setup);
        }
        if ui.button("Get API Keys").on_hover_text(tr_broker::snaptrade::DASHBOARD_URL).clicked() {
            out.push(PortAction::Open(tr_broker::snaptrade::DASHBOARD_URL));
        }
        if ui.button("Documentation").on_hover_text(tr_broker::snaptrade::DOCS_URL).clicked() {
            out.push(PortAction::Open(tr_broker::snaptrade::DOCS_URL));
        }
    });
    ui.add_space(dp(8.0));
    note(
        ui,
        "Or set SNAPTRADE_CLIENT_ID, SNAPTRADE_CONSUMER_KEY, SNAPTRADE_USER_ID and SNAPTRADE_USER_SECRET. TR_BROKER=sample shows a simulated portfolio.",
    );
}

fn error_card(ui: &mut Ui, e: &str, out: &mut Vec<PortAction>) {
    ui.label(RichText::new("Could not load brokerage accounts").font(ui_font(13.5)).color(theme::down()));
    note(ui, e);
    ui.horizontal(|ui| {
        if ui.button("Retry").clicked() {
            out.push(PortAction::Refresh);
        }
        if ui.button("Brokerage Settings…").clicked() {
            out.push(PortAction::Setup);
        }
    });
}

fn empty_state(ui: &mut Ui, out: &mut Vec<PortAction>) {
    ui.add_space(dp(8.0));
    ui.label(RichText::new("No brokerage connected yet").font(ui_font(14.0)).color(text()));
    note(
        ui,
        "Connect a broker through SnapTrade's Connection Portal; accounts appear here a few moments after you finish.",
    );
    ui.add_space(dp(4.0));
    ui.horizontal(|ui| {
        if ui.button(RichText::new("Connect Brokerage…").strong()).clicked() {
            out.push(PortAction::Connect);
        }
        if ui.button("Brokerage Settings…").clicked() {
            out.push(PortAction::Setup);
        }
    });
}

/// Account filter of a pane, detached from it so the pane can still be changed.
fn selected(pane: &PortfolioPane) -> impl Fn(&str) -> bool + use<> {
    let account = pane.account.clone();
    move |id: &str| account.as_deref().is_none_or(|a| a == id)
}

fn matches_symbol(pane: &PortfolioPane, key: Option<String>) -> bool {
    pane.symbol.is_none() || key == pane.symbol
}

fn currency(pf: &Portfolio, pane: &PortfolioPane) -> String {
    pf.accounts.iter().find(|a| selected(pane)(&a.id)).map_or("USD".into(), |a| a.currency.clone())
}

fn summary(
    ui: &mut Ui,
    pane: &mut PortfolioPane,
    pf: &Portfolio,
    price_of: &dyn Fn(&Position) -> Option<f64>,
    out: &mut Vec<PortAction>,
) {
    let t = pf.totals(pane.account.as_deref(), price_of);
    let ccy = currency(pf, pane);
    // Headline figures, laid out as a grid that wraps to the pane width.
    let pnl = match t.pnl_pct() {
        Some(p) => format!("{} ({})", signed_money(t.pnl, &ccy), pct(p)),
        None => signed_money(t.pnl, &ccy),
    };
    let stats = [
        ("Total value", money(t.value, &ccy), text()),
        ("Unrealised P&L", pnl, pnl_color(t.pnl)),
        ("Invested", money(t.invested, &ccy), text()),
        ("Cash", money(t.cash, &ccy), text()),
        ("Buying power", money(t.buying_power, &ccy), text()),
    ];
    let galleys: Vec<_> = stats
        .iter()
        .map(|(label, value, color)| {
            let p = ui.painter();
            (
                p.layout_no_wrap((*label).into(), ui_font(11.0), muted()),
                p.layout_no_wrap(value.clone(), mono(16.0), *color),
            )
        })
        .collect();
    let gap = dp(22.0);
    let width = ui.available_width();
    let cell_h = galleys.iter().map(|(l, v)| l.size().y + v.size().y + 2.0).fold(0.0, f32::max);
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, (l, v)) in galleys.iter().enumerate() {
        let w = l.size().x.max(v.size().x);
        if x > 0.0 && x + w > width {
            rows.push(Vec::new());
            x = 0.0;
        }
        rows.last_mut().expect("row").push(i);
        x += w + gap;
    }
    let (area, _) = ui.allocate_exact_size(vec2(width, rows.len() as f32 * (cell_h + dp(6.0))), Sense::hover());
    let p = ui.painter();
    for (r, row) in rows.iter().enumerate() {
        let mut x = area.left();
        let y = area.top() + r as f32 * (cell_h + dp(6.0));
        for &i in row {
            let (l, v) = &galleys[i];
            p.galley(pos2(x, y), l.clone(), muted());
            p.galley(pos2(x, y + l.size().y + 2.0), v.clone(), text());
            x += l.size().x.max(v.size().x) + gap;
        }
    }
    ui.add_space(dp(6.0));

    // Allocation by holding.
    let sel = selected(pane);
    let mut held: Vec<(&Position, f64)> = pf
        .positions
        .iter()
        .filter(|p| sel(&p.account_id))
        .filter_map(|p| Some((p, p.market_value_at(price_of(p))?.abs())))
        .filter(|(_, v)| *v > 0.0)
        .collect();
    held.sort_by(|a, b| b.1.total_cmp(&a.1));
    let gross: f64 = held.iter().map(|(_, v)| v).sum::<f64>() + t.cash.max(0.0);
    if gross > 0.0 {
        let w = ui.available_width();
        let (bar, _) = ui.allocate_exact_size(vec2(w, dp(10.0)), Sense::hover());
        let p = ui.painter();
        p.rect_filled(bar, 3.0, theme::control());
        let mut x = bar.left();
        let mut legend = Vec::new();
        for (i, (pos, v)) in held.iter().take(8).enumerate() {
            let wseg = (*v / gross) as f32 * bar.width();
            let c = theme::series(i);
            p.rect_filled(Rect::from_min_max(pos2(x, bar.top()), pos2(x + wseg, bar.bottom())), 0.0, c);
            x += wseg;
            legend.push((pos.short_label(), *v / gross * 100.0, c));
        }
        let rest: f64 = held.iter().skip(8).map(|(_, v)| v).sum();
        if rest > 0.0 {
            let wseg = (rest / gross) as f32 * bar.width();
            p.rect_filled(Rect::from_min_max(pos2(x, bar.top()), pos2(x + wseg, bar.bottom())), 0.0, muted());
            legend.push(("Other".into(), rest / gross * 100.0, muted()));
        }
        if t.cash > 0.0 {
            legend.push(("Cash".into(), t.cash / gross * 100.0, theme::control_hover()));
        }
        ui.horizontal_wrapped(|ui| {
            for (name, share, c) in legend {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, c);
                ui.label(RichText::new(format!("{name} {share:.1}%")).font(ui_font(11.0)).color(muted()));
                ui.add_space(dp(6.0));
            }
        });
    }
    ui.add_space(dp(6.0));

    // Top holdings and accounts side by side when there is room.
    ui.label(RichText::new("Top holdings").font(ui_font(12.5)).color(text()));
    let cols = fit(
        vec![
            col("Symbol", 1.0, false),
            opt(col("Qty", 0.9, true), 2),
            col("Last", 1.1, true),
            col("Value", 1.3, true),
            col("P&L", 1.3, true),
            opt(col("%", 0.9, true), 1),
        ],
        ui.available_width(),
    );
    header(ui, &cols, None);
    for (p, _) in held.iter().take(8) {
        position_row(ui, &cols, p, price_of(p), out, |ui, r, cols, p, live| {
            let painter = ui.painter();
            cell(painter, r, cols, 0, p.short_label(), mono(12.0), amber());
            cell(painter, r, cols, 1, qty(p.units), mono(12.0), text());
            cell(painter, r, cols, 2, live.or(p.price).map_or("—".into(), price), mono(12.0), text());
            cell(painter, r, cols, 3, p.market_value_at(live).map_or("—".into(), |v| group(v, 2)), mono(12.0), text());
            let pnl = p.pnl_at(live);
            cell(
                painter,
                r,
                cols,
                4,
                pnl.map_or("—".into(), |v| signed_money(v, "").replace('$', "")),
                mono(12.0),
                pnl.map_or(muted(), pnl_color),
            );
            let pc = p.pnl_pct_at(live);
            cell(painter, r, cols, 5, pc.map_or("—".into(), pct), mono(12.0), pc.map_or(muted(), pnl_color));
        });
    }
    if held.len() > 8 && ui.small_button(format!("All {} positions ›", held.len())).clicked() {
        pane.view = PortView::Positions;
    }
    let open = pf.orders.iter().filter(|o| sel(&o.account_id) && o.is_open()).count();
    if open > 0 && ui.small_button(format!("{open} open order{} ›", if open == 1 { "" } else { "s" })).clicked() {
        pane.view = PortView::Orders;
        pane.open_only = true;
    }
    if pane.account.is_none() && pf.accounts.len() > 1 {
        ui.add_space(dp(6.0));
        ui.label(RichText::new("Accounts").font(ui_font(12.5)).color(text()));
        accounts(ui, pane, pf, price_of);
    }
}

/// One clickable position row: click charts the security, right-click offers more.
fn position_row(
    ui: &mut Ui,
    cols: &[Col],
    p: &Position,
    live: Option<f64>,
    out: &mut Vec<PortAction>,
    draw: impl FnOnce(&mut Ui, Rect, &[Col], &Position, Option<f64>),
) {
    let (r, resp) = row(ui);
    draw(ui, r, cols, p, live);
    let key = p.market_key();
    let resp = match (&key, live) {
        (Some(k), Some(_)) => resp.on_hover_text(format!("{k} · live terminal quote · click to chart")),
        (Some(k), None) => resp.on_hover_text(format!("{k} · click to chart")),
        (None, _) => resp.on_hover_text("Not available as a terminal market key"),
    };
    if let Some(k) = &key {
        let tradable = matches!(p.kind, tr_broker::InstrumentKind::Stock | tr_broker::InstrumentKind::Etf)
            && matches!(tr_data::security(k).class, tr_core::AssetClass::Equity | tr_core::AssetClass::Etf);
        let mut pills = Vec::new();
        if tradable {
            pills.extend(widgets::buy_sell_pills());
            if p.units > 0.0 {
                pills.push(("Close", theme::control(), text()));
            }
        }
        let tips = ["Buy more (⇧B)", "Sell (⇧S)", "Close: sell the whole position"];
        let pill = widgets::row_pills(ui, r, ("pos-pills", &p.account_id, &p.symbol), &pills, &tips);
        match pill {
            Some(0) => out.push(PortAction::Trade {
                key: k.clone(),
                buy: true,
                account: Some(p.account_id.clone()),
                units: None,
            }),
            Some(1) => out.push(PortAction::Trade {
                key: k.clone(),
                buy: false,
                account: Some(p.account_id.clone()),
                units: None,
            }),
            Some(_) => out.push(PortAction::ClosePosition { account: p.account_id.clone(), key: k.clone() }),
            None if resp.clicked() => out.push(PortAction::Core(Action::OpenChart(k.clone()))),
            None => {}
        }
        resp.context_menu(|ui| {
            if p.kind != tr_broker::InstrumentKind::Option {
                trade_items(ui, k, Some(&p.account_id), Some(p.units), out);
                if tradable && p.units > 0.0 {
                    if ui.button(format!("Close Position ({} {})…", qty(p.units), p.short_label())).clicked() {
                        out.push(PortAction::ClosePosition { account: p.account_id.clone(), key: k.clone() });
                        ui.close();
                    }
                    ui.separator();
                }
            }
            security_menu(ui, k, &p.symbol, out)
        });
    }
}

fn accounts(ui: &mut Ui, pane: &mut PortfolioPane, pf: &Portfolio, price_of: &dyn Fn(&Position) -> Option<f64>) {
    let cols = fit(
        vec![
            col("Account", 1.6, false),
            opt(col("Institution", 1.1, false), 2),
            opt(col("Number", 1.0, false), 4),
            opt(col("Type", 0.8, false), 3),
            col("Value", 1.3, true),
            col("Cash", 1.1, true),
            opt(col("Buying power", 1.2, true), 1),
            col("P&L", 1.2, true),
            opt(col("Synced", 0.9, true), 5),
        ],
        ui.available_width(),
    );
    header(ui, &cols, None);
    let mut pick = None;
    for a in &pf.accounts {
        let (r, resp) = row(ui);
        let p = ui.painter();
        let t = pf.totals(Some(&a.id), price_of);
        let label = if a.is_paper { format!("{} (paper)", a.label()) } else { a.label() };
        cell(p, r, &cols, 0, label, ui_font(12.5), amber());
        cell(p, r, &cols, 1, &a.institution, ui_font(12.0), text());
        cell(p, r, &cols, 2, a.masked_number(), mono(11.5), muted());
        cell(p, r, &cols, 3, &a.kind, ui_font(12.0), muted());
        let value = a.total.or((t.value > 0.0).then_some(t.value));
        cell(p, r, &cols, 4, value.map_or("—".into(), |v| money(v, &a.currency)), mono(12.0), text());
        cell(p, r, &cols, 5, a.cash.map_or("—".into(), |v| group(v, 2)), mono(12.0), text());
        cell(p, r, &cols, 6, a.buying_power.map_or("—".into(), |v| group(v, 2)), mono(12.0), text());
        cell(p, r, &cols, 7, signed_money(t.pnl, "").replace('$', ""), mono(12.0), pnl_color(t.pnl));
        let synced =
            a.synced.map_or("—".into(), |s| s.with_timezone(&chrono::Local).format("%d %b %H:%M").to_string());
        cell(p, r, &cols, 8, synced, ui_font(11.0), muted());
        if resp.on_hover_text("Click to show this account's positions").clicked() {
            pick = Some(a.id.clone());
        }
    }
    if let Some(id) = pick {
        pane.account = Some(id);
        pane.view = PortView::Positions;
    }
}

fn positions(
    ui: &mut Ui,
    pane: &mut PortfolioPane,
    pf: &Portfolio,
    price_of: &dyn Fn(&Position) -> Option<f64>,
    out: &mut Vec<PortAction>,
) {
    let sel = selected(pane);
    let mut list: Vec<(&Position, Option<f64>)> = pf
        .positions
        .iter()
        .filter(|p| sel(&p.account_id) && matches_symbol(pane, p.market_key()))
        .map(|p| (p, price_of(p)))
        .collect();
    if list.is_empty() {
        note(ui, if pane.symbol.is_some() { "No position in this security." } else { "No positions." });
        return;
    }
    let value = |(p, live): &(&Position, Option<f64>)| p.market_value_at(*live).unwrap_or(0.0);
    match pane.sort {
        PosSort::Value => list.sort_by(|a, b| value(b).abs().total_cmp(&value(a).abs())),
        PosSort::Pnl => {
            list.sort_by(|a, b| b.0.pnl_at(b.1).unwrap_or(f64::MIN).total_cmp(&a.0.pnl_at(a.1).unwrap_or(f64::MIN)))
        }
        PosSort::PnlPct => list.sort_by(|a, b| {
            b.0.pnl_pct_at(b.1).unwrap_or(f64::MIN).total_cmp(&a.0.pnl_pct_at(a.1).unwrap_or(f64::MIN))
        }),
        PosSort::Symbol => list.sort_by(|a, b| a.0.symbol.cmp(&b.0.symbol)),
    }
    let gross: f64 = list.iter().map(|x| value(x).abs()).sum();
    let all = pane.account.is_none() && pf.accounts.len() > 1;
    let mut cols = vec![
        Col { sort: Some(PosSort::Symbol), ..col("Symbol", 1.0, false) },
        opt(col("Description", 2.0, false), 3),
        col("Qty", 0.9, true),
        col("Last", 1.1, true),
        opt(col("Avg cost", 1.1, true), 2),
        Col { sort: Some(PosSort::Value), ..col("Value", 1.3, true) },
        Col { sort: Some(PosSort::Pnl), ..col("P&L", 1.3, true) },
        Col { sort: Some(PosSort::PnlPct), ..col("P&L %", 1.0, true) },
        opt(col("Weight", 0.8, true), 4),
    ];
    if all {
        cols.push(opt(col("Account", 1.3, false), 5));
    }
    let cols = fit(cols, ui.available_width());
    if let Some(s) = header(ui, &cols, Some(pane.sort)) {
        pane.sort = s;
    }
    let ccy = currency(pf, pane);
    egui::ScrollArea::vertical()
        .id_salt("port-positions")
        .auto_shrink([false, true])
        .max_height(ui.available_height() - dp(40.0))
        .show(ui, |ui| {
            for (p, live) in &list {
                let share = (gross > 0.0).then(|| value(&(p, *live)).abs() / gross * 100.0);
                let account = pf.account(&p.account_id).map(Account::label).unwrap_or_default();
                position_row(ui, &cols, p, *live, out, |ui, r, cols, p, live| {
                    let painter = ui.painter();
                    cell(painter, r, cols, 0, p.short_label(), mono(12.0), amber());
                    let desc =
                        if p.description.is_empty() { p.kind.label().to_string() } else { p.description.clone() };
                    cell(painter, r, cols, 1, desc, ui_font(12.0), if p.cash_equivalent { muted() } else { text() });
                    cell(
                        painter,
                        r,
                        cols,
                        2,
                        qty(p.units),
                        mono(12.0),
                        if p.units < 0.0 { theme::down() } else { text() },
                    );
                    let last = live.or(p.price);
                    let last_color = if live.is_some() { theme::cyan() } else { text() };
                    cell(painter, r, cols, 3, last.map_or("—".into(), price), mono(12.0), last_color);
                    cell(painter, r, cols, 4, p.cost_basis.map_or("—".into(), price), mono(12.0), muted());
                    cell(
                        painter,
                        r,
                        cols,
                        5,
                        p.market_value_at(live).map_or("—".into(), |v| group(v, 2)),
                        mono(12.0),
                        text(),
                    );
                    let pnl = p.pnl_at(live);
                    cell(
                        painter,
                        r,
                        cols,
                        6,
                        pnl.map_or("—".into(), |v| signed_money(v, "").replace('$', "")),
                        mono(12.0),
                        pnl.map_or(muted(), pnl_color),
                    );
                    let pc = p.pnl_pct_at(live);
                    cell(painter, r, cols, 7, pc.map_or("—".into(), pct), mono(12.0), pc.map_or(muted(), pnl_color));
                    cell(painter, r, cols, 8, share.map_or("—".into(), |s| format!("{s:.1}%")), mono(12.0), muted());
                    cell(painter, r, cols, 9, &account, ui_font(11.5), muted());
                });
            }
        });
    // Footer totals over the rows shown (respecting account and security filters).
    let shown_value: f64 = list.iter().filter(|(p, _)| !p.cash_equivalent).map(value).sum();
    let shown_pnl: f64 = list.iter().filter_map(|(p, live)| p.pnl_at(*live)).sum();
    ui.horizontal(|ui| {
        note(
            ui,
            format!(
                "{} position{} · value {} · P&L ",
                list.len(),
                if list.len() == 1 { "" } else { "s" },
                money(shown_value, &ccy)
            ),
        );
        ui.label(RichText::new(signed_money(shown_pnl, &ccy)).font(ui_font(11.5)).color(pnl_color(shown_pnl)));
        note(ui, "· cyan prices are live terminal quotes · click a row to chart");
    });
}

fn orders(ui: &mut Ui, pane: &mut PortfolioPane, pf: &Portfolio, out: &mut Vec<PortAction>) {
    ui.horizontal(|ui| {
        let sel = Some(usize::from(!pane.open_only));
        if let Some(i) = widgets::segmented(ui, "port-orders", &["Open", "All"], sel, 20.0) {
            pane.open_only = i == 0;
        }
        if pf.demo {
            note(ui, "Demo orders · hover an open order to cancel it");
        } else {
            note(
                ui,
                format!(
                    "Last {} days · hover or right-click an open order to cancel it",
                    tr_broker::snaptrade::ORDER_DAYS
                ),
            );
        }
    });
    let sel = selected(pane);
    let list: Vec<_> = pf
        .orders
        .iter()
        .filter(|o| sel(&o.account_id) && (!pane.open_only || o.is_open()) && matches_symbol(pane, o.market_key()))
        .collect();
    if list.is_empty() {
        note(ui, if pane.open_only { "No open orders." } else { "No orders in this period." });
        return;
    }
    let all = pane.account.is_none() && pf.accounts.len() > 1;
    let mut cols = vec![
        col("Placed", 1.2, false),
        col("Symbol", 1.0, false),
        col("Side", 1.15, false),
        opt(col("Type", 0.8, false), 3),
        col("Filled / Qty", 1.1, true),
        col("Limit", 0.9, true),
        opt(col("Stop", 0.8, true), 5),
        opt(col("Avg fill", 0.9, true), 2),
        col("Status", 1.1, false),
        opt(col("TIF", 0.5, false), 4),
    ];
    if all {
        cols.push(opt(col("Account", 1.2, false), 6));
    }
    let cols = fit(cols, ui.available_width());
    header(ui, &cols, None);
    egui::ScrollArea::vertical().id_salt("port-orders").auto_shrink([false, true]).show(ui, |ui| {
        for o in list {
            let (r, resp) = row(ui);
            let p = ui.painter();
            let placed =
                o.placed.map_or("—".into(), |t| t.with_timezone(&chrono::Local).format("%d %b %H:%M").to_string());
            cell(p, r, &cols, 0, placed, mono(11.5), muted());
            cell(p, r, &cols, 1, o.short_label(), mono(12.0), amber());
            cell(
                p,
                r,
                &cols,
                2,
                o.action.replace('_', " "),
                mono(11.5),
                if o.is_buy() { theme::up() } else { theme::down() },
            );
            cell(p, r, &cols, 3, &o.order_type, ui_font(12.0), text());
            let fq = format!("{} / {}", o.filled.map_or("0".into(), qty), o.quantity.map_or("—".into(), qty));
            cell(p, r, &cols, 4, fq, mono(12.0), text());
            cell(p, r, &cols, 5, o.limit_price.map_or("—".into(), price), mono(12.0), text());
            cell(p, r, &cols, 6, o.stop_price.map_or("—".into(), price), mono(12.0), text());
            cell(p, r, &cols, 7, o.execution_price.map_or("—".into(), price), mono(12.0), text());
            let status_color = match o.status.as_str() {
                "EXECUTED" => theme::up(),
                s if o.is_open() || s == "PARTIAL" => theme::cyan(),
                "REJECTED" | "FAILED" => theme::down(),
                _ => muted(),
            };
            cell(p, r, &cols, 8, o.status.replace('_', " "), mono(11.5), status_color);
            cell(p, r, &cols, 9, &o.time_in_force, ui_font(11.5), muted());
            if all {
                cell(
                    p,
                    r,
                    &cols,
                    10,
                    pf.account(&o.account_id).map(Account::label).unwrap_or_default(),
                    ui_font(11.5),
                    muted(),
                );
            }
            let key = o.market_key();
            let cancellable = o.is_open() && !o.id.is_empty();
            let pills: &[widgets::Pill<'_>] =
                if cancellable { &[("Cancel", theme::down(), Color32::WHITE)] } else { &[] };
            if widgets::row_pills(ui, r, ("ord-pill", &o.account_id, &o.id), pills, &["Cancel this order"]).is_some() {
                out.push(PortAction::CancelOrder { account: o.account_id.clone(), id: o.id.clone() });
            } else if let Some(k) = &key
                && resp.clicked()
            {
                out.push(PortAction::Core(Action::OpenChart(k.clone())));
            }
            if cancellable || key.is_some() {
                resp.context_menu(|ui| {
                    if cancellable {
                        if ui.button(RichText::new("Cancel Order…").color(theme::down())).clicked() {
                            out.push(PortAction::CancelOrder { account: o.account_id.clone(), id: o.id.clone() });
                            ui.close();
                        }
                        ui.separator();
                    }
                    if let Some(k) = &key {
                        security_menu(ui, k, &o.symbol, out);
                    }
                });
            }
        }
    });
}

fn activity(ui: &mut Ui, pane: &mut PortfolioPane, pf: &Portfolio, out: &mut Vec<PortAction>) {
    note(
        ui,
        format!("Last {} days: trades, dividends, interest, transfers and fees", tr_broker::snaptrade::ACTIVITY_DAYS),
    );
    let sel = selected(pane);
    let list: Vec<_> =
        pf.activities.iter().filter(|a| sel(&a.account_id) && matches_symbol(pane, a.market_key())).collect();
    if list.is_empty() {
        note(ui, "No activity in this period.");
        return;
    }
    let all = pane.account.is_none() && pf.accounts.len() > 1;
    let mut cols = vec![
        col("Date", 1.0, false),
        col("Type", 1.1, false),
        col("Symbol", 0.9, false),
        opt(col("Description", 2.4, false), 3),
        opt(col("Units", 0.9, true), 2),
        opt(col("Price", 1.2, true), 1),
        col("Amount", 1.3, true),
        opt(col("Fee", 0.6, true), 4),
    ];
    if all {
        cols.push(opt(col("Account", 1.2, false), 5));
    }
    let cols = fit(cols, ui.available_width());
    header(ui, &cols, None);
    egui::ScrollArea::vertical().id_salt("port-activity").auto_shrink([false, true]).show(ui, |ui| {
        for a in list {
            let (r, resp) = row(ui);
            let p = ui.painter();
            let date = a.trade_date.map_or("—".into(), |t| t.format("%d %b %y").to_string());
            cell(p, r, &cols, 0, date, mono(11.5), muted());
            let kind_color = match a.kind.as_str() {
                "BUY" => theme::up(),
                "SELL" => theme::down(),
                "DIVIDEND" | "INTEREST" | "REI" => theme::cyan(),
                _ => text(),
            };
            cell(p, r, &cols, 1, a.kind.replace('_', " "), mono(11.5), kind_color);
            cell(p, r, &cols, 2, a.symbol.as_deref().unwrap_or("—"), mono(12.0), amber());
            cell(p, r, &cols, 3, &a.description, ui_font(12.0), text());
            cell(p, r, &cols, 4, a.units.filter(|u| *u != 0.0).map_or("—".into(), qty), mono(12.0), text());
            cell(p, r, &cols, 5, a.price.filter(|u| *u != 0.0).map_or("—".into(), price), mono(12.0), text());
            let amt = a.amount.unwrap_or(0.0);
            cell(
                p,
                r,
                &cols,
                6,
                a.amount.map_or("—".into(), |v| signed_money(v, &a.currency)),
                mono(12.0),
                pnl_color(amt),
            );
            cell(p, r, &cols, 7, a.fee.filter(|f| *f != 0.0).map_or("—".into(), |f| group(f, 2)), mono(12.0), muted());
            if all {
                cell(
                    p,
                    r,
                    &cols,
                    8,
                    pf.account(&a.account_id).map(Account::label).unwrap_or_default(),
                    ui_font(11.5),
                    muted(),
                );
            }
            if let Some(k) = a.market_key() {
                if resp.clicked() {
                    out.push(PortAction::Core(Action::OpenChart(k.clone())));
                }
                let sym = a.symbol.clone().unwrap_or_default();
                resp.context_menu(|ui| security_menu(ui, &k, &sym, out));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_panes_drop_optional_columns_first() {
        let cols =
            vec![col("A", 1.0, false), opt(col("B", 1.0, true), 2), opt(col("C", 1.0, true), 1), col("D", 1.0, true)];
        let shown = |cols: &[Col]| cols.iter().map(|c| c.w > 0.0).collect::<Vec<_>>();
        assert_eq!(shown(&fit(cols.clone(), 4.0 * PX_PER_WEIGHT)), [true; 4]);
        assert_eq!(
            shown(&fit(cols.clone(), 3.0 * PX_PER_WEIGHT)),
            [true, false, true, true],
            "highest number goes first"
        );
        assert_eq!(shown(&fit(cols, 10.0)), [true, false, false, true], "required columns stay");
    }

    #[test]
    fn number_formats() {
        assert_eq!(group(1_234_567.891, 2), "1,234,567.89");
        assert_eq!(group(-999.999, 2), "-1,000.00");
        assert_eq!(group(-0.001, 2), "0.00", "no negative zero");
        assert_eq!(group(12.0, 0), "12");
        assert_eq!(money(31_884.62, "USD"), "$31,884.62");
        assert_eq!(money(-5.5, "USD"), "-$5.50");
        assert_eq!(money(1250.0, "CAD"), "1,250.00 CAD");
        assert_eq!(signed_money(300.0, "USD"), "+$300.00");
        assert_eq!(signed_money(-300.0, "USD"), "-$300.00");
        assert_eq!(signed_money(0.0, "USD"), "$0.00");
        assert_eq!(qty(50.0), "50");
        assert_eq!(qty(0.105), "0.105");
        assert_eq!(qty(-1.0), "-1");
        assert_eq!(qty(12_000.5), "12,000.5");
        assert_eq!(price(339.51), "339.51");
        assert_eq!(price(0.1234), "0.1234");
    }

    #[test]
    fn renders_every_view_without_panicking() {
        let ctx = egui::Context::default();
        // A store that holds nothing, so the test never reads (or moves) the user's Keychain item.
        let none = std::env::temp_dir().join(format!("tr-portfolio-test-{}/snaptrade.json", std::process::id()));
        let mut b = BrokerState::with_store(tr_broker::SecretStore::file(none), true);
        b.creds = tr_broker::Credentials::default();
        b.portfolio = Some(Box::new(tr_broker::sample::portfolio()));
        let quotes = HashMap::new();
        for view in PortView::ALL {
            let mut pane = PortfolioPane::new(view);
            let mut out = ctx.run_ui(Default::default(), |ui| {
                let _ = portfolio(ui, &mut pane, &b, None, &quotes);
            });
            out.textures_delta.clear();
            pane.account = Some("sim-ira".into());
            pane.symbol = Some("SPY US Equity".into());
            let mut out = ctx.run_ui(Default::default(), |ui| {
                let _ = portfolio(ui, &mut pane, &b, None, &quotes);
            });
            out.textures_delta.clear();
        }
        // The demo account.
        let demo = tr_broker::demo::DemoBook::new(chrono::Utc::now()).portfolio();
        for view in PortView::ALL {
            let mut pane = PortfolioPane::new(view);
            let mut out = ctx.run_ui(Default::default(), |ui| {
                let _ = portfolio(ui, &mut pane, &b, Some(&demo), &quotes);
            });
            out.textures_delta.clear();
        }
        // The setup screen when nothing is configured and sample mode is off.
        b.simulate = false;
        let mut pane = PortfolioPane::new(PortView::Summary);
        let mut out = ctx.run_ui(Default::default(), |ui| {
            let _ = portfolio(ui, &mut pane, &b, None, &quotes);
        });
        out.textures_delta.clear();
    }
}
