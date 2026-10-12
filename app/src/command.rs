//! Bloomberg-style command line: `<SECURITY> [<FUNCTION> args] <GO>`.
//!
//! Examples: `AAPL`, `AAPL US EQUITY <GO>`, `5`, `1W`, `RSI`, `SMA 50`, `CMP MSFT`,
//! `AAPL CONG`, `CONG`, `POL PELOSI`, `TRACK PELOSI`, `LAYOUT 4`, `FIB`, `HELP`.

use tr_chart::drawings::Tool;
use tr_chart::scene::ChartType;
use tr_core::{Security, Timeframe, YellowKey};

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum IndicatorSpec {
    Sma(usize),
    Ema(usize),
    Vwap,
    Bollinger(usize, f64),
    Rsi(usize),
    Macd(usize, usize, usize),
}

impl IndicatorSpec {
    pub fn label(&self) -> String {
        match self {
            IndicatorSpec::Sma(n) => format!("SMA {n}"),
            IndicatorSpec::Ema(n) => format!("EMA {n}"),
            IndicatorSpec::Vwap => "VWAP".into(),
            IndicatorSpec::Bollinger(n, k) => format!("BB {n} {k}"),
            IndicatorSpec::Rsi(n) => format!("RSI {n}"),
            IndicatorSpec::Macd(a, b, c) => format!("MACD {a} {b} {c}"),
        }
    }

    /// Oscillators get their own sub-pane.
    pub fn is_pane(&self) -> bool {
        matches!(self, IndicatorSpec::Rsi(_) | IndicatorSpec::Macd(..))
    }

    /// Same indicator family (used to toggle and replace).
    pub fn same_kind(&self, other: &IndicatorSpec) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && match (self, other) {
                (IndicatorSpec::Sma(a), IndicatorSpec::Sma(b)) | (IndicatorSpec::Ema(a), IndicatorSpec::Ema(b)) => {
                    a == b
                }
                _ => true,
            }
    }
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
    Indicator(IndicatorSpec),
    ClearIndicators,
    Compare(Option<String>),
    LogScale,
    Range(Range),
    Draw(Tool),
    ClearDrawings,
    /// Show or hide the focused chart's drawing toolbar.
    DrawingToolbar,
    /// Undo or redo the focused chart's last drawing change.
    Undo,
    Redo,
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
        "SMA" => Function::Indicator(IndicatorSpec::Sma(num(rest.first()).unwrap_or(20))),
        "EMA" => Function::Indicator(IndicatorSpec::Ema(num(rest.first()).unwrap_or(20))),
        "VWAP" => Function::Indicator(IndicatorSpec::Vwap),
        "BB" | "BOLL" | "BOLLINGER" => Function::Indicator(IndicatorSpec::Bollinger(
            num(rest.first()).unwrap_or(20),
            num(rest.get(1)).unwrap_or(2.0),
        )),
        "RSI" => Function::Indicator(IndicatorSpec::Rsi(num(rest.first()).unwrap_or(14))),
        "MACD" => Function::Indicator(IndicatorSpec::Macd(
            num(rest.first()).unwrap_or(12),
            num(rest.get(1)).unwrap_or(26),
            num(rest.get(2)).unwrap_or(9),
        )),
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
        "RAY" => Function::Draw(Tool::Ray),
        "EXTLINE" | "EXTENDED" => Function::Draw(Tool::ExtendedLine),
        "HRAY" => Function::Draw(Tool::HorizontalRay),
        "CROSS" | "CROSSLINE" => Function::Draw(Tool::CrossLine),
        "CHANNEL" => Function::Draw(Tool::ParallelChannel),
        "FIBEXT" => Function::Draw(Tool::FibExtension),
        "RECT" | "RECTANGLE" => Function::Draw(Tool::Rectangle),
        "LONG" => Function::Draw(Tool::LongPosition),
        "SHORT" => Function::Draw(Tool::ShortPosition),
        "MEASURE" | "RULER" => Function::Draw(Tool::Measure),
        "TOOLS" | "TOOLBAR" | "DRAWBAR" => Function::DrawingToolbar,
        "UNDO" => Function::Undo,
        "REDO" => Function::Redo,
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
        "RANGE" => Function::Range(Range::parse(rest.first()?)?),
        // Timeframes win where spellings overlap (`1D`, `1M`); otherwise try a range (`5D`, `YTD`).
        _ => {
            return head
                .parse::<Timeframe>()
                .ok()
                .map(Function::Timeframe)
                .or_else(|| Range::parse(&up).map(Function::Range));
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
        Some((&f, args)) => parse_function(f, args).map(|func| Command::SymbolFunction(sec.key(), func)),
    }
}

