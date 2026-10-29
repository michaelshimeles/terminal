//! Congressional trading panels: latest feed, per-ticker list, politician profile,
//! politician directory and alerts. Rows are dense fixed-width monospace like a
//! Bloomberg list function.

use egui::{Color32, RichText, ScrollArea, Sense, Ui};
use tr_congress::db::{Alert, PoliticianSummary};
use tr_congress::{Chamber, Party, Trade, TxType};

use crate::theme::{self, amber, mono, muted, text, ui_font};
use crate::worker::Profile;

/// Shown instead of Congress data in the browser build, which can't fetch disclosures yet.
pub const WEB_NOTE: &str =
    "Congress data needs the desktop app for now: browsers can't reach Senate eFD or the House Clerk.";

/// What a click inside a panel asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    OpenChart(String),
    OpenTicker(String),
    OpenProfile(String),
    Track(String, bool),
    AddToWatchlist(String),
    /// Load a security into every pane of a colour link group.
    LinkedSymbol(crate::layouts::Link, String),
    /// Run a command line (`AAPL OMON`, `PORT REFRESH`) without adding it to the history.
    Command(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeedFilter {
    pub party: Option<Party>,
    pub chamber: Option<Chamber>,
    /// `Some(true)` buys only, `Some(false)` sells only.
    pub buys: Option<bool>,
    pub tickers_only: bool,
}

impl FeedFilter {
    pub fn matches(&self, t: &Trade) -> bool {
        self.party.is_none_or(|p| p == t.party)
            && self.chamber.is_none_or(|c| c == t.chamber)
            && self.buys.is_none_or(|b| if b { t.tx_type.is_buy() } else { t.tx_type.is_sell() })
            && (!self.tickers_only || t.ticker.is_some())
    }
}

fn tx_text(t: TxType) -> (&'static str, Color32) {
    match t {
        TxType::Purchase => ("BUY ", theme::up()),
        TxType::Sale => ("SELL", theme::down()),
        TxType::SalePartial => ("SL-P", theme::down()),
        TxType::Exchange => ("EXCH", muted()),
        TxType::Other => ("OTHR", muted()),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        format!("{s:<n$}")
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn clickable(ui: &mut Ui, text: String, color: Color32) -> egui::Response {
    let r = ui.add(egui::Label::new(RichText::new(text).font(mono(12.0)).color(color)).extend().sense(Sense::click()));
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

fn plain(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text.into()).font(mono(12.0)).color(color)).extend());
}

/// Column header row; never wraps (narrow panes clip instead, like a terminal).
fn header(ui: &mut Ui, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).font(mono(11.5)).color(amber()).background_color(theme::header()))
            .extend(),
    );
}

/// One trade row. `show_politician` / `show_ticker` drop redundant columns.
fn trade_row(
    ui: &mut Ui,
    t: &Trade,
    show_politician: bool,
    show_ticker: bool,
    extra: Option<(String, Color32)>,
    out: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        plain(ui, t.filed_date.format("%m/%d/%y").to_string(), muted());
        plain(ui, t.tx_date.format("%m/%d/%y").to_string(), text());
        let lag = t.disclosure_lag_days();
        plain(ui, format!("{lag:>3}d"), if lag > 45 { theme::down() } else { muted() });
        if show_politician {
            let name = truncate(&t.politician, 22);
            let r = clickable(ui, name, theme::party_color(t.party));
            if r.clicked() {
                out.push(Action::OpenProfile(t.politician.clone()));
            }
            r.context_menu(|ui| {
                if ui.button("Open Profile").clicked() {
                    out.push(Action::OpenProfile(t.politician.clone()));
                    ui.close();
                }
                if ui.button("Track Filings").clicked() {
                    out.push(Action::Track(t.politician.clone(), true));
                    ui.close();
                }
                if ui.button("Copy Disclosure Link").clicked() {
                    ui.ctx().copy_text(t.source_url.clone());
                    ui.close();
                }
            });
            plain(
                ui,
                format!("{}-{:<2}", t.party.code(), t.state.as_deref().unwrap_or("")),
                theme::party_color(t.party),
            );
            plain(ui, if t.chamber == Chamber::House { "H" } else { "S" }, muted());
        }
        let (tx, c) = tx_text(t.tx_type);
        plain(ui, tx, c);
        if show_ticker {
            match &t.ticker {
                Some(tk) => {
                    let r = clickable(ui, format!("{tk:<6}"), amber());
                    if r.clicked() {
                        out.push(Action::OpenChart(tk.clone()));
                    }
                    let r = r.on_hover_text("Click: chart · right-click: more");
                    r.context_menu(|ui| {
                        if ui.button("Open Chart").clicked() {
                            out.push(Action::OpenChart(tk.clone()));
                            ui.close();
                        }
                        if ui.button(format!("Who in Congress Traded {tk}")).clicked() {
                            out.push(Action::OpenTicker(tk.clone()));
                            ui.close();
                        }
                        if ui.button("Add to Watchlist").clicked() {
                            out.push(Action::AddToWatchlist(tk.clone()));
                            ui.close();
                        }
                    });
                }
                None => plain(ui, "-     ", muted()),
            }
        }
        plain(ui, format!("{:<13}", t.amount.map(|a| a.to_string()).unwrap_or_else(|| "n/a".into())), text());
        if let Some((s, c)) = extra {
            plain(ui, s, c);
        }
        plain(ui, truncate(t.owner.as_deref().unwrap_or("Self"), 8), muted());
        plain(ui, truncate(&t.asset_name, 40), muted());
    });
}

