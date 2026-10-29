//! Brokerage portfolio model, normalised from SnapTrade responses.
//!
//! SnapTrade sends decimals both as JSON numbers and as strings (`"10.5"`), and many fields
//! are nullable or vary by brokerage, so parsing is lenient: unknown or missing values become
//! `None` rather than failing the whole account.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tr_core::{AssetClass, Security};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    /// Account number as reported (shown masked).
    pub number: String,
    pub institution: String,
    /// Reporting currency of the total value.
    pub currency: String,
    /// Total market value including cash.
    pub total: Option<f64>,
    pub cash: Option<f64>,
    pub buying_power: Option<f64>,
    pub kind: String,
    pub status: String,
    pub is_paper: bool,
    /// Last successful holdings sync.
    pub synced: Option<DateTime<Utc>>,
    /// The brokerage connection (SnapTrade authorization) this account belongs to.
    #[serde(default)]
    pub connection_id: String,
}

/// A brokerage connection (SnapTrade "brokerage authorization"): one login at one broker,
/// usually holding one or more accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    /// Brokerage display name, e.g. `Robinhood`.
    pub brokerage: String,
    pub slug: String,
    /// Connection label, e.g. `Connection-1`.
    pub name: String,
    /// The broker revoked access or the login expired: it needs reconnecting.
    pub disabled: bool,
    pub created: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    /// Whether the connection allows trading (Terminal only trades when the user enables it).
    pub trading: bool,
}

impl Account {
    /// `••••8443`.
    pub fn masked_number(&self) -> String {
        let n = self.number.trim();
        if n.len() <= 4 {
            return n.to_string();
        }
        format!("••••{}", &n[n.len() - 4..])
    }

