//! Bloomberg-style command line: `<SECURITY> [<FUNCTION> args] <GO>`.
//!
//! Examples: `AAPL`, `AAPL US EQUITY <GO>`, `5`, `1W`, `RSI`, `SMA 50`, `ICHIMOKU`, `IND MOMENTUM`,
//! `CMP MSFT`, `AAPL CONG`, `CONG`, `POL PELOSI`, `TRACK PELOSI`, `LAYOUT 4`, `FIB`, `HELP`.
//! Hyperliquid HIP-3 perps go by their venue key (`xyz:AAPL`), and `AAPL PERP` opens the perp
//! linked to a security.

use tr_chart::drawings::Tool;
use tr_chart::scene::ChartType;
pub use tr_core::indicators::registry::Spec as IndicatorSpec;
use tr_core::{Security, Timeframe, YellowKey};

/// Indicators on a new chart: the 20 and 50 bar simple moving averages.
pub fn default_indicators() -> Vec<IndicatorSpec> {
    [20.0, 50.0].iter().filter_map(|n| IndicatorSpec::new("SMA", &[*n])).collect()
}

/// Views of the brokerage portfolio pane (`PORT`, `ACCT`, `POS`, `ORD`, `ACTV`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PortView {
    #[default]
    Summary,
    Accounts,
    Positions,
    Orders,
    Activity,
}

impl PortView {
    pub const ALL: [PortView; 5] =
        [PortView::Summary, PortView::Accounts, PortView::Positions, PortView::Orders, PortView::Activity];

    pub fn label(self) -> &'static str {
        match self {
            PortView::Summary => "Summary",
            PortView::Accounts => "Accounts",
            PortView::Positions => "Positions",
            PortView::Orders => "Orders",
            PortView::Activity => "Activity",
        }
    }

    /// The function code shown on the tab and typed on the command line.
    pub fn code(self) -> &'static str {
        match self {
            PortView::Summary => "PORT",
            PortView::Accounts => "ACCT",
            PortView::Positions => "POS",
            PortView::Orders => "ORD",
            PortView::Activity => "ACTV",
        }
    }
}

/// `BUY AAPL 10`, `SELL 5 MSFT LMT 410`, `BUY NVDA $500`, `TRADE AAPL`: fields for the order
/// ticket. A command only fills the ticket; orders are placed from the ticket after a preview
/// and a confirmation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeArgs {
    /// `Some(true)` buy, `Some(false)` sell, `None` the ticket's default.
    pub buy: Option<bool>,
    /// Market key (`AAPL US Equity`) of the security, when one was given.
    pub symbol: Option<String>,
    pub units: Option<f64>,
    /// Dollar amount (`$500`).
    pub notional: Option<f64>,
    pub limit: Option<f64>,
    pub stop: Option<f64>,
    /// `SELL ALL TSLA`: the whole position.
    pub all: bool,
}

impl TradeArgs {
    /// Parse the words after `BUY`/`SELL`/`TRADE`. Unknown words make the command invalid so
    /// a typo never fills the ticket with something unintended.
    fn parse(buy: Option<bool>, rest: &[&str]) -> Option<TradeArgs> {
        let mut t = TradeArgs { buy, ..Default::default() };
        let price =
            |s: &str| s.trim_start_matches(['@', '$']).replace(',', "").parse::<f64>().ok().filter(|x| *x > 0.0);
        let mut words = Vec::new();
        let mut i = 0;
        while i < rest.len() {
            let w = rest[i];
            let up = w.to_ascii_uppercase();
            match up.as_str() {
                "LMT" | "LIMIT" | "@" => {
                    t.limit = Some(price(rest.get(i + 1)?)?);
                    i += 1;
                }
                "STP" | "STOP" => {
                    t.stop = Some(price(rest.get(i + 1)?)?);
                    i += 1;
                }
                "MKT" | "MARKET" | "SHARES" | "SHS" | "SH" => {}
                _ if up.starts_with('@') => t.limit = Some(price(&up)?),
                _ if up.starts_with('$') => t.notional = Some(price(&up)?),
                _ if up.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.') => {
                    if t.units.is_some() {
                        return None;
                    }
                    t.units = Some(price(&up)?);
                }
                _ => words.push(w),
            }
            i += 1;
        }
        if t.units.is_some() && t.notional.is_some() {
            return None;
        }
        // `ALL` means the whole position when another word names the security (`ALL` alone
        // is Allstate's ticker).
        if words.len() > 1
            && let Some(i) = words.iter().position(|w| w.eq_ignore_ascii_case("ALL"))
        {
            if t.buy == Some(true) || t.units.is_some() || t.notional.is_some() {
                return None;
            }
            words.remove(i);
            t.all = true;
            t.buy = Some(false);
        }
        if !words.is_empty() {
            let (sec, used) = Security::resolve_tokens(&words)?;
            if used != words.len() {
                return None;
            }
            t.symbol = Some(sec.key());
        }
        Some(t)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A market key, e.g. `BTCUSD Curncy`.
    Symbol(String),
    SymbolFunction(String, Function),
    Function(Function),
}