pub fn feed(ui: &mut Ui, trades: &[Trade], filter: &mut FeedFilter, status: &str) -> Vec<Action> {
    let mut out = Vec::new();
    ui.horizontal(|ui| {
        let chip = |ui: &mut Ui, label: &str, on: bool| {
            ui.selectable_label(on, RichText::new(label).font(ui_font(11.5))).clicked()
        };
        if chip(ui, "ALL", *filter == FeedFilter::default()) {
            *filter = FeedFilter::default();
        }
        for (label, p) in [("DEM", Party::Democrat), ("REP", Party::Republican), ("IND", Party::Independent)] {
            if chip(ui, label, filter.party == Some(p)) {
                filter.party = if filter.party == Some(p) { None } else { Some(p) };
            }
        }
        for (label, c) in [("HOUSE", Chamber::House), ("SENATE", Chamber::Senate)] {
            if chip(ui, label, filter.chamber == Some(c)) {
                filter.chamber = if filter.chamber == Some(c) { None } else { Some(c) };
            }
        }
        for (label, b) in [("BUYS", true), ("SELLS", false)] {
            if chip(ui, label, filter.buys == Some(b)) {
                filter.buys = if filter.buys == Some(b) { None } else { Some(b) };
            }
        }
        if chip(ui, "TICKERS", filter.tickers_only) {
            filter.tickers_only = !filter.tickers_only;
        }
        plain(ui, status, muted());
    });
    header(ui, "FILED    TRADED   LAG  POLITICIAN             PTY  C  TYPE TICKER AMOUNT        OWNER    ASSET");
    let rows: Vec<&Trade> = trades.iter().filter(|t| filter.matches(t)).collect();
    if rows.is_empty() {
        let empty = if cfg!(target_arch = "wasm32") {
            WEB_NOTE
        } else {
            "No disclosures yet. Ingest runs in the background (REFRESH to force)."
        };
        plain(ui, empty, muted());
    }
    let row_h = 16.0;
    ScrollArea::both().id_salt("feed").auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
        for t in &rows[range] {
            trade_row(ui, t, true, true, None, &mut out);
        }
    });
    out
}

pub fn ticker_trades(ui: &mut Ui, symbol: &str, trades: &[Trade]) -> Vec<Action> {
    let mut out = Vec::new();
    let buys = trades.iter().filter(|t| t.tx_type.is_buy()).count();
    let sells = trades.iter().filter(|t| t.tx_type.is_sell()).count();
    let mut people: Vec<&str> = trades.iter().map(|t| t.politician.as_str()).collect();
    people.sort();
    people.dedup();
    let buy_vol: f64 =
        trades.iter().filter(|t| t.tx_type.is_buy()).filter_map(|t| t.amount.map(|a| a.midpoint())).sum();
    let sell_vol: f64 =
        trades.iter().filter(|t| t.tx_type.is_sell()).filter_map(|t| t.amount.map(|a| a.midpoint())).sum();
    ui.horizontal(|ui| {
        let r = clickable(ui, format!("{symbol} US Equity"), amber());
        if r.clicked() {
            out.push(Action::OpenChart(symbol.to_string()));
        }
        plain(
            ui,
            format!(
                "{} members · {buys} buys / {sells} sells · est. net ${:+.1}M",
                people.len(),
                (buy_vol - sell_vol) / 1e6
            ),
            text(),
        );
    });
    header(ui, "FILED    TRADED   LAG  POLITICIAN             PTY  C  TYPE AMOUNT        OWNER    ASSET");
    if trades.is_empty() {
        plain(ui, format!("No congressional trades disclosed for {symbol}."), muted());
    }
    ScrollArea::both().id_salt(("ticker", symbol)).auto_shrink([false, false]).show_rows(
        ui,
        16.0,
        trades.len(),
        |ui, range| {
            for t in &trades[range] {
                trade_row(ui, t, true, false, None, &mut out);
            }
        },
    );
    out
}