    pub fn label(&self) -> String {
        if self.name.is_empty() { format!("{} {}", self.institution, self.masked_number()) } else { self.name.clone() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstrumentKind {
    Stock,
    Etf,
    Fund,
    Crypto,
    Option,
    Future,
    Bond,
    Cash,
    Other,
}

impl InstrumentKind {
    pub fn parse(kind: &str) -> Self {
        match kind.to_ascii_lowercase().as_str() {
            "stock" | "adr" | "cs" | "equity" => Self::Stock,
            "etf" | "et" => Self::Etf,
            "mutualfund" | "cef" | "oef" | "fund" => Self::Fund,
            "crypto" | "crypto_currency" | "cryptocurrency" => Self::Crypto,
            "option" | "future_option" => Self::Option,
            "future" => Self::Future,
            "bond" => Self::Bond,
            "cash" => Self::Cash,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stock => "Stock",
            Self::Etf => "ETF",
            Self::Fund => "Fund",
            Self::Crypto => "Crypto",
            Self::Option => "Option",
            Self::Future => "Future",
            Self::Bond => "Bond",
            Self::Cash => "Cash",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub account_id: String,
    /// Broker symbol (`AAPL`, `BTC`, `VAB.TO`, or an OCC option symbol).
    pub symbol: String,
    pub description: String,
    pub kind: InstrumentKind,
    /// Signed: negative for shorts.
    pub units: f64,
    /// Last price per unit reported by the broker.
    pub price: Option<f64>,
    /// Average cost per unit (per share for options).
    pub cost_basis: Option<f64>,
    /// Contract multiplier (100 for equity options, 1 otherwise).
    pub multiplier: f64,
    pub currency: String,
    pub cash_equivalent: bool,
    /// For options: the underlying ticker.
    pub underlying: Option<String>,
}

impl Position {
    /// Market value at `price` (or the broker's price).
    pub fn market_value_at(&self, price: Option<f64>) -> Option<f64> {
        Some(self.units * price.or(self.price)? * self.multiplier)
    }

    pub fn market_value(&self) -> Option<f64> {
        self.market_value_at(None)
    }

    pub fn cost(&self) -> Option<f64> {
        Some(self.units * self.cost_basis? * self.multiplier)
    }

    /// Unrealised profit or loss at `price` (or the broker's price).
    pub fn pnl_at(&self, price: Option<f64>) -> Option<f64> {
        Some(self.market_value_at(price)? - self.cost()?)
    }

    pub fn pnl_pct_at(&self, price: Option<f64>) -> Option<f64> {
        let cost = self.cost()?;
        (cost.abs() > 1e-9).then(|| self.pnl_at(price).map(|p| p / cost.abs() * 100.0)).flatten()
    }

    /// The terminal market key for charting, quotes and Congress lookups, when the
    /// instrument maps onto one (US-listed equities and ETFs, crypto, option underlyings).
    pub fn market_key(&self) -> Option<String> {
        market_key(self.kind, self.underlying.as_deref().unwrap_or(&self.symbol), &self.currency)
    }

    pub fn short_label(&self) -> String {
        short_label(self.kind, &self.symbol, self.underlying.as_deref())
    }
}

/// A parsed OCC option symbol: `NVDA  261218C00250000` (root padded to six, YYMMDD, C/P,
/// strike × 1000 in eight digits).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OccOption<'a> {
    pub root: &'a str,
    pub expiry: NaiveDate,
    pub call: bool,
    pub strike: f64,
}

pub fn parse_occ(symbol: &str) -> Option<OccOption<'_>> {
    let s = symbol.trim();
    if s.len() < 16 || !s.is_ascii() {
        return None;
    }
    let (head, tail) = s.split_at(s.len() - 15);
    let root = head.trim();
    let expiry = NaiveDate::parse_from_str(&tail[..6], "%y%m%d").ok()?;
    let call = match &tail[6..7] {
        "C" => true,
        "P" => false,
        _ => return None,
    };
    let strike = tail[7..].parse::<u64>().ok()? as f64 / 1000.0;
    (!root.is_empty() && root.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')).then_some(OccOption {
        root,
        expiry,
        call,
        strike,
    })
}

/// Compact label for tables: `AAPL 350C` for options, the symbol otherwise.
pub fn short_label(kind: InstrumentKind, symbol: &str, underlying: Option<&str>) -> String {
    if kind == InstrumentKind::Option
        && let Some(o) = parse_occ(symbol)
    {
        let strike = if o.strike.fract() == 0.0 { format!("{}", o.strike as i64) } else { format!("{}", o.strike) };
        return format!("{} {strike}{}", underlying.unwrap_or(o.root), if o.call { 'C' } else { 'P' });
    }
    match kind {
        InstrumentKind::Option => underlying.unwrap_or(symbol).to_string(),
        _ => symbol.to_string(),
    }
}

/// Map a broker symbol onto a terminal market key.
pub fn market_key(kind: InstrumentKind, symbol: &str, currency: &str) -> Option<String> {
    let s = symbol.trim().to_ascii_uppercase();
    if s.is_empty() || s.contains(' ') {
        return None;
    }
    match kind {
        // Share classes (`BRK.B`) are fine; exchange suffixes (`VAB.TO`, `VOD.L`) are not US listings.
        InstrumentKind::Stock | InstrumentKind::Etf | InstrumentKind::Fund | InstrumentKind::Option => {
            match s.split_once('.') {
                Some((_, suffix)) if suffix.len() > 1 => None,
                _ if !tr_core::security::looks_like_ticker(&s) => None,
                _ => Some(
                    Security::parse(&s).filter(|sec| matches!(sec.class, AssetClass::Equity | AssetClass::Etf))?.key(),
                ),
            }
        }
        InstrumentKind::Crypto => {
            let base = s.split(['-', '/']).next().unwrap_or(&s).to_string();
            let quote = match currency.trim().to_ascii_uppercase().as_str() {
                "" | "USDT" | "USDC" => "USD".to_string(),
                c => c.to_string(),
            };
            let pair = if base.ends_with(&quote) && base.len() > quote.len() { base } else { format!("{base}{quote}") };
            Some(Security::new(pair, AssetClass::Crypto).key())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub account_id: String,
    pub id: String,
    pub symbol: String,
    pub description: String,
    pub kind: InstrumentKind,
    /// BUY, SELL, SELL_SHORT, BUY_OPEN, …
    pub action: String,
    /// EXECUTED, PENDING, CANCELED, …
    pub status: String,
    pub order_type: String,
    pub time_in_force: String,
    pub quantity: Option<f64>,
    pub filled: Option<f64>,
    pub limit_price: Option<f64>,
    pub stop_price: Option<f64>,
    pub execution_price: Option<f64>,
    pub placed: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    pub underlying: Option<String>,
}

impl Order {
    pub fn is_open(&self) -> bool {
        matches!(
            self.status.as_str(),
            "PENDING"
                | "ACCEPTED"
                | "PARTIAL"
                | "QUEUED"
                | "TRIGGERED"
                | "ACTIVATED"
                | "REPLACE_PENDING"
                | "CANCEL_PENDING"
                | "PENDING_RISK_REVIEW"
                | "CONTINGENT_ORDER"
        )
    }

    pub fn is_buy(&self) -> bool {
        self.action.starts_with("BUY")
    }

    pub fn market_key(&self) -> Option<String> {
        market_key(self.kind, self.underlying.as_deref().unwrap_or(&self.symbol), "USD")
    }

    pub fn short_label(&self) -> String {
        short_label(self.kind, &self.symbol, self.underlying.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub account_id: String,
    pub id: String,
    /// BUY, SELL, DIVIDEND, CONTRIBUTION, WITHDRAWAL, FEE, INTEREST, …
    pub kind: String,
    pub symbol: Option<String>,
    pub description: String,
    pub units: Option<f64>,
    pub price: Option<f64>,
    pub amount: Option<f64>,
    pub fee: Option<f64>,
    pub currency: String,
    pub trade_date: Option<DateTime<Utc>>,
    pub settlement_date: Option<DateTime<Utc>>,
}

impl Activity {
    pub fn market_key(&self) -> Option<String> {
        market_key(InstrumentKind::Stock, self.symbol.as_deref()?, &self.currency)
    }
}

/// Everything shown in the portfolio panels.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Portfolio {
    pub accounts: Vec<Account>,
    pub positions: Vec<Position>,
    pub orders: Vec<Order>,
    pub activities: Vec<Activity>,
    /// Brokerage connections (for Settings › Brokerage).
    #[serde(default)]
    pub connections: Vec<Connection>,
    pub fetched: Option<DateTime<Utc>>,
    /// Per-account problems that did not stop the rest from loading.
    pub warnings: Vec<String>,
    /// Offline sample data, not a real brokerage.
    pub simulated: bool,
    /// The local paper-trading account ([`crate::demo`]), not a brokerage.
    #[serde(default)]
    pub demo: bool,
}

/// Totals for a set of accounts, in the first account's currency.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub value: f64,
    pub cash: f64,
    pub buying_power: f64,
    pub invested: f64,
    pub cost: f64,
    pub pnl: f64,
}

impl Totals {
    pub fn pnl_pct(&self) -> Option<f64> {
        (self.cost.abs() > 1e-9).then(|| self.pnl / self.cost.abs() * 100.0)
    }
}

impl Portfolio {
    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Accounts held through connection `id`.
    pub fn accounts_of(&self, connection: &str) -> impl Iterator<Item = &Account> {
        self.accounts.iter().filter(move |a| a.connection_id == connection)
    }

    /// Totals over the accounts selected by `filter` (`None` = all), repricing positions with
    /// `price_of` where the terminal has a fresher quote.
    pub fn totals(&self, filter: Option<&str>, price_of: impl Fn(&Position) -> Option<f64>) -> Totals {
        let mut t = Totals::default();
        let selected = |id: &str| filter.is_none_or(|f| f == id);
        for a in self.accounts.iter().filter(|a| selected(&a.id)) {
            t.cash += a.cash.unwrap_or(0.0);
            t.buying_power += a.buying_power.unwrap_or(0.0);
        }
        for p in self.positions.iter().filter(|p| selected(&p.account_id) && !p.cash_equivalent) {
            let live = price_of(p);
            t.invested += p.market_value_at(live).unwrap_or(0.0);
            if let (Some(cost), Some(pnl)) = (p.cost(), p.pnl_at(live)) {
                t.cost += cost;
                t.pnl += pnl;
            }
        }
        let cash_like: f64 = self
            .positions
            .iter()
            .filter(|p| selected(&p.account_id) && p.cash_equivalent)
            .filter_map(|p| p.market_value())
            .sum();
        // Prefer the broker's total; fall back to cash + holdings when it is missing.
        let reported: Vec<f64> = self.accounts.iter().filter(|a| selected(&a.id)).filter_map(|a| a.total).collect();
        let n = self.accounts.iter().filter(|a| selected(&a.id)).count();
        t.value = if reported.len() == n && n > 0 && price_of_is_trivial(&self.positions, &price_of) {
            reported.iter().sum()
        } else {
            t.cash + t.invested + cash_like
        };
        t
    }
}

/// True when the price override changes nothing (so the broker's totals stay authoritative).
fn price_of_is_trivial(positions: &[Position], price_of: &impl Fn(&Position) -> Option<f64>) -> bool {
    positions.iter().all(|p| price_of(p).is_none())
}

// ---- parsing ------------------------------------------------------------------------------

/// A decimal sent as a number or a numeric string.
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// RFC 3339 timestamps or bare dates.
pub fn time(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.as_str()?.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(t.and_utc());
    }
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

pub fn parse_accounts(v: &Value) -> Vec<Account> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let id = text(&a["id"]);
            (!id.is_empty()).then(|| Account {
                id,
                name: text(&a["name"]),
                number: text(&a["number"]),
                institution: text(&a["institution_name"]),
                currency: Some(text(&a["balance"]["total"]["currency"]))
                    .filter(|c| !c.is_empty())
                    .unwrap_or("USD".into()),
                total: num(&a["balance"]["total"]["amount"]),
                cash: None,
                buying_power: None,
                kind: Some(text(&a["raw_type"])).filter(|k| !k.is_empty()).unwrap_or_else(|| text(&a["meta"]["type"])),
                status: text(&a["status"]),
                is_paper: a["is_paper"].as_bool().unwrap_or(false),
                synced: time(&a["sync_status"]["holdings"]["last_successful_sync"]),
                connection_id: text(&a["brokerage_authorization"]),
            })
        })
        .collect()
}

/// `/authorizations`: the user's brokerage connections.
pub fn parse_connections(v: &Value) -> Vec<Connection> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let id = text(&c["id"]);
            let b = &c["brokerage"];
            let brokerage = Some(text(&b["display_name"]))
                .filter(|s| !s.is_empty())
                .or_else(|| Some(text(&b["name"])).filter(|s| !s.is_empty()))
                .unwrap_or_else(|| "Brokerage".into());
            (!id.is_empty()).then(|| Connection {
                id,
                brokerage,
                slug: text(&b["slug"]),
                name: text(&c["name"]),
                disabled: c["disabled"].as_bool().unwrap_or(false),
                created: time(&c["created_date"]),
                updated: time(&c["updated_date"]),
                trading: c["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("trade")),
            })
        })
        .collect()
}