/// One-line live interpretation of partially typed input, shown under the command line.
pub fn hint(input: &str) -> String {
    match parse(input) {
        None if input.trim().is_empty() => "Type a ticker and press <GO> (Enter).  HELP for functions.".into(),
        None => "Unrecognised command".into(),
        Some(Command::Symbol(s)) => match Security::parse(&s).and_then(|x| x.name()) {
            Some(name) => format!("Load {s} — {name}"),
            None => format!("Load {s}"),
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
        Function::Indicator(i) => format!("Toggle {}", i.label()),
        Function::ClearIndicators => "Remove all indicators".into(),
        Function::Compare(Some(s)) => format!("Compare with {s}"),
        Function::Compare(None) => "Remove comparison".into(),
        Function::LogScale => "Toggle log scale".into(),
        Function::Range(r) => format!("Show {} at {}", r.label(), r.timeframe()),
        Function::Draw(t) => format!("Draw {}", t.label()),
        Function::ClearDrawings => "Remove all drawings".into(),
        Function::DrawingToolbar => "Show or hide the drawing toolbar".into(),
        Function::Undo => "Undo the last drawing change".into(),
        Function::Redo => "Redo the last undone drawing change".into(),
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
        assert_eq!(parse("rsi"), Some(Command::Function(Function::Indicator(IndicatorSpec::Rsi(14)))));
        assert_eq!(parse("SMA 50"), Some(Command::Function(Function::Indicator(IndicatorSpec::Sma(50)))));
        assert_eq!(parse("bb 20 2.5"), Some(Command::Function(Function::Indicator(IndicatorSpec::Bollinger(20, 2.5)))));
        assert_eq!(parse("MACD"), Some(Command::Function(Function::Indicator(IndicatorSpec::Macd(12, 26, 9)))));
        assert_eq!(parse("CMP msft"), Some(Command::Function(Function::Compare(Some("MSFT".into())))));
        assert_eq!(parse("CMP"), Some(Command::Function(Function::Compare(None))));
        assert_eq!(parse("FIB"), Some(Command::Function(Function::Draw(Tool::Fib))));
        assert_eq!(parse("CLEAR DRAW"), Some(Command::Function(Function::ClearDrawings)));
        assert_eq!(parse("hray"), Some(Command::Function(Function::Draw(Tool::HorizontalRay))));
        assert_eq!(parse("CHANNEL"), Some(Command::Function(Function::Draw(Tool::ParallelChannel))));
        assert_eq!(parse("FIBEXT"), Some(Command::Function(Function::Draw(Tool::FibExtension))));
        assert_eq!(parse("LONG"), Some(Command::Function(Function::Draw(Tool::LongPosition))));
        assert_eq!(parse("RULER"), Some(Command::Function(Function::Draw(Tool::Measure))));
        assert_eq!(parse("TOOLS"), Some(Command::Function(Function::DrawingToolbar)));
        assert_eq!(parse("undo"), Some(Command::Function(Function::Undo)));
        assert_eq!(parse("REDO"), Some(Command::Function(Function::Redo)));
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
    fn hints() {
        assert_eq!(hint("5"), "Interval 5 minutes");
        assert_eq!(hint("msft"), "Load MSFT US Equity — Microsoft Corp");
        assert_eq!(hint("US10Y GOVT"), "Load US10Y Govt — US Treasury 10 Year");
        assert!(hint("").contains("ticker"));
    }

    #[test]
    fn indicator_identity() {
        assert!(IndicatorSpec::Rsi(14).same_kind(&IndicatorSpec::Rsi(21)));
        assert!(!IndicatorSpec::Sma(20).same_kind(&IndicatorSpec::Sma(50)));
        assert!(IndicatorSpec::Macd(12, 26, 9).is_pane());
    }
}