pub fn profile(ui: &mut Ui, query: &str, p: Option<&Profile>, loading: bool) -> Vec<Action> {
    let mut out = Vec::new();
    let Some(p) = p else {
        plain(
            ui,
            if loading {
                format!("Loading profile for '{query}'…")
            } else {
                format!("No politician matches '{query}'. Try POL with a last name.")
            },
            muted(),
        );
        return out;
    };
    let pol = &p.politician;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&pol.name).font(mono(15.0)).color(theme::party_color(pol.party)));
        plain(
            ui,
            format!(
                "{} · {} {}{}",
                match pol.party {
                    Party::Democrat => "Democrat",
                    Party::Republican => "Republican",
                    Party::Independent => "Independent",
                    Party::Unknown => "Party unknown",
                },
                pol.chamber.as_str(),
                pol.state.as_deref().unwrap_or(""),
                pol.district.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()
            ),
            text(),
        );
        let label = if p.tracked { "★ TRACKED" } else { "☆ TRACK" };
        if ui
            .button(RichText::new(label).font(mono(12.0)).color(amber()))
            .on_hover_text("Alert me when this politician files a new trade")
            .clicked()
        {
            out.push(Action::Track(pol.id.to_string(), !p.tracked));
        }
    });
    let perf = &p.perf;
    let ret_col = if perf.weighted_return >= 0.0 { theme::up() } else { theme::down() };
    ui.horizontal(|ui| {
        plain(ui, format!("Trades {}", p.trades.len()), text());
        plain(ui, format!("Scored {}", perf.scored), muted());
        ui.label(RichText::new(format!("Est. return {:+.1}%", perf.weighted_return * 100.0)).font(mono(12.0)).color(ret_col))
            .on_hover_text("Amount-weighted directional return from each trade date to today (buys long, sells short). Estimate: disclosures give value bands only.");
        plain(ui, format!("Hit rate {:.0}%", perf.hit_rate * 100.0), text());
        plain(ui, format!("Avg lag {:.0}d", perf.avg_lag_days), if perf.avg_lag_days > 45.0 { theme::down() } else { muted() });
    });
    // Top tickers by count.
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for t in &p.trades {
        if let Some(tk) = &t.ticker {
            *counts.entry(tk).or_default() += 1;
        }
    }
    let mut top: Vec<(&str, usize)> = counts.into_iter().collect();
    top.sort_by_key(|t| std::cmp::Reverse(t.1));
    ui.horizontal_wrapped(|ui| {
        plain(ui, "Most traded:", muted());
        for (tk, n) in top.into_iter().take(12) {
            if clickable(ui, format!("{tk}({n})"), amber()).clicked() {
                out.push(Action::OpenChart(tk.to_string()));
            }
        }
    });
    header(ui, "FILED    TRADED   LAG  TYPE TICKER AMOUNT        RETURN  OWNER    ASSET");
    let ret_by_id: std::collections::HashMap<i64, f64> = p.returns.iter().map(|r| (r.trade_id, r.ret)).collect();
    ScrollArea::both().id_salt(("profile", pol.id)).auto_shrink([false, false]).show_rows(
        ui,
        16.0,
        p.trades.len(),
        |ui, range| {
            for t in &p.trades[range] {
                let extra = ret_by_id
                    .get(&t.id)
                    .map(|r| (format!("{:>+6.1}%", r * 100.0), if *r >= 0.0 { theme::up() } else { theme::down() }))
                    .unwrap_or(("   n/a ".into(), muted()));
                trade_row(ui, t, false, true, Some(extra), &mut out);
            }
        },
    );
    out
}

