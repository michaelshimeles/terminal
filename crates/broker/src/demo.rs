//! The demo account: paper trading with virtual cash, kept entirely on this machine.
//!
//! A [`DemoBook`] holds cash, positions, the order history and fills. Market orders fill at
//! the terminal's live quote; limit, stop and stop-limit orders rest until a quote crosses
//! their price ([`DemoBook::on_price`]). Buys need buying power (cash not already set aside
//! for resting buys) and sells need shares not already offered by resting sells: the demo
//! account never borrows or sells short. The book is saved as JSON in the app data folder
//! after every change.
//!
//! Simplifications, stated so nobody mistakes the demo for a broker: no commissions, no
//! partial fills, no market hours (orders fill at the last quote whenever it updates), and
//! Day orders expire at the next 4 p.m. New York close, taken as 20:00 UTC.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Account, Activity, InstrumentKind, Order, Portfolio, Position};
use crate::trading::{Amount, OrderRequest, OrderType, Side, TimeInForce, clean_symbol, fmt_units};

/// The demo account's id (never a SnapTrade id, which are UUIDs).
pub const ACCOUNT_ID: &str = "demo-paper";
/// Virtual cash a new or reset demo account starts with.
pub const STARTING_CASH: f64 = 100_000.0;
/// Orders and fills kept in the history (oldest dropped first).
pub const HISTORY: usize = 500;
const FILE_VERSION: u32 = 1;
/// Slack for float comparisons on money and share counts.
const EPS: f64 = 1e-6;

/// Statuses the demo uses (the portfolio model's vocabulary).
const OPEN: &str = "PENDING";
const TRIGGERED: &str = "TRIGGERED";
const FILLED: &str = "EXECUTED";
const CANCELED: &str = "CANCELED";
const REJECTED: &str = "REJECTED";
const EXPIRED: &str = "EXPIRED";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DemoBook {
    pub version: u32,
    pub starting_cash: f64,
    pub cash: f64,
    /// Profit or loss realised by sells since the last reset.
    pub realized: f64,
    pub positions: Vec<Position>,
    /// Newest first.
    pub orders: Vec<Order>,
    /// Fills, newest first.
    pub activities: Vec<Activity>,
    pub created: Option<DateTime<Utc>>,
    pub updated: Option<DateTime<Utc>>,
    seq: u64,
}

impl Default for DemoBook {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            starting_cash: STARTING_CASH,
            cash: STARTING_CASH,
            realized: 0.0,
            positions: Vec::new(),
            orders: Vec::new(),
            activities: Vec::new(),
            created: None,
            updated: None,
            seq: 0,
        }
    }
}

/// Something that happened to a resting order.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Filled(Order),
    /// Could not fill when its price was reached (no longer enough cash or shares).
    Rejected(Order, String),
    Expired(Order),
}

impl Event {
    pub fn order(&self) -> &Order {
        match self {
            Event::Filled(o) | Event::Rejected(o, _) | Event::Expired(o) => o,
        }
    }

    /// A line for a toast: `Bought 10 AAPL at $187.40`.
    pub fn message(&self) -> String {
        match self {
            Event::Filled(o) => fill_message(o),
            Event::Rejected(o, why) => format!("{} rejected: {why}", order_line(o)),
            Event::Expired(o) => format!("{} expired", order_line(o)),
        }
    }
}

/// What an order would cost or raise, for the ticket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub units: f64,
    /// The price used: the quote for market orders, the order's own price otherwise.
    pub price: f64,
    pub value: f64,
    pub buying_power: f64,
    /// Shares held that are not already offered by resting sells.
    pub available: f64,
}

/// `Bought 10 AAPL at $187.40` / `Sold 2.5 MSFT at $410.00`.
pub fn fill_message(o: &Order) -> String {
    let units = fmt_units(o.filled.or(o.quantity).unwrap_or(0.0));
    let px = o.execution_price.unwrap_or(0.0);
    let verb = if o.is_buy() { "Bought" } else { "Sold" };
    format!("{verb} {units} {} at ${px:.2}", o.symbol)
}