/// Quick range: span of history plus the interval used to show it (mirrors common charting presets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    D1,
    D5,
    M1,
    M3,
    M6,
    Ytd,
    Y1,
    Y5,
    All,
}

impl Range {
    pub const ALL: [Range; 9] =
        [Range::D1, Range::D5, Range::M1, Range::M3, Range::M6, Range::Ytd, Range::Y1, Range::Y5, Range::All];

    pub fn label(self) -> &'static str {
        match self {
            Range::D1 => "1D",
            Range::D5 => "5D",
            Range::M1 => "1M",
            Range::M3 => "3M",
            Range::M6 => "6M",
            Range::Ytd => "YTD",
            Range::Y1 => "1Y",
            Range::Y5 => "5Y",
            Range::All => "ALL",
        }
    }

    pub fn timeframe(self) -> Timeframe {
        match self {
            Range::D1 => Timeframe::M1,
            Range::D5 => Timeframe::M5,
            Range::M1 => Timeframe::M30,
            Range::M3 => Timeframe::H1,
            Range::M6 => Timeframe::H4,
            Range::Ytd | Range::Y1 => Timeframe::D1,
            Range::Y5 => Timeframe::W1,
            Range::All => Timeframe::MN1,
        }
    }

    /// Approximate seconds of history to show (`None` = everything).
    pub fn span_secs(self, now: i64) -> Option<i64> {
        const D: i64 = 86_400;
        Some(match self {
            Range::D1 => D,
            Range::D5 => 5 * D,
            Range::M1 => 30 * D,
            Range::M3 => 91 * D,
            Range::M6 => 182 * D,
            Range::Ytd => {
                use chrono::Datelike;
                let y = chrono::DateTime::from_timestamp(now, 0).unwrap_or_default().year();
                now - chrono::NaiveDate::from_ymd_opt(y, 1, 1)?.and_hms_opt(0, 0, 0)?.and_utc().timestamp()
            }
            Range::Y1 => 365 * D,
            Range::Y5 => 5 * 365 * D,
            Range::All => return None,
        })
    }

    fn parse(s: &str) -> Option<Range> {
        Range::ALL.into_iter().find(|r| r.label().eq_ignore_ascii_case(s))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Function {
    Timeframe(Timeframe),
    ChartType(ChartType),
    /// Add or remove an indicator (any code from the catalog: `RSI`, `BB 20 2.5`, `ICHIMOKU`).
    Indicator(IndicatorSpec),
    /// Open the indicator picker, pre-filled with a search (`IND`, `IND VOLUME`).
    IndicatorPicker(String),
    ClearIndicators,
    Compare(Option<String>),
    LogScale,
    Range(Range),
    Draw(Tool),
    ClearDrawings,
    Magnet,
    /// Congress trades: feed when used alone, per-ticker list after a symbol.
    Congress,
    Politician(String),
    Track(String),
    Untrack(String),
    Alerts,
    Refresh,
    Watchlist,
    WatchAdd(Vec<String>),
    WatchRemove(Vec<String>),
    /// `LAYOUT 1|2|2H|3|4|6` preset, `LAYOUT <name>` switch, or a layout subcommand
    /// (`SAVE`, `SAVEAS`, `NEW`, `RENAME`, `DUP`, `DEL`, `EXPORT`, `IMPORT`).
    Layout(String),
    /// List saved layouts.
    Layouts,
    /// Add a panel (`NEW NEWS`, `NEW BOOK BTCUSD`); empty adds a chart.
    NewPanel(String),
    /// Split the focused pane, adding a panel to the right (or below).
    Split {
        down: bool,
        panel: String,
    },
    PopOut,
    /// Put the focused pane in a colour link group (`None` = unlink).
    Link(Option<crate::layouts::Link>),
    /// Headlines (for a security when used after one).
    News,
    /// Order book depth (levels).
    OrderBook(Option<usize>),
    /// Option chain monitor (Bloomberg OMON).
    Options,
    /// `SIDEBAR LEFT|RIGHT|TOGGLE|SHOW|HIDE|SWAP`.
    Sidebar(String),
    Settings,
    /// Check for a newer release of the app.
    Update,
    /// `THEME LIGHT|DARK|SYSTEM|COMPACT|REGULAR|COMFORTABLE`.
    Theme(String),
    /// Rename the focused tab (empty resets).
    Title(String),
    ClosePane,
    Help,
    /// World equity indices / markets overview.
    Wei,
    /// US Treasury yield curve.
    Curve,
    /// Security search across asset classes.
    Search(String),
    /// Brokerage portfolio view, with an optional subcommand (`REFRESH`, `CONNECT`, `SETUP`).
    Portfolio(PortView, String),
    /// Open the order ticket (`BUY`, `SELL`, `TRADE`).
    Trade(TradeArgs),
    /// Switch between the demo account and live accounts (`DEMO`, `MODE LIVE`); `true` = live.
    AccountMode(bool),
    /// `KILL`: cancel every open Hyperliquid order and turn Hyperliquid orders off (confirmed).
    Kill,
    /// `AAPL PERP`: open the HIP-3 perp linked to a security. Alone, or on a perp, it switches
    /// the focused pane between the two.
    Perp,
}

fn num<T: std::str::FromStr>(t: Option<&&str>) -> Option<T> {
    t.and_then(|s| s.parse().ok())
}

fn parse_function(head: &str, rest: &[&str]) -> Option<Function> {
    let up = head.to_ascii_uppercase();
    let words = || rest.iter().map(|s| s.to_ascii_uppercase()).collect::<Vec<_>>();
    let joined = || rest.join(" ");
    Some(match up.as_str() {
        "CANDLE" | "CANDLES" => Function::ChartType(ChartType::Candles),
        "BAR" | "BARS" | "OHLC" => Function::ChartType(ChartType::Bars),
        "LINE" => Function::ChartType(ChartType::Line),
        "AREA" | "MOUNTAIN" => Function::ChartType(ChartType::Area),
        // `IND RSI 21` adds directly; anything else searches the picker.
        "IND" | "INDICATOR" | "INDICATORS" | "STUDY" | "STUDIES" => {
            match rest.split_first().and_then(|(code, args)| IndicatorSpec::parse(code, args)) {
                Some(spec) => Function::Indicator(spec),
                None => Function::IndicatorPicker(joined()),
            }
        }
        "CLEAR" | "CLR" => match words().first().map(String::as_str) {
            Some("DRAW") | Some("DRAWINGS") => Function::ClearDrawings,
            _ => Function::ClearIndicators,
        },
        "CMP" | "COMPARE" | "COMP" => Function::Compare(rest.first().map(|s| s.to_ascii_uppercase())),
        "LOG" => Function::LogScale,
        "TL" | "TREND" | "TRENDLINE" => Function::Draw(Tool::Trendline),
        "HL" | "HLINE" => Function::Draw(Tool::Horizontal),
        "VL" | "VLINE" => Function::Draw(Tool::Vertical),
        "FIB" => Function::Draw(Tool::Fib),
        "MAGNET" | "MAG" => Function::Magnet,
        "CONG" | "CONGRESS" => Function::Congress,
        "POL" | "PROFILE" => Function::Politician(joined()),
        "TRACK" => Function::Track(joined()),
        "UNTRACK" => Function::Untrack(joined()),
        "ALRT" | "ALERTS" | "ALERT" => Function::Alerts,
        "REFRESH" | "SYNC" => Function::Refresh,
        "W" | "WL" | "WATCH" | "WATCHLIST" | "MON" => match words().first().map(String::as_str) {
            Some("ADD") => Function::WatchAdd(words()[1..].to_vec()),
            Some("DEL") | Some("RM") | Some("REMOVE") => Function::WatchRemove(words()[1..].to_vec()),
            _ => Function::Watchlist,
        },
        "LAYOUT" | "GRID" | "LP" => Function::Layout(joined()),
        "LAYOUTS" => Function::Layouts,
        "NEW" | "ADD" => Function::NewPanel(joined()),
        "SPLIT" | "SPLITV" | "SPLITH" => {
            let w = words();
            let down = up == "SPLITH" || matches!(w.first().map(String::as_str), Some("DOWN" | "H" | "BELOW"));
            let skip =
                usize::from(matches!(w.first().map(String::as_str), Some("DOWN" | "H" | "BELOW" | "RIGHT" | "V")));
            Function::Split { down, panel: rest[skip.min(rest.len())..].join(" ") }
        }
        "POP" | "POPOUT" | "UNDOCK" => Function::PopOut,
        "LINK" => Function::Link(match words().first().map(String::as_str) {
            None | Some("OFF" | "NONE" | "0") => None,
            Some(c) => Some(crate::layouts::Link::ALL.into_iter().find(|l| l.label().eq_ignore_ascii_case(c))?),
        }),
        "N" | "CN" | "NEWS" | "TOP" => Function::News,
        "OB" | "BOOK" | "DEPTH" | "DOM" => Function::OrderBook(num(rest.first())),
        "OMON" | "OMST" | "OPT" | "OPTIONS" | "CHAIN" => Function::Options,
        "SIDEBAR" | "SB" => Function::Sidebar(words().join(" ")),
        "PREFS" | "SETTINGS" | "PDF" | "CONFIG" => Function::Settings,
        "UPDATE" | "UPDATES" => Function::Update,
        "THEME" => Function::Theme(words().join(" ")),
        "TITLE" | "RENAME" => Function::Title(joined()),
        "CLOSE" => Function::ClosePane,
        "HELP" | "?" | "H" => Function::Help,
        "WEI" | "MARKETS" | "WORLD" => Function::Wei,
        "GC" | "YCRV" | "CURVE" | "YC" => Function::Curve,
        "SECF" | "SEARCH" | "FIND" => Function::Search(joined()),
        "PORT" | "PORTFOLIO" | "PRTU" => Function::Portfolio(PortView::Summary, words().join(" ")),
        "ACCT" | "ACCOUNTS" | "ACCTS" | "BAL" | "BALANCES" => {
            Function::Portfolio(PortView::Accounts, words().join(" "))
        }
        "POS" | "POSITIONS" | "HOLDINGS" => Function::Portfolio(PortView::Positions, words().join(" ")),
        "ORD" | "ORDERS" | "BLTR" => Function::Portfolio(PortView::Orders, words().join(" ")),
        "ACTV" | "ACTIVITY" | "TXNS" | "TRANSACTIONS" => Function::Portfolio(PortView::Activity, words().join(" ")),
        "BROKER" | "BROKERAGE" | "SNAPTRADE" => Function::Portfolio(PortView::Summary, "SETUP".into()),
        "BUY" => Function::Trade(TradeArgs::parse(Some(true), rest)?),
        "SELL" => Function::Trade(TradeArgs::parse(Some(false), rest)?),
        "TRADE" | "TICKET" | "EMSX" => Function::Trade(TradeArgs::parse(None, rest)?),
        "KILL" if rest.is_empty() => Function::Kill,
        "DEMO" | "PAPER" if rest.is_empty() => Function::AccountMode(false),
        "MODE" => match words().join(" ").as_str() {
            "DEMO" | "PAPER" => Function::AccountMode(false),
            "LIVE" | "REAL" => Function::AccountMode(true),
            _ => return None,
        },
        "RANGE" => Function::Range(Range::parse(rest.first()?)?),
        "PERP" if rest.is_empty() => Function::Perp,
        // Timeframes win where spellings overlap (`1D`, `1M`); otherwise try a range (`5D`, `YTD`),
        // then the indicator catalog (`RSI`, `BB 20 2.5`).
        _ => {
            return head
                .parse::<Timeframe>()
                .ok()
                .map(Function::Timeframe)
                .or_else(|| Range::parse(&up).map(Function::Range))
                .or_else(|| IndicatorSpec::parse(head, rest).map(Function::Indicator));
        }
    })
}

/// Parse one command line. Returns `None` for empty or unrecognised input.
pub fn parse(input: &str) -> Option<Command> {
    let tokens: Vec<&str> = input.split_whitespace().filter(|t| !t.eq_ignore_ascii_case("<GO>")).collect();
    let (&head, rest) = tokens.split_first()?;
    // `<TICKER> <YELLOW KEY>` is always a security, even if the ticker spells a function.
    let keyed = match rest {
        [k, ..] if YellowKey::parse(k).is_some() => true,
        [us, k, ..] if us.eq_ignore_ascii_case("US") && YellowKey::parse(k).is_some() => true,
        _ => false,
    };
    if !keyed && let Some(f) = parse_function(head, rest) {
        return Some(Command::Function(f));
    }
    let (sec, used) = Security::resolve_tokens(&tokens)?;
    match tokens[used..].split_first() {
        None => Some(Command::Symbol(sec.key())),
        // `TSLA SELL ALL`: the security is already named, so `ALL` is the whole position.
        Some((&f, args))
            if ["SELL", "TRADE"].iter().any(|h| f.eq_ignore_ascii_case(h))
                && args.iter().any(|a| a.eq_ignore_ascii_case("ALL")) =>
        {
            let rest: Vec<&str> = args.iter().copied().filter(|a| !a.eq_ignore_ascii_case("ALL")).collect();
            match parse_function(f, &rest)? {
                Function::Trade(mut t) if t.symbol.is_none() && t.units.is_none() && t.notional.is_none() => {
                    t.all = true;
                    t.buy = Some(false);
                    Some(Command::SymbolFunction(sec.key(), Function::Trade(t)))
                }
                _ => None,
            }
        }
        Some((&f, args)) => parse_function(f, args).map(|func| Command::SymbolFunction(sec.key(), func)),
    }
}

/// One-line live interpretation of partially typed input, shown under the command line.
pub fn hint(input: &str) -> String {
    match parse(input) {
        None if input.trim().is_empty() => "Type a ticker and press <GO> (Enter).  HELP for functions.".into(),
        None => "Unrecognised command".into(),
        Some(Command::Symbol(s)) => {
            let badge = tr_core::perps::is_perp(&s).then(|| tr_core::perps::badge(&s));
            match (Security::parse(&s).and_then(|x| x.name()), badge) {
                (Some(name), Some(b)) => format!("Load {s} — {name} · {b}"),
                (None, Some(b)) => format!("Load {s} · {b}"),
                (Some(name), None) => format!("Load {s} — {name}"),
                (None, None) => format!("Load {s}"),
            }
        }
        Some(Command::SymbolFunction(s, Function::Perp)) => match tr_core::perps::counterpart(&s) {
            Some(other) if tr_core::perps::is_perp(&s) => format!("{s}: switch to {other}"),
            Some(perp) => format!("{s}: open its HIP-3 perp {perp} ({})", tr_core::perps::badge(perp)),
            None if tr_core::perps::is_perp(&s) => format!("{s}: no listed instrument is linked to it"),
            None => format!("{s}: no HIP-3 perp is linked to it"),
        },
        Some(Command::SymbolFunction(s, f)) => format!("{s}: {}", describe(&f)),
        Some(Command::Function(f)) => describe(&f),
    }
}

fn describe(f: &Function) -> String {
    match f {
        Function::Timeframe(tf) => format!(
            "Interval {}",
            match tf {
                Timeframe::M1 => "1 minute",
                Timeframe::M5 => "5 minutes",
                Timeframe::M15 => "15 minutes",
                Timeframe::M30 => "30 minutes",
                Timeframe::H1 => "1 hour",
                Timeframe::H4 => "4 hours",
                Timeframe::D1 => "1 day",
                Timeframe::W1 => "1 week",
                Timeframe::MN1 => "1 month",
            }
        ),
        Function::ChartType(c) => format!("Chart type {}", c.label()),
        Function::Indicator(i) => format!("Toggle {} — {}", i.label(), i.name()),
        Function::IndicatorPicker(q) if q.is_empty() => "Browse indicators".into(),
        Function::IndicatorPicker(q) => format!("Search indicators for \"{q}\""),
        Function::ClearIndicators => "Remove all indicators".into(),
        Function::Compare(Some(s)) => format!("Compare with {s}"),
        Function::Compare(None) => "Remove comparison".into(),
        Function::LogScale => "Toggle log scale".into(),
        Function::Range(r) => format!("Show {} at {}", r.label(), r.timeframe()),
        Function::Draw(t) => format!("Draw {}", t.label()),
        Function::ClearDrawings => "Remove all drawings".into(),
        Function::Magnet => "Toggle magnet (snap to OHLC)".into(),
        Function::Congress => "Congressional trades".into(),
        Function::Politician(n) => format!("Politician profile: {n}"),
        Function::Track(n) => format!("Track {n} (alert on new filings)"),
        Function::Untrack(n) => format!("Stop tracking {n}"),
        Function::Alerts => "Alerts".into(),
        Function::Refresh => "Refresh congressional disclosures now".into(),
        Function::Watchlist => "Watchlist".into(),
        Function::WatchAdd(s) => format!("Add {} to watchlist", s.join(", ")),
        Function::WatchRemove(s) => format!("Remove {} from watchlist", s.join(", ")),
        Function::Layout(l) if l.trim().is_empty() => "Saved layouts".into(),
        Function::Layout(l) => format!("Layout {l}"),
        Function::Layouts => "Saved layouts".into(),
        Function::NewPanel(p) if p.trim().is_empty() => "Add a chart pane".into(),
        Function::NewPanel(p) => format!("Add panel: {p}"),
        Function::Split { down, panel } => {
            format!(
                "Split {} with {}",
                if *down { "down" } else { "right" },
                if panel.is_empty() { "a chart" } else { panel }
            )
        }
        Function::PopOut => "Pop the focused pane out into its own window".into(),
        Function::Link(Some(l)) => format!("Link focused pane to the {} group", l.label()),
        Function::Link(None) => "Unlink focused pane".into(),
        Function::News => "News headlines".into(),
        Function::OrderBook(_) => "Order book (depth)".into(),
        Function::Options => "Option chain: calls and puts by strike, IV and greeks".into(),
        Function::Sidebar(s) if s.is_empty() => "Toggle the sidebar".into(),
        Function::Sidebar(s) => format!("Sidebar {}", s.to_lowercase()),
        Function::Settings => "Settings: theme, fonts, density, hotkeys, panels, layouts".into(),
        Function::Update => "Check for updates".into(),
        Function::Theme(t) => format!("Theme {t}"),
        Function::Title(t) => format!("Rename tab to {t:?}"),
        Function::ClosePane => "Close focused pane".into(),
        Function::Help => "Help".into(),
        Function::Wei => "World markets overview".into(),
        Function::Curve => "US Treasury yield curve".into(),
        Function::Search(q) => format!("Search securities: {q}"),
        Function::Portfolio(v, a) if a.is_empty() => match v {
            PortView::Summary => "Brokerage portfolio: value, cash, P&L and holdings (SnapTrade)".into(),
            PortView::Accounts => "Brokerage accounts and balances".into(),
            PortView::Positions => "Brokerage positions with unrealised P&L".into(),
            PortView::Orders => "Brokerage orders (last 30 days)".into(),
            PortView::Activity => "Brokerage activity: trades, dividends, transfers".into(),
        },
        Function::Portfolio(_, a) => match a.as_str() {
            "REFRESH" | "SYNC" => "Reload brokerage accounts now".into(),
            "CONNECT" | "LINK" => "Connect a brokerage in your browser (SnapTrade, read-only)".into(),
            _ => "Brokerage connection settings".into(),
        },
        Function::Trade(t) => {
            let side = match t.buy {
                Some(true) => "Buy",
                Some(false) => "Sell",
                None => "Trade",
            };
            let mut s = format!("Order ticket: {side}");
            if let Some(u) = t.units {
                s += &format!(" {}", tr_broker::trading::fmt_units(u));
            }
            if let Some(n) = t.notional {
                s += &format!(" ${n:.2} of");
            }
            if let Some(k) = &t.symbol {
                s += &format!(" {}", Security::parse(k).map_or(k.clone(), |x| x.ticker));
            }
            if let Some(p) = t.stop {
                s += &format!(" stop {p}");
            }
            if let Some(p) = t.limit {
                s += &format!(" limit {p}");
            }
            if t.all {
                s += " (whole position)";
            }
            s
        }
        Function::AccountMode(false) => "Switch to the DEMO account (paper trading)".into(),
        Function::AccountMode(true) => "Switch to LIVE brokerage accounts (asks first)".into(),
        Function::Kill => {
            "Kill switch: cancel every open Hyperliquid testnet order, then turn Hyperliquid orders off".into()
        }
        Function::Perp => "Switch between the security and its HIP-3 perp (PERP · HL xyz)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols() {
        let sym = |s: &str| Some(Command::Symbol(s.into()));
        assert_eq!(parse("aapl"), sym("AAPL US Equity"));
        assert_eq!(parse("AAPL US EQUITY <GO>"), sym("AAPL US Equity"));
        assert_eq!(parse("brk/b"), sym("BRK.B US Equity"));
        assert_eq!(parse("BTCUSD <CRNCY>"), sym("BTCUSD Curncy"));
        assert_eq!(parse("EURUSD <CRNCY>"), sym("EURUSD Curncy"));
        assert_eq!(parse("US10Y <GOVT>"), sym("US10Y Govt"));
        assert_eq!(parse("CL1 <CMDTY>"), sym("CL1 Comdty"));
        assert_eq!(parse("SPX <INDEX>"), sym("SPX Index"));
        assert_eq!(parse("AAPL <EQUITY>"), sym("AAPL US Equity"));
        assert_eq!(parse("btcusd"), sym("BTCUSD Curncy"));
        // A yellow key makes a function-like word a ticker.
        assert_eq!(parse("LINE <EQUITY>"), sym("LINE US Equity"));
        assert_eq!(parse("LINE"), Some(Command::Function(Function::ChartType(ChartType::Line))));
        assert_eq!(parse("   "), None);
        assert_eq!(parse("123abc"), None);
    }

    #[test]
    fn timeframes_and_ranges() {
        assert_eq!(parse("5"), Some(Command::Function(Function::Timeframe(Timeframe::M5))));
        assert_eq!(parse("1W"), Some(Command::Function(Function::Timeframe(Timeframe::W1))));
        assert_eq!(parse("1M"), Some(Command::Function(Function::Timeframe(Timeframe::MN1))));
        assert_eq!(parse("1m"), Some(Command::Function(Function::Timeframe(Timeframe::M1))));
        assert_eq!(parse("YTD"), Some(Command::Function(Function::Range(Range::Ytd))));
        assert_eq!(parse("RANGE 5D"), Some(Command::Function(Function::Range(Range::D5))));
        assert_eq!(Range::Y5.timeframe(), Timeframe::W1);
    }

    #[test]
    fn functions() {
        let ind = |code: &str, params: &[f64]| {
            Some(Command::Function(Function::Indicator(IndicatorSpec::new(code, params).unwrap())))
        };
        assert_eq!(parse("rsi"), ind("RSI", &[14.0]));
        assert_eq!(parse("SMA 50"), ind("SMA", &[50.0]));
        assert_eq!(parse("bb 20 2.5"), ind("BB", &[20.0, 2.5]));
        assert_eq!(parse("MACD"), ind("MACD", &[12.0, 26.0, 9.0]));
        assert_eq!(parse("ichimoku"), ind("ICHIMOKU", &[]));
        assert_eq!(parse("IND STOCH 5 3 3"), ind("STOCH", &[5.0, 3.0, 3.0]));
        assert_eq!(parse("IND"), Some(Command::Function(Function::IndicatorPicker(String::new()))));
        assert_eq!(parse("study volume"), Some(Command::Function(Function::IndicatorPicker("volume".into()))));
        let Some(Command::SymbolFunction(_, Function::Indicator(adx))) = parse("AAPL ADX") else { panic!("AAPL ADX") };
        assert_eq!(adx.label(), "ADX 14 14");
        assert_eq!(parse("CMP msft"), Some(Command::Function(Function::Compare(Some("MSFT".into())))));
        assert_eq!(parse("CMP"), Some(Command::Function(Function::Compare(None))));
        assert_eq!(parse("FIB"), Some(Command::Function(Function::Draw(Tool::Fib))));
        assert_eq!(parse("CLEAR DRAW"), Some(Command::Function(Function::ClearDrawings)));
        assert_eq!(parse("POL Nancy Pelosi"), Some(Command::Function(Function::Politician("Nancy Pelosi".into()))));
        assert_eq!(
            parse("WL ADD msft nvda"),
            Some(Command::Function(Function::WatchAdd(vec!["MSFT".into(), "NVDA".into()])))
        );
        assert_eq!(parse("LAYOUT 4"), Some(Command::Function(Function::Layout("4".into()))));
        assert_eq!(
            parse("LAYOUT save Morning Desk"),
            Some(Command::Function(Function::Layout("save Morning Desk".into())))
        );
        assert_eq!(parse("NEW BOOK"), Some(Command::Function(Function::NewPanel("BOOK".into()))));
        assert_eq!(
            parse("SPLIT DOWN NEWS"),
            Some(Command::Function(Function::Split { down: true, panel: "NEWS".into() }))
        );
        assert_eq!(parse("SPLIT"), Some(Command::Function(Function::Split { down: false, panel: String::new() })));
        assert_eq!(parse("LINK BLUE"), Some(Command::Function(Function::Link(Some(crate::layouts::Link::Blue)))));
        assert_eq!(parse("LINK OFF"), Some(Command::Function(Function::Link(None))));
        assert_eq!(parse("LINK PINK"), None);
        assert_eq!(parse("AAPL CN"), Some(Command::SymbolFunction("AAPL US Equity".into(), Function::News)));
        assert_eq!(
            parse("BTCUSD CRNCY OB 50"),
            Some(Command::SymbolFunction("BTCUSD Curncy".into(), Function::OrderBook(Some(50))))
        );
        assert_eq!(parse("PREFS"), Some(Command::Function(Function::Settings)));
        assert_eq!(parse("update"), Some(Command::Function(Function::Update)));
        assert_eq!(parse("AAPL OMON"), Some(Command::SymbolFunction("AAPL US Equity".into(), Function::Options)));
        assert_eq!(parse("SPX INDEX OMON"), Some(Command::SymbolFunction("SPX Index".into(), Function::Options)));
        assert_eq!(parse("OMON"), Some(Command::Function(Function::Options)));
        assert_eq!(parse("SIDEBAR RIGHT"), Some(Command::Function(Function::Sidebar("RIGHT".into()))));
    }

    #[test]
    fn portfolio_commands() {
        let f = |v, a: &str| Some(Command::Function(Function::Portfolio(v, a.into())));
        assert_eq!(parse("PORT"), f(PortView::Summary, ""));
        assert_eq!(parse("port refresh"), f(PortView::Summary, "REFRESH"));
        assert_eq!(parse("ACCT"), f(PortView::Accounts, ""));
        assert_eq!(parse("POS"), f(PortView::Positions, ""));
        assert_eq!(parse("ORD"), f(PortView::Orders, ""));
        assert_eq!(parse("ACTV"), f(PortView::Activity, ""));
        assert_eq!(parse("PORT CONNECT"), f(PortView::Summary, "CONNECT"));
        assert_eq!(parse("BROKER"), f(PortView::Summary, "SETUP"));
        // After a security: positions (or orders) in that security.
        assert_eq!(
            parse("AAPL POS"),
            Some(Command::SymbolFunction(
                "AAPL US Equity".into(),
                Function::Portfolio(PortView::Positions, String::new())
            ))
        );
        // `ACT` stays a ticker (Enact Holdings), not the activity view.
        assert_eq!(parse("ACT"), Some(Command::Symbol("ACT US Equity".into())));
        for v in PortView::ALL {
            assert_eq!(parse(v.code()), f(v, ""), "{} round-trips", v.code());
        }
    }

    #[test]
    fn trade_commands() {
        let t = |s: &str| match parse(s) {
            Some(Command::Function(Function::Trade(t))) => Some(t),
            Some(Command::SymbolFunction(k, Function::Trade(mut t))) => {
                t.symbol = Some(k);
                Some(t)
            }
            _ => None,
        };
        let aapl = Some("AAPL US Equity".to_string());
        assert_eq!(
            t("BUY AAPL 10"),
            Some(TradeArgs { buy: Some(true), symbol: aapl.clone(), units: Some(10.0), ..Default::default() })
        );
        assert_eq!(t("buy 10 aapl"), t("BUY AAPL 10"), "quantity and symbol in either order");
        assert_eq!(t("AAPL BUY 10"), t("BUY AAPL 10"), "after a security");
        assert_eq!(t("AAPL US EQUITY BUY 10"), t("BUY AAPL 10"));
        let s = t("SELL MSFT 5 LMT 410.50").unwrap();
        assert_eq!((s.buy, s.units, s.limit), (Some(false), Some(5.0), Some(410.5)));
        assert_eq!(t("SELL MSFT 5 @410.5").unwrap().limit, Some(410.5));
        let st = t("SELL TSLA 3 STP 250 LMT 245").unwrap();
        assert_eq!((st.stop, st.limit), (Some(250.0), Some(245.0)));
        let n = t("BUY NVDA $1,500").unwrap();
        assert_eq!((n.notional, n.units), (Some(1500.0), None));
        assert_eq!(t("BUY 0.5 BRK/B").unwrap().symbol.as_deref(), Some("BRK.B US Equity"));
        let open = t("TRADE").unwrap();
        assert_eq!((open.buy, open.symbol), (None, None));
        assert_eq!(t("TRADE AAPL").unwrap().symbol, aapl);
        assert_eq!(t("BUY").unwrap().buy, Some(true));
        // Ambiguous or malformed orders are rejected rather than guessed.
        assert_eq!(t("BUY AAPL 10 20"), None);
        assert_eq!(t("BUY AAPL 10 $500"), None);
        assert_eq!(t("BUY AAPL LMT"), None);
        assert_eq!(t("BUY AAPL LMT -5"), None);
        assert!(hint("BUY AAPL 10 LMT 150").contains("Buy 10 AAPL limit 150"));
        let all = t("SELL ALL TSLA").unwrap();
        assert_eq!((all.buy, all.all, all.symbol.as_deref()), (Some(false), true, Some("TSLA US Equity")));
        assert_eq!(t("sell tsla all"), t("SELL ALL TSLA"));
        assert_eq!(t("TSLA SELL ALL").map(|a| a.all), Some(true));
        assert!(!t("SELL ALL").unwrap().all, "ALL alone is Allstate");
        assert_eq!(t("BUY ALL TSLA"), None);
        assert_eq!(t("SELL ALL TSLA 5"), None);
        assert_eq!(parse("DEMO"), Some(Command::Function(Function::AccountMode(false))));
        assert_eq!(parse("MODE LIVE"), Some(Command::Function(Function::AccountMode(true))));
        assert_eq!(parse("KILL"), Some(Command::Function(Function::Kill)));
        assert_eq!(parse("kill"), Some(Command::Function(Function::Kill)));
        let perp = t("BUY xyz:AAPL 0.5 @226.4").unwrap();
        assert_eq!((perp.symbol.as_deref(), perp.units, perp.limit), (Some("xyz:AAPL"), Some(0.5), Some(226.4)));
    }

    #[test]
    fn symbol_with_function() {
        assert_eq!(parse("NVDA CONG <GO>"), Some(Command::SymbolFunction("NVDA US Equity".into(), Function::Congress)));
        assert_eq!(
            parse("BTCUSD CRNCY 5"),
            Some(Command::SymbolFunction("BTCUSD Curncy".into(), Function::Timeframe(Timeframe::M5)))
        );
        assert_eq!(parse("WEI"), Some(Command::Function(Function::Wei)));
        assert_eq!(parse("GC"), Some(Command::Function(Function::Curve)));
        assert_eq!(parse("SECF gold"), Some(Command::Function(Function::Search("gold".into()))));
        assert_eq!(
            parse("AAPL US EQUITY 1W"),
            Some(Command::SymbolFunction("AAPL US Equity".into(), Function::Timeframe(Timeframe::W1)))
        );
        assert_eq!(parse("AAPL WHATEVER"), None);
    }

    #[test]
    fn hip3_perps() {
        let sym = |s: &str| Some(Command::Symbol(s.into()));
        assert_eq!(parse("xyz:aapl"), sym("xyz:AAPL"));
        assert_eq!(parse("XYZ:AAPL <GO>"), sym("xyz:AAPL"));
        assert_eq!(parse("aapl"), sym("AAPL US Equity"), "a ticker is still the listed stock");
        assert_eq!(
            parse("xyz:AAPL 5"),
            Some(Command::SymbolFunction("xyz:AAPL".into(), Function::Timeframe(Timeframe::M5)))
        );
        assert_eq!(parse("xyz:GOLD OB"), Some(Command::SymbolFunction("xyz:GOLD".into(), Function::OrderBook(None))));
        // `PERP` after a security asks for its perp; alone it switches the focused pane.
        let perp = |s: &str| Some(Command::SymbolFunction(s.into(), Function::Perp));
        assert_eq!(parse("AAPL PERP"), perp("AAPL US Equity"));
        assert_eq!(parse("aapl <equity> perp"), perp("AAPL US Equity"));
        assert_eq!(parse("EURUSD CRNCY PERP"), perp("EURUSD Curncy"));
        assert_eq!(parse("PERP"), Some(Command::Function(Function::Perp)));
        assert_eq!(hint("xyz:aapl"), "Load xyz:AAPL — Apple Inc perp · PERP · HL xyz");
        assert_eq!(hint("xyz:SKHX"), "Load xyz:SKHX · PERP · HL xyz");
        assert_eq!(hint("AAPL PERP"), "AAPL US Equity: open its HIP-3 perp xyz:AAPL (PERP · HL xyz)");
        assert_eq!(hint("xyz:AAPL PERP"), "xyz:AAPL: switch to AAPL US Equity");
        assert_eq!(hint("NDX INDEX PERP"), "NDX Index: no HIP-3 perp is linked to it");
    }

    #[test]
    fn hints() {
        assert_eq!(hint("5"), "Interval 5 minutes");
        assert_eq!(hint("msft"), "Load MSFT US Equity — Microsoft Corp");
        assert_eq!(hint("US10Y GOVT"), "Load US10Y Govt — US Treasury 10 Year");
        assert!(hint("").contains("ticker"));
    }

    #[test]
    fn indicator_identity() {
        let spec = |code: &str, params: &[f64]| IndicatorSpec::new(code, params).unwrap();
        assert!(spec("RSI", &[14.0]).same_kind(&spec("RSI", &[21.0])));
        assert!(!spec("SMA", &[20.0]).same_kind(&spec("SMA", &[50.0])));
        assert!(spec("MACD", &[]).is_pane());
    }

    /// Every indicator code and alias reaches the indicator: no function, timeframe or range
    /// spelling shadows it.
    #[test]
    fn every_indicator_code_parses_to_its_indicator() {
        for def in tr_core::indicators::registry::all() {
            for code in std::iter::once(&def.code).chain(def.aliases) {
                match parse(code) {
                    Some(Command::Function(Function::Indicator(s))) => assert_eq!(s.code, def.code, "{code}"),
                    other => panic!("{code} parses as {other:?}"),
                }
                assert_eq!(parse(&format!("IND {code}")), parse(code), "IND {code}");
            }
        }
    }
}