pub fn politicians(ui: &mut Ui, list: &[PoliticianSummary], search: &mut String) -> Vec<Action> {
    let mut out = Vec::new();
    ui.horizontal(|ui| {
        plain(ui, "Search:", muted());
        ui.add(egui::TextEdit::singleline(search).font(mono(12.0)).desired_width(180.0));
        plain(ui, format!("{} politicians", list.len()), muted());
    });
    header(ui, "TRK POLITICIAN               PTY  ST  CH      TRADES  LAST FILED");
    let q = search.to_ascii_lowercase();
    let rows: Vec<&PoliticianSummary> =
        list.iter().filter(|p| q.is_empty() || p.politician.name.to_ascii_lowercase().contains(&q)).collect();
    ScrollArea::both().id_salt("politicians").auto_shrink([false, false]).show_rows(
        ui,
        16.0,
        rows.len(),
        |ui, range| {
            for s in &rows[range] {
                let p = &s.politician;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if clickable(ui, if s.tracked { " ★ ".into() } else { " ☆ ".into() }, amber()).clicked() {
                        out.push(Action::Track(p.id.to_string(), !s.tracked));
                    }
                    if clickable(ui, truncate(&p.name, 24), theme::party_color(p.party)).clicked() {
                        out.push(Action::OpenProfile(p.id.to_string()));
                    }
                    plain(ui, format!("{:<4}", p.party.code()), theme::party_color(p.party));
                    plain(ui, format!("{:<3}", p.state.as_deref().unwrap_or("")), text());
                    plain(ui, format!("{:<7}", p.chamber.as_str()), muted());
                    plain(ui, format!("{:>6}", s.trades), text());
                    plain(ui, s.last_filed.map(|d| d.to_string()).unwrap_or_default(), muted());
                });
            }
        },
    );
    out
}

pub fn alerts(ui: &mut Ui, alerts: &[Alert]) -> (Vec<Action>, bool) {
    let mut out = Vec::new();
    let mut mark_seen = false;
    ui.horizontal(|ui| {
        let unseen = alerts.iter().filter(|a| !a.seen).count();
        plain(ui, format!("{unseen} unread"), if unseen > 0 { amber() } else { muted() });
        if ui.button(RichText::new("MARK READ").font(mono(11.5))).clicked() {
            mark_seen = true;
        }
        plain(ui, "TRACK <name> to get alerts when a politician files a new PTR", muted());
    });
    header(ui, "TIME              POLITICIAN               MESSAGE");
    ScrollArea::vertical().id_salt("alerts").auto_shrink([false, false]).show(ui, |ui| {
        for a in alerts {
            ui.horizontal(|ui| {
                let ts = chrono::DateTime::from_timestamp(a.created_at, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                plain(ui, ts, if a.seen { muted() } else { amber() });
                if clickable(ui, truncate(&a.politician, 24), text()).clicked() {
                    out.push(Action::OpenProfile(a.politician_id.to_string()));
                }
                plain(ui, &a.message, if a.seen { muted() } else { text() });
            });
        }
    });
    (out, mark_seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use tr_congress::AmountRange;

    fn trade(party: Party, chamber: Chamber, tx: TxType, ticker: Option<&str>) -> Trade {
        Trade {
            id: 1,
            filing_id: "f".into(),
            politician_id: 1,
            politician: "X".into(),
            party,
            chamber,
            state: None,
            ticker: ticker.map(str::to_string),
            asset_name: "a".into(),
            asset_type: None,
            owner: None,
            tx_type: tx,
            amount: AmountRange::parse("$1,001 - $15,000"),
            tx_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            filed_date: NaiveDate::from_ymd_opt(2026, 1, 20).unwrap(),
            source_url: String::new(),
        }
    }

    #[test]
    fn feed_filter() {
        let t = trade(Party::Democrat, Chamber::Senate, TxType::SalePartial, Some("AAPL"));
        assert!(FeedFilter::default().matches(&t));
        assert!(FeedFilter { party: Some(Party::Democrat), buys: Some(false), ..Default::default() }.matches(&t));
        assert!(!FeedFilter { chamber: Some(Chamber::House), ..Default::default() }.matches(&t));
        assert!(!FeedFilter { buys: Some(true), ..Default::default() }.matches(&t));
        let bond = trade(Party::Republican, Chamber::House, TxType::Purchase, None);
        assert!(!FeedFilter { tickers_only: true, ..Default::default() }.matches(&bond));
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("abc", 5), "abc  ");
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
    }
}