/// Apply `/balances` to an account: cash and buying power in the account currency (or the
/// first currency reported).
pub fn apply_balances(account: &mut Account, v: &Value) {
    let rows = v.as_array().cloned().unwrap_or_default();
    let row =
        rows.iter().find(|b| text(&b["currency"]["code"]).eq_ignore_ascii_case(&account.currency)).or(rows.first());
    if let Some(b) = row {
        account.cash = num(&b["cash"]);
        account.buying_power = num(&b["buying_power"]);
    }
}

/// `/positions/all`: every instrument kind in one list.
pub fn parse_positions(account_id: &str, v: &Value) -> Vec<Position> {
    let rows = v["results"].as_array().or(v.as_array());
    rows.into_iter()
        .flatten()
        .filter_map(|p| {
            let inst = &p["instrument"];
            let kind = InstrumentKind::parse(inst["kind"].as_str().unwrap_or(""));
            let symbol = text(&inst["symbol"]);
            let units = num(&p["units"])?;
            if symbol.is_empty() {
                return None;
            }
            let underlying =
                (kind == InstrumentKind::Option).then(|| text(&inst["underlying"]["symbol"])).filter(|s| !s.is_empty());
            let description = match kind {
                InstrumentKind::Option => option_label(
                    underlying.as_deref().unwrap_or(&symbol),
                    inst["expiration_date"].as_str(),
                    num(&inst["strike_price"]),
                    inst["option_type"].as_str(),
                ),
                _ => text(&inst["description"]),
            };
            Some(Position {
                account_id: account_id.to_string(),
                symbol,
                description,
                kind,
                units,
                price: num(&p["price"]),
                cost_basis: num(&p["cost_basis"]),
                multiplier: num(&inst["multiplier"]).filter(|m| *m > 0.0).unwrap_or(1.0),
                currency: Some(text(&p["currency"]))
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| text(&inst["currency"])),
                cash_equivalent: p["cash_equivalent"].as_bool().unwrap_or(false),
                underlying,
            })
        })
        .collect()
}