/// `Buy 10 AAPL limit 180.00`.
pub fn order_line(o: &Order) -> String {
    let side = if o.is_buy() { "Buy" } else { "Sell" };
    let mut s = format!("{side} {} {}", fmt_units(o.quantity.unwrap_or(0.0)), o.symbol);
    match (o.stop_price, o.limit_price) {
        (Some(st), Some(l)) => s += &format!(" stop {st:.2} limit {l:.2}"),
        (Some(st), None) => s += &format!(" stop {st:.2}"),
        (None, Some(l)) => s += &format!(" limit {l:.2}"),
        (None, None) => s += " at market",
    }
    s
}

fn positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// When a Day order placed at `placed` expires: the first 20:00 UTC after it.
pub fn day_expiry(placed: DateTime<Utc>) -> DateTime<Utc> {
    let close = NaiveTime::from_hms_opt(20, 0, 0).expect("valid time");
    let today = placed.date_naive().and_time(close).and_utc();
    if placed < today { today } else { today + Duration::days(1) }
}

impl DemoBook {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self { created: Some(now), updated: Some(now), ..Default::default() }
    }

    /// Load from `path`. A missing file is a fresh account; an unreadable one is an error so
    /// the caller can keep it aside instead of overwriting the user's history.
    pub fn load(path: &Path, now: DateTime<Utc>) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_json(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new(now)),
            Err(e) => Err(e).context("reading the demo account"),
        }
    }

    /// Parse a saved account, rejecting damaged files.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let mut b: DemoBook = serde_json::from_slice(bytes).context("the demo account file is damaged")?;
        b.version = FILE_VERSION;
        if !b.cash.is_finite() || !b.starting_cash.is_finite() {
            anyhow::bail!("the demo account file has invalid balances");
        }
        Ok(b)
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// Write atomically (temporary file, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_json()?)?;
        std::fs::rename(&tmp, path).context("saving the demo account")
    }

    /// Start over with `cash` (the default is [`STARTING_CASH`]): positions, orders and
    /// history are cleared.
    pub fn reset(&mut self, cash: f64, now: DateTime<Utc>) {
        let cash = if positive(cash) { cash } else { STARTING_CASH };
        *self = Self { starting_cash: cash, cash, ..Self::new(now) };
    }

    pub fn position(&self, symbol: &str) -> Option<&Position> {
        self.positions.iter().find(|p| same(&p.symbol, symbol))
    }

    pub fn held(&self, symbol: &str) -> f64 {
        self.position(symbol).map_or(0.0, |p| p.units)
    }

    /// Cash set aside for resting buy orders, at their limit or stop price.
    pub fn reserved(&self) -> f64 {
        self.orders
            .iter()
            .filter(|o| o.is_open() && o.is_buy())
            .map(|o| o.quantity.unwrap_or(0.0) * o.limit_price.or(o.stop_price).unwrap_or(0.0))
            .sum()
    }

    /// Cash available for new buys.
    pub fn buying_power(&self) -> f64 {
        (self.cash - self.reserved()).max(0.0)
    }

    /// Shares of `symbol` that can still be sold (held minus resting sells).
    pub fn available(&self, symbol: &str) -> f64 {
        let offered: f64 = self
            .orders
            .iter()
            .filter(|o| o.is_open() && !o.is_buy() && same(&o.symbol, symbol))
            .map(|o| o.quantity.unwrap_or(0.0))
            .sum();
        (self.held(symbol) - offered).max(0.0)
    }

    /// Market value of the positions at their last prices.
    pub fn invested(&self) -> f64 {
        self.positions.iter().filter_map(Position::market_value).sum()
    }

    pub fn equity(&self) -> f64 {
        self.cash + self.invested()
    }

    /// The account as the portfolio panes show it.
    pub fn account(&self) -> Account {
        Account {
            id: ACCOUNT_ID.into(),
            name: "Demo".into(),
            number: "DEMO".into(),
            institution: "Paper trading".into(),
            currency: "USD".into(),
            total: Some(self.equity()),
            cash: Some(self.cash),
            buying_power: Some(self.buying_power()),
            kind: "Paper".into(),
            status: "open".into(),
            is_paper: true,
            synced: self.updated,
            connection_id: String::new(),
        }
    }

    pub fn portfolio(&self) -> Portfolio {
        Portfolio {
            accounts: vec![self.account()],
            positions: self.positions.clone(),
            orders: self.orders.clone(),
            activities: self.activities.clone(),
            connections: Vec::new(),
            fetched: self.updated,
            warnings: Vec::new(),
            simulated: false,
            demo: true,
        }
    }

    /// Units and value of `req` at `quote`, or why it cannot be placed.
    pub fn estimate(&self, req: &OrderRequest, quote: Option<f64>) -> Result<Estimate, String> {
        req.validate()?;
        if req.account_id != ACCOUNT_ID {
            return Err("That order is not for the demo account.".into());
        }
        let symbol = clean_symbol(&req.symbol).ok_or("Enter a symbol.")?;
        let quote = quote.filter(|q| positive(*q));
        let price = req
            .reference_price(quote)
            .ok_or_else(|| format!("Waiting for a price for {symbol}; try again in a moment or use a limit order."))?;
        let units = match req.amount {
            Amount::Units(u) => u,
            // Rounded down, so a dollar amount never costs more than it says.
            Amount::Notional(n) => (n / price * 1e6).floor() / 1e6,
        };
        Ok(Estimate {
            units,
            price,
            value: units * price,
            buying_power: self.buying_power(),
            available: self.available(&symbol),
        })
    }

    /// Place an order. Market orders (and limits that are already marketable) fill at once at
    /// `quote`; the rest rest as `PENDING`. Returns the order as it stands afterwards.
    pub fn submit(&mut self, req: &OrderRequest, quote: Option<f64>, now: DateTime<Utc>) -> Result<Order, String> {
        let est = self.estimate(req, quote)?;
        let symbol = clean_symbol(&req.symbol).expect("validated");
        if !positive(est.units) {
            return Err("Quantity must be greater than zero.".into());
        }
        match req.side {
            Side::Buy if est.value > est.buying_power + EPS => {
                return Err(format!(
                    "Not enough buying power: this costs about ${:.2} and you have ${:.2}.",
                    est.value, est.buying_power
                ));
            }
            Side::Sell if est.units > est.available + EPS => {
                let held = self.held(&symbol);
                return Err(if held <= EPS {
                    format!("You don't hold any {symbol}. The demo account can't sell short.")
                } else {
                    format!(
                        "You can sell up to {} {symbol} ({} held{}).",
                        fmt_units(round_units(est.available)),
                        fmt_units(round_units(held)),
                        if est.available + EPS < held { ", the rest is in open sell orders" } else { "" }
                    )
                });
            }
            _ => {}
        }
        self.seq += 1;
        let order = Order {
            account_id: ACCOUNT_ID.into(),
            id: format!("demo-{}", self.seq),
            symbol: symbol.clone(),
            description: String::new(),
            kind: InstrumentKind::Stock,
            action: req.side.api().into(),
            status: OPEN.into(),
            order_type: req.order_type.api().into(),
            time_in_force: req.time_in_force.api().into(),
            quantity: Some(round_units(est.units)),
            filled: Some(0.0),
            limit_price: req.limit.filter(|_| req.order_type.needs_limit()),
            stop_price: req.stop.filter(|_| req.order_type.needs_stop()),
            execution_price: None,
            placed: Some(now),
            updated: Some(now),
            underlying: None,
        };
        let id = order.id.clone();
        self.orders.insert(0, order);
        if req.order_type == OrderType::Market {
            let px = quote.filter(|q| positive(*q)).expect("estimate needs a quote for market orders");
            if let Err(e) = self.fill(0, px, now) {
                self.orders.remove(0);
                return Err(e);
            }
        } else if let Some(px) = quote.filter(|q| positive(*q)) {
            // A limit or stop that is already through the market behaves as it would at a broker.
            self.evaluate(0, px, now);
        }
        // Immediate-or-cancel and fill-or-kill never rest.
        let i = self.index(&id).expect("just inserted");
        if self.orders[i].is_open() && matches!(req.time_in_force, TimeInForce::Ioc | TimeInForce::Fok) {
            self.orders[i].status = CANCELED.into();
        }
        // Copy the result first: trimming a full history may drop this (finished) order.
        let placed = self.orders[i].clone();
        self.trim();
        self.updated = Some(now);
        Ok(placed)
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.orders.iter().position(|o| o.id == id)
    }

    /// Cancel a resting order.
    pub fn cancel(&mut self, id: &str, now: DateTime<Utc>) -> Result<Order, String> {
        let i = self.index(id).ok_or("Order not found.")?;
        let o = &mut self.orders[i];
        if !o.is_open() {
            return Err(format!("The order is {} and cannot be cancelled.", o.status.to_ascii_lowercase()));
        }
        o.status = CANCELED.into();
        o.updated = Some(now);
        self.updated = Some(now);
        Ok(o.clone())
    }

    /// A market order that sells the whole position (shares not already in open sells).
    pub fn close_request(&self, symbol: &str) -> Option<OrderRequest> {
        let units = round_units(self.available(symbol));
        positive(units).then(|| OrderRequest {
            account_id: ACCOUNT_ID.into(),
            symbol: symbol.to_ascii_uppercase(),
            side: Side::Sell,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            amount: Amount::Units(units),
            limit: None,
            stop: None,
        })
    }

    /// A new price for `symbol`: marks the position and fills or triggers resting orders it
    /// crosses, oldest first. Day orders past their close expire first, so a quote that
    /// arrives late (after sleep, say) never fills an order that should have expired.
    pub fn on_price(&mut self, symbol: &str, price: f64, now: DateTime<Utc>) -> Vec<Event> {
        if !positive(price) {
            return Vec::new();
        }
        for p in self.positions.iter_mut().filter(|p| same(&p.symbol, symbol)) {
            p.price = Some(price);
        }
        let mut events = self.expire(now);
        let ids: Vec<String> =
            self.orders.iter().rev().filter(|o| o.is_open() && same(&o.symbol, symbol)).map(|o| o.id.clone()).collect();
        for id in ids {
            let Some(i) = self.index(&id) else { continue };
            if let Some(e) = self.evaluate(i, price, now) {
                events.push(e);
            }
        }
        if !events.is_empty() {
            self.trim();
            self.updated = Some(now);
        }
        events
    }

    /// Expire Day orders whose session has closed.
    pub fn expire(&mut self, now: DateTime<Utc>) -> Vec<Event> {
        let mut events = Vec::new();
        for o in self.orders.iter_mut().filter(|o| o.is_open() && o.time_in_force == TimeInForce::Day.api()) {
            if o.placed.is_some_and(|t| now >= day_expiry(t)) {
                o.status = EXPIRED.into();
                o.updated = Some(now);
                events.push(Event::Expired(o.clone()));
            }
        }
        if !events.is_empty() {
            self.updated = Some(now);
        }
        events
    }

    /// Symbols that need quotes: positions to mark and open orders to watch.
    pub fn watched(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .positions
            .iter()
            .map(|p| p.symbol.clone())
            .chain(self.orders.iter().filter(|o| o.is_open()).map(|o| o.symbol.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Check resting order `i` against `price`; fill, trigger or leave it.
    fn evaluate(&mut self, i: usize, price: f64, now: DateTime<Utc>) -> Option<Event> {
        let o = &self.orders[i];
        let buy = o.is_buy();
        let kind = o.order_type.clone();
        let crosses = |level: f64| if buy { price <= level + EPS } else { price >= level - EPS };
        let stop_hit = |stop: f64| if buy { price >= stop - EPS } else { price <= stop + EPS };
        let triggered = o.status == TRIGGERED;
        let fill_now = match kind.as_str() {
            "Limit" => o.limit_price.is_some_and(crosses),
            "Stop" => o.stop_price.is_some_and(stop_hit),
            "StopLimit" => {
                if !triggered && o.stop_price.is_some_and(stop_hit) {
                    self.orders[i].status = TRIGGERED.into();
                    self.orders[i].updated = Some(now);
                }
                self.orders[i].status == TRIGGERED && self.orders[i].limit_price.is_some_and(crosses)
            }
            _ => true,
        };
        if !fill_now {
            return None;
        }
        Some(match self.fill(i, price, now) {
            Ok(()) => Event::Filled(self.orders[i].clone()),
            Err(why) => {
                let o = &mut self.orders[i];
                o.status = REJECTED.into();
                o.updated = Some(now);
                Event::Rejected(o.clone(), why)
            }
        })
    }

    /// Execute order `i` in full at `px`: move cash, update the position, record the fill.
    fn fill(&mut self, i: usize, px: f64, now: DateTime<Utc>) -> Result<(), String> {
        let (buy, units, symbol) = {
            let o = &self.orders[i];
            (o.is_buy(), o.quantity.unwrap_or(0.0), o.symbol.clone())
        };
        let value = units * px;
        if buy {
            // This order's own reservation is part of the cash it may use.
            let own = self.orders[i].limit_price.or(self.orders[i].stop_price).map_or(0.0, |l| l * units);
            let available = self.cash - (self.reserved() - own);
            if value > available + EPS {
                return Err(format!("not enough cash (needs ${value:.2}, ${:.2} available)", available.max(0.0)));
            }
            self.cash -= value;
            match self.positions.iter_mut().find(|p| same(&p.symbol, &symbol)) {
                Some(p) => {
                    let total = p.units + units;
                    p.cost_basis = Some((p.cost_basis.unwrap_or(px) * p.units + value) / total);
                    p.units = total;
                    p.price = Some(px);
                }
                None => self.positions.push(Position {
                    account_id: ACCOUNT_ID.into(),
                    symbol: symbol.clone(),
                    description: String::new(),
                    kind: InstrumentKind::Stock,
                    units,
                    price: Some(px),
                    cost_basis: Some(px),
                    multiplier: 1.0,
                    currency: "USD".into(),
                    cash_equivalent: false,
                    underlying: None,
                }),
            }
        } else {
            let Some(pi) = self.positions.iter().position(|p| same(&p.symbol, &symbol)) else {
                return Err(format!("no {symbol} shares left to sell"));
            };
            let p = &mut self.positions[pi];
            if units > p.units + EPS {
                return Err(format!("only {} {symbol} left to sell", fmt_units(round_units(p.units))));
            }
            self.realized += (px - p.cost_basis.unwrap_or(px)) * units;
            self.cash += value;
            p.units -= units;
            p.price = Some(px);
            if p.units.abs() < EPS {
                self.positions.remove(pi);
            }
        }
        let o = &mut self.orders[i];
        o.status = FILLED.into();
        o.filled = Some(units);
        o.execution_price = Some(px);
        o.updated = Some(now);
        self.seq += 1;
        self.activities.insert(
            0,
            Activity {
                account_id: ACCOUNT_ID.into(),
                id: format!("demo-fill-{}", self.seq),
                kind: o.action.clone(),
                symbol: Some(symbol),
                description: format!("Demo fill: {}", order_line(o)),
                units: Some(if buy { units } else { -units }),
                price: Some(px),
                amount: Some(if buy { -value } else { value }),
                fee: Some(0.0),
                currency: "USD".into(),
                trade_date: Some(now),
                settlement_date: Some(now),
            },
        );
        Ok(())
    }

    /// Keep the history bounded, dropping the oldest finished orders (never an open one).
    fn trim(&mut self) {
        while self.orders.len() > HISTORY {
            match self.orders.iter().rposition(|o| !o.is_open()) {
                Some(i) => {
                    self.orders.remove(i);
                }
                None => break,
            }
        }
        self.activities.truncate(HISTORY);
    }
}

/// Share counts are kept to 6 decimals so notional orders do not accumulate float dust.
fn round_units(u: f64) -> f64 {
    (u * 1e6).round() / 1e6
}