fn option_label(underlying: &str, expiry: Option<&str>, strike: Option<f64>, kind: Option<&str>) -> String {
    let mut s = underlying.to_string();
    if let Some(d) = expiry.and_then(|e| NaiveDate::parse_from_str(e.get(..10)?, "%Y-%m-%d").ok()) {
        s += &format!(" {}", d.format("%d %b %y"));
    }
    if let Some(k) = strike {
        s += &format!(" {k}");
    }
    if let Some(t) = kind {
        s += if t.eq_ignore_ascii_case("CALL") { " C" } else { " P" };
    }
    s
}

pub fn parse_orders(account_id: &str, v: &Value) -> Vec<Order> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            let us = &o["universal_symbol"];
            let opt = &o["option_symbol"];
            let (symbol, description, kind, underlying) = if opt.is_object() {
                let under = text(&opt["underlying_symbol"]["symbol"]);
                let label = option_label(
                    &under,
                    opt["expiration_date"].as_str(),
                    num(&opt["strike_price"]),
                    opt["option_type"].as_str(),
                );
                (text(&opt["ticker"]), label, InstrumentKind::Option, Some(under).filter(|s| !s.is_empty()))
            } else if us.is_object() {
                (text(&us["symbol"]), text(&us["description"]), InstrumentKind::parse(&text(&us["type"]["code"])), None)
            } else {
                (text(&o["symbol"]), String::new(), InstrumentKind::Other, None)
            };
            let id = text(&o["brokerage_order_id"]);
            (!id.is_empty() || !symbol.is_empty()).then(|| Order {
                account_id: account_id.to_string(),
                id,
                symbol,
                description,
                kind,
                action: text(&o["action"]).to_ascii_uppercase(),
                status: text(&o["status"]).to_ascii_uppercase(),
                order_type: text(&o["order_type"]),
                time_in_force: text(&o["time_in_force"]),
                quantity: num(&o["total_quantity"]),
                filled: num(&o["filled_quantity"]),
                limit_price: num(&o["limit_price"]),
                stop_price: num(&o["stop_price"]),
                execution_price: num(&o["execution_price"]),
                placed: time(&o["time_placed"]),
                updated: time(&o["time_updated"]).or_else(|| time(&o["time_executed"])),
                underlying,
            })
        })
        .collect()
}

/// `/activities` returns `{data, pagination}`; older endpoints return a bare array.
pub fn parse_activities(account_id: &str, v: &Value) -> Vec<Activity> {
    let rows = v["data"].as_array().or(v.as_array());
    rows.into_iter()
        .flatten()
        .map(|a| {
            let opt = &a["option_symbol"];
            let symbol = if opt.is_object() { text(&opt["ticker"]) } else { text(&a["symbol"]["symbol"]) };
            Activity {
                account_id: Some(text(&a["account"]["id"])).filter(|s| !s.is_empty()).unwrap_or(account_id.to_string()),
                id: text(&a["id"]),
                kind: text(&a["type"]).to_ascii_uppercase(),
                symbol: Some(symbol).filter(|s| !s.is_empty()),
                description: text(&a["description"]),
                units: num(&a["units"]),
                price: num(&a["price"]),
                amount: num(&a["amount"]),
                fee: num(&a["fee"]),
                currency: Some(text(&a["currency"]["code"])).filter(|c| !c.is_empty()).unwrap_or("USD".into()),
                trade_date: time(&a["trade_date"]),
                settlement_date: time(&a["settlement_date"]),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(kind: InstrumentKind, symbol: &str, units: f64, price: f64, cost: f64) -> Position {
        Position {
            account_id: "a".into(),
            symbol: symbol.into(),
            description: String::new(),
            kind,
            units,
            price: Some(price),
            cost_basis: Some(cost),
            multiplier: if kind == InstrumentKind::Option { 100.0 } else { 1.0 },
            currency: "USD".into(),
            cash_equivalent: false,
            underlying: None,
        }
    }

    #[test]
    fn decimals_as_numbers_or_strings() {
        assert_eq!(num(&serde_json::json!("10.5")), Some(10.5));
        assert_eq!(num(&serde_json::json!(3)), Some(3.0));
        assert_eq!(num(&serde_json::json!(null)), None);
        assert_eq!(num(&serde_json::json!("n/a")), None);
    }

    #[test]
    fn position_pnl() {
        let p = pos(InstrumentKind::Stock, "AAPL", 10.0, 120.0, 100.0);
        assert_eq!(p.market_value(), Some(1200.0));
        assert_eq!(p.pnl_at(None), Some(200.0));
        assert_eq!(p.pnl_pct_at(None), Some(20.0));
        assert_eq!(p.pnl_at(Some(90.0)), Some(-100.0), "a live quote reprices the position");
        let short = pos(InstrumentKind::Stock, "TSLA", -5.0, 200.0, 250.0);
        assert_eq!(short.pnl_at(None), Some(250.0), "a short gains when the price falls");
        assert_eq!(short.pnl_pct_at(None), Some(20.0));
        let call = pos(InstrumentKind::Option, "AAPL  261218C00240000", 2.0, 3.5, 2.0);
        assert_eq!(call.market_value(), Some(700.0));
        assert_eq!(call.pnl_at(None), Some(300.0));
    }

    #[test]
    fn market_keys() {
        assert_eq!(market_key(InstrumentKind::Stock, "aapl", "USD").as_deref(), Some("AAPL US Equity"));
        assert_eq!(market_key(InstrumentKind::Etf, "SPY", "USD").as_deref(), Some("SPY US Equity"));
        assert_eq!(market_key(InstrumentKind::Stock, "BRK.B", "USD").as_deref(), Some("BRK.B US Equity"));
        assert_eq!(market_key(InstrumentKind::Stock, "VAB.TO", "CAD"), None, "foreign listings are not charted");
        assert_eq!(market_key(InstrumentKind::Crypto, "BTC", "USD").as_deref(), Some("BTCUSD Curncy"));
        assert_eq!(market_key(InstrumentKind::Crypto, "ETH-USD", "").as_deref(), Some("ETHUSD Curncy"));
        assert_eq!(market_key(InstrumentKind::Crypto, "SOL", "USDT").as_deref(), Some("SOLUSD Curncy"));
        assert_eq!(market_key(InstrumentKind::Bond, "US912828", "USD"), None);
        let mut opt = pos(InstrumentKind::Option, "AAPL  261218C00240000", 1.0, 1.0, 1.0);
        opt.underlying = Some("AAPL".into());
        assert_eq!(opt.market_key().as_deref(), Some("AAPL US Equity"), "options chart their underlying");
    }

    #[test]
    fn totals_reprice_with_live_quotes() {
        let mut pf = Portfolio {
            accounts: vec![Account {
                id: "a".into(),
                name: "Individual".into(),
                number: "Q6542138443".into(),
                institution: "Robinhood".into(),
                currency: "USD".into(),
                total: Some(2500.0),
                cash: Some(300.0),
                buying_power: Some(600.0),
                kind: "Margin".into(),
                status: "open".into(),
                is_paper: false,
                synced: None,
                connection_id: "c1".into(),
            }],
            positions: vec![
                pos(InstrumentKind::Stock, "AAPL", 10.0, 120.0, 100.0),
                pos(InstrumentKind::Etf, "SPY", 2.0, 500.0, 450.0),
            ],
            ..Default::default()
        };
        let t = pf.totals(None, |_| None);
        assert_eq!(t.value, 2500.0, "the broker's total is authoritative without live quotes");
        assert_eq!(t.invested, 2200.0);
        assert_eq!(t.pnl, 300.0);
        assert!((t.pnl_pct().unwrap() - 300.0 / 1900.0 * 100.0).abs() < 1e-9);
        let live = pf.totals(None, |p| (p.symbol == "AAPL").then_some(130.0));
        assert_eq!(live.invested, 2300.0);
        assert_eq!(live.value, 2600.0, "cash + repriced holdings");
        assert_eq!(pf.accounts[0].masked_number(), "••••8443");
        pf.accounts[0].total = None;
        assert_eq!(pf.totals(Some("a"), |_| None).value, 2500.0);
        assert_eq!(pf.totals(Some("other"), |_| None), Totals::default());
    }

    #[test]
    fn occ_symbols() {
        let o = parse_occ("NVDA  261218C00250000").unwrap();
        assert_eq!((o.root, o.call, o.strike), ("NVDA", true, 250.0));
        assert_eq!(o.expiry, NaiveDate::from_ymd_opt(2026, 12, 18).unwrap());
        let p = parse_occ("SPY261120P00612500").unwrap();
        assert_eq!((p.root, p.call, p.strike), ("SPY", false, 612.5));
        assert_eq!(parse_occ("AAPL"), None);
        assert_eq!(parse_occ("NVDA  261318C00250000"), None, "invalid month");
        assert_eq!(short_label(InstrumentKind::Option, "NVDA  261218C00250000", Some("NVDA")), "NVDA 250C");
        assert_eq!(short_label(InstrumentKind::Option, "SPY261120P00612500", None), "SPY 612.5P");
        assert_eq!(short_label(InstrumentKind::Stock, "AAPL", None), "AAPL");
    }

    #[test]
    fn times() {
        assert!(time(&serde_json::json!("2024-03-22T16:27:55.000Z")).is_some());
        assert!(time(&serde_json::json!("2024-03-22")).is_some());
        assert!(time(&serde_json::json!("2024-03-22T16:27:55.123456")).is_some());
        assert_eq!(time(&serde_json::json!(null)), None);
    }
}
