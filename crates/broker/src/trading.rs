//! Order entry through SnapTrade: order tickets, validation, the order-impact preview, placing
//! a previewed order and cancelling open orders, plus an offline simulator for the sample
//! portfolio.
//!
//! Placing an order always goes through [`SnapTrade::order_impact`] first: SnapTrade returns a
//! `Trade` id (valid five minutes) together with the estimated commission and remaining cash,
//! and only that previewed trade can be placed ([`SnapTrade::place_previewed`]). There is no
//! call that places an order without a preview. Nothing in this crate decides to trade: the
//! terminal gates every call behind an explicit setting and a confirmation dialog.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{self, InstrumentKind, Order, Portfolio, num};
use crate::snaptrade::SnapTrade;

/// How long SnapTrade keeps a previewed trade before rejecting it.
pub const PREVIEW_TTL_SECS: i64 = 5 * 60;
/// Treat a preview as stale a little before SnapTrade does, so a confirm that takes a few
/// seconds to reach the server is not rejected.
pub const PREVIEW_MARGIN_SECS: i64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn api(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Buy => "Buy",
            Side::Sell => "Sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum OrderType {
    #[default]
    Market,
    Limit,
    Stop,
    StopLimit,
}

impl OrderType {
    pub const ALL: [OrderType; 4] = [OrderType::Market, OrderType::Limit, OrderType::Stop, OrderType::StopLimit];

    pub fn api(self) -> &'static str {
        match self {
            OrderType::Market => "Market",
            OrderType::Limit => "Limit",
            OrderType::Stop => "Stop",
            OrderType::StopLimit => "StopLimit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OrderType::Market => "Market",
            OrderType::Limit => "Limit",
            OrderType::Stop => "Stop",
            OrderType::StopLimit => "Stop Limit",
        }
    }

    pub fn needs_limit(self) -> bool {
        matches!(self, OrderType::Limit | OrderType::StopLimit)
    }

    pub fn needs_stop(self) -> bool {
        matches!(self, OrderType::Stop | OrderType::StopLimit)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TimeInForce {
    #[default]
    Day,
    Gtc,
    Fok,
    Ioc,
}

impl TimeInForce {
    pub const ALL: [TimeInForce; 4] = [TimeInForce::Day, TimeInForce::Gtc, TimeInForce::Fok, TimeInForce::Ioc];

    pub fn api(self) -> &'static str {
        match self {
            TimeInForce::Day => "Day",
            TimeInForce::Gtc => "GTC",
            TimeInForce::Fok => "FOK",
            TimeInForce::Ioc => "IOC",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TimeInForce::Day => "Day",
            TimeInForce::Gtc => "GTC",
            TimeInForce::Fok => "FOK",
            TimeInForce::Ioc => "IOC",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            TimeInForce::Day => "Day: expires at the end of the trading day",
            TimeInForce::Gtc => "Good 'til cancelled",
            TimeInForce::Fok => "Fill or kill: all at once or not at all",
            TimeInForce::Ioc => "Immediate or cancel: fill what is possible now, cancel the rest",
        }
    }
}

/// Order size: a number of units (shares, coins) or a cash amount.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Amount {
    Units(f64),
    /// Dollar amount (notional). Market + Day only, and not every broker supports it.
    Notional(f64),
}

/// Everything needed to preview an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub account_id: String,
    /// Broker ticker (`AAPL`, `BRK.B`, `VAB.TO`).
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub amount: Amount,
    pub limit: Option<f64>,
    pub stop: Option<f64>,
}

fn positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

/// Ids SnapTrade issues are UUIDs or broker order numbers; anything else is refused before
/// it reaches a URL path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.contains("..")
}

/// Upper-cased ticker if it looks like one SnapTrade understands.
pub fn clean_symbol(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_uppercase();
    (!s.is_empty() && s.len() <= 21 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '/')))
        .then_some(s)
}

impl OrderRequest {
    /// Check the ticket before anything is sent. Returns a message for the user.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.account_id) {
            return Err("Choose an account.".into());
        }
        if clean_symbol(&self.symbol).is_none() {
            return Err("Enter a symbol.".into());
        }
        match self.amount {
            Amount::Units(u) if !positive(u) => return Err("Quantity must be greater than zero.".into()),
            Amount::Notional(n) if !positive(n) => return Err("Amount must be greater than zero.".into()),
            Amount::Notional(_) if self.order_type != OrderType::Market || self.time_in_force != TimeInForce::Day => {
                return Err("Dollar amounts work only for Market orders with Day time in force.".into());
            }
            _ => {}
        }
        if self.order_type == OrderType::Market && matches!(self.time_in_force, TimeInForce::Gtc) {
            return Err("Market orders cannot be good 'til cancelled.".into());
        }
        match (self.order_type.needs_limit(), self.limit) {
            (true, None) => return Err("Enter a limit price.".into()),
            (true, Some(p)) if !positive(p) => return Err("The limit price must be greater than zero.".into()),
            _ => {}
        }
        match (self.order_type.needs_stop(), self.stop) {
            (true, None) => return Err("Enter a stop price.".into()),
            (true, Some(p)) if !positive(p) => return Err("The stop price must be greater than zero.".into()),
            _ => {}
        }
        if self.order_type == OrderType::StopLimit
            && let (Some(l), Some(s)) = (self.limit, self.stop)
        {
            // A buy stop-limit triggers on a rise, so its limit is at or above the stop; a
            // sell stop-limit the reverse. Anything else can never fill as intended.
            let ok = match self.side {
                Side::Buy => l >= s,
                Side::Sell => l <= s,
            };
            if !ok {
                return Err(match self.side {
                    Side::Buy => "For a buy stop-limit the limit price should be at or above the stop.".into(),
                    Side::Sell => "For a sell stop-limit the limit price should be at or below the stop.".into(),
                });
            }
        }
        Ok(())
    }

    /// The `/trade/impact` body for this request.
    pub fn impact_body(&self, universal_symbol_id: &str) -> Value {
        let (units, notional) = match self.amount {
            Amount::Units(u) => (json!(u), Value::Null),
            Amount::Notional(n) => (Value::Null, json!(n)),
        };
        json!({
            "account_id": self.account_id,
            "action": self.side.api(),
            "universal_symbol_id": universal_symbol_id,
            "order_type": self.order_type.api(),
            "time_in_force": self.time_in_force.api(),
            "price": if self.order_type.needs_limit() { json!(self.limit) } else { Value::Null },
            "stop": if self.order_type.needs_stop() { json!(self.stop) } else { Value::Null },
            "units": units,
            "notional_value": notional,
        })
    }

    /// The price the order is expected to fill near: the limit, else the stop, else `quote`.
    pub fn reference_price(&self, quote: Option<f64>) -> Option<f64> {
        self.limit
            .filter(|_| self.order_type.needs_limit())
            .or(self.stop.filter(|_| self.order_type.needs_stop()))
            .or(quote)
            .filter(|p| positive(*p))
    }

    /// One line: `BUY 10 AAPL LMT 150.00 DAY`.
    pub fn summary(&self) -> String {
        let size = match self.amount {
            Amount::Units(u) => fmt_units(u),
            Amount::Notional(n) => format!("${n:.2} of"),
        };
        let mut s = format!("{} {size} {}", self.side.api(), self.symbol);
        match self.order_type {
            OrderType::Market => s += " MKT",
            OrderType::Limit => s += &format!(" LMT {:.2}", self.limit.unwrap_or(0.0)),
            OrderType::Stop => s += &format!(" STP {:.2}", self.stop.unwrap_or(0.0)),
            OrderType::StopLimit => {
                s += &format!(" STP {:.2} LMT {:.2}", self.stop.unwrap_or(0.0), self.limit.unwrap_or(0.0))
            }
        }
        s + " " + &self.time_in_force.api().to_ascii_uppercase()
    }
}

pub fn fmt_units(u: f64) -> String {
    if u.fract() == 0.0 { format!("{}", u as i64) } else { format!("{u}") }
}

/// A SnapTrade universal symbol the account's broker can trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolMatch {
    pub id: String,
    pub symbol: String,
    pub description: String,
    pub currency: String,
    pub exchange: String,
}

fn text(v: &Value) -> String {
    v.as_str().map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn parse_symbols(v: &Value) -> Vec<SymbolMatch> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let id = text(&s["id"]);
            (!id.is_empty()).then(|| SymbolMatch {
                id,
                symbol: text(&s["symbol"]),
                description: text(&s["description"]),
                currency: Some(text(&s["currency"]["code"])).filter(|c| !c.is_empty()).unwrap_or("USD".into()),
                exchange: Some(text(&s["exchange"]["code"]))
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| text(&s["exchange"]["name"])),
            })
        })
        .collect()
}

/// The exact ticker among search results (search also matches names and prefixes).
pub fn pick_symbol(results: &[SymbolMatch], ticker: &str) -> Option<SymbolMatch> {
    let want = ticker.trim().to_ascii_uppercase();
    let alt = want.replace('/', ".");
    results.iter().find(|s| s.symbol.eq_ignore_ascii_case(&want) || s.symbol.eq_ignore_ascii_case(&alt)).cloned()
}

/// SnapTrade's estimate of an order, before it is placed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderPreview {
    /// Trade id to place within [`PREVIEW_TTL_SECS`].
    pub trade_id: String,
    pub request: OrderRequest,
    pub symbol: SymbolMatch,
    /// Price SnapTrade or the broker quoted for the trade, if any.
    pub price: Option<f64>,
    /// Units SnapTrade will send (it fills these in for notional orders on some brokers).
    pub units: Option<f64>,
    pub commission: Option<f64>,
    pub forex_fees: Option<f64>,
    pub remaining_cash: Option<f64>,
    pub currency: String,
    pub created: DateTime<Utc>,
    /// Computed locally (sample portfolio), not by a broker.
    pub simulated: bool,
}

impl OrderPreview {
    pub fn expires(&self) -> DateTime<Utc> {
        self.created + Duration::seconds(PREVIEW_TTL_SECS - PREVIEW_MARGIN_SECS)
    }

    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires()
    }

    /// Estimated cash moved by the order before fees: units × price, or the notional amount.
    /// `quote` is the terminal's last price, used when the order carries no price of its own.
    pub fn estimated_value(&self, quote: Option<f64>) -> Option<f64> {
        if let Amount::Notional(n) = self.request.amount {
            return Some(n);
        }
        let units = self.units.or(match self.request.amount {
            Amount::Units(u) => Some(u),
            Amount::Notional(_) => None,
        })?;
        let price = self.request.reference_price(self.price.or(quote))?;
        Some(units * price)
    }

    /// Estimated total: value plus commission and fees for a buy, minus them for a sell.
    pub fn estimated_total(&self, quote: Option<f64>) -> Option<f64> {
        let fees = self.commission.unwrap_or(0.0) + self.forex_fees.unwrap_or(0.0);
        let v = self.estimated_value(quote)?;
        Some(match self.request.side {
            Side::Buy => v + fees,
            Side::Sell => v - fees,
        })
    }
}

/// Parse a `/trade/impact` response.
pub fn parse_impact(req: &OrderRequest, symbol: SymbolMatch, v: &Value, now: DateTime<Utc>) -> Result<OrderPreview> {
    let trade = &v["trade"];
    let trade_id = text(&trade["id"]);
    if !valid_id(&trade_id) {
        bail!("SnapTrade did not return a trade id for the preview");
    }
    let impact = v["trade_impacts"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
    let currency = Some(text(&v["combined_remaining_balance"]["currency"]["code"]))
        .filter(|c| !c.is_empty())
        .or_else(|| Some(text(&trade["symbol"]["currency"]["code"])).filter(|c| !c.is_empty()))
        .unwrap_or_else(|| symbol.currency.clone());
    Ok(OrderPreview {
        trade_id,
        request: req.clone(),
        symbol,
        price: num(&trade["price"]).filter(|p| *p > 0.0),
        units: num(&trade["units"]).filter(|u| *u > 0.0),
        commission: num(&impact["estimated_commission"]),
        forex_fees: num(&impact["forex_fees"]),
        remaining_cash: num(&impact["remaining_cash"]).or_else(|| num(&v["combined_remaining_balance"]["cash"])),
        currency,
        created: now,
        simulated: false,
    })
}

/// The order a placement returned (`AccountOrderRecord`), in the portfolio model.
pub fn parse_placed(account_id: &str, v: &Value) -> Option<Order> {
    model::parse_orders(account_id, &Value::Array(vec![v.clone()])).into_iter().next()
}

/// What an account can do, from its brokerage and connection (`/authorizations`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub brokerage: String,
    /// The brokerage supports trading through SnapTrade at all.
    pub broker_trading: bool,
    /// The connection was made with trading access (`type: trade`).
    pub connection_trading: bool,
    pub fractional: bool,
    /// The connection needs reconnecting.
    pub disabled: bool,
}

impl Capability {
    pub fn can_trade(&self) -> bool {
        self.broker_trading && self.connection_trading && !self.disabled
    }

    /// Why orders cannot be sent, for the ticket.
    pub fn blocker(&self) -> Option<String> {
        if !self.broker_trading {
            Some(format!("{} does not support trading through SnapTrade (read-only).", self.brokerage))
        } else if self.disabled {
            Some(format!("The {} connection is disabled; reconnect it in Settings › Brokerage.", self.brokerage))
        } else if !self.connection_trading {
            Some(format!(
                "The {} connection is read-only. Reconnect with trading access (Settings › Trading).",
                self.brokerage
            ))
        } else {
            None
        }
    }
}

/// Capabilities per brokerage connection (SnapTrade authorization id) from `/authorizations`,
/// where each connection carries its brokerage and access type. Accounts map onto these
/// through [`crate::Account::connection_id`], so one request covers every account.
pub fn parse_connection_capabilities(authorizations: &Value) -> std::collections::HashMap<String, Capability> {
    authorizations
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let b = &c["brokerage"];
            let brokerage = [&b["display_name"], &b["name"], &b["slug"]]
                .iter()
                .map(|v| text(v))
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| "The brokerage".into());
            Some((
                Some(text(&c["id"])).filter(|s| !s.is_empty())?,
                Capability {
                    brokerage,
                    broker_trading: b["allows_trading"].as_bool().unwrap_or(false),
                    connection_trading: c["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("trade")),
                    fractional: b["allows_fractional_units"].as_bool().unwrap_or(false),
                    disabled: c["disabled"].as_bool().unwrap_or(false),
                },
            ))
        })
        .collect()
}

/// Per-account capabilities from `/accounts` (each account names its `brokerage_authorization`)
/// and `/authorizations`.
pub fn parse_capabilities(accounts: &Value, authorizations: &Value) -> std::collections::HashMap<String, Capability> {
    let conns = parse_connection_capabilities(authorizations);
    accounts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let conn = text(&a["brokerage_authorization"]);
            let cap = conns.get(&conn)?.clone();
            Some((Some(text(&a["id"])).filter(|s| !s.is_empty())?, cap))
        })
        .collect()
}

/// Why a placement did not clearly succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceError {
    /// Refused before anything was sent.
    NotSent(String),
    /// SnapTrade or the broker rejected the order (4xx): nothing was placed.
    Rejected(String),
    /// The request may have reached the broker but no clear answer came back: the order may
    /// exist. Check open orders before placing again.
    Unknown(String),
}

impl PlaceError {
    pub fn uncertain(&self) -> bool {
        matches!(self, PlaceError::Unknown(_))
    }

    pub fn message(&self) -> &str {
        match self {
            PlaceError::NotSent(m) | PlaceError::Rejected(m) | PlaceError::Unknown(m) => m,
        }
    }
}

impl std::fmt::Display for PlaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// Only a client error (4xx) is a definite "not placed"; everything else after sending is unknown.
pub fn classify_place_error(e: anyhow::Error) -> PlaceError {
    let msg = format!("{e:#}");
    if e.downcast_ref::<crate::snaptrade::NotSent>().is_some() {
        return PlaceError::NotSent(msg);
    }
    match e.downcast_ref::<crate::snaptrade::ApiError>() {
        Some(api) if (400..500).contains(&api.status) => PlaceError::Rejected(msg),
        _ => PlaceError::Unknown(msg),
    }
}

/// Validate a path segment before it is interpolated into a URL.
fn segment<'a>(what: &str, id: &'a str) -> Result<&'a str> {
    if valid_id(id) { Ok(id) } else { Err(anyhow!("invalid {what}")) }
}

impl SnapTrade {
    /// Look up `ticker` among the symbols the account's broker supports.
    pub async fn find_symbol(&self, account_id: &str, ticker: &str) -> Result<SymbolMatch> {
        let acct = segment("account id", account_id)?;
        let ticker = clean_symbol(ticker).context("invalid symbol")?;
        let v = self
            .call(Method::POST, &format!("/accounts/{acct}/symbols"), &[], Some(json!({ "substring": ticker })), true)
            .await?;
        let found = parse_symbols(&v);
        pick_symbol(&found, &ticker).ok_or_else(|| match found.len() {
            0 => anyhow!("{ticker} is not tradable at this broker"),
            _ => anyhow!(
                "{ticker} not found at this broker (did you mean {}?)",
                found.iter().take(4).map(|s| s.symbol.as_str()).collect::<Vec<_>>().join(", ")
            ),
        })
    }

    /// Ask SnapTrade (and through it the broker) to check an order and estimate its impact.
    /// Nothing is placed.
    pub async fn order_impact(&self, req: &OrderRequest) -> Result<OrderPreview> {
        req.validate().map_err(|e| anyhow!(e))?;
        let symbol = self.find_symbol(&req.account_id, &req.symbol).await?;
        let v = self.call(Method::POST, "/trade/impact", &[], Some(req.impact_body(&symbol.id)), true).await?;
        parse_impact(req, symbol, &v, Utc::now())
    }

    /// Place a previewed order. Refuses previews that have expired or are simulated.
    ///
    /// Once the request has gone out, only a 4xx answer is a definite rejection. A transport
    /// error, a lost or unreadable response, or a 5xx may hide an order the broker accepted, so
    /// those come back as [`PlaceError::Unknown`] and must not be retried blindly.
    pub async fn place_previewed(&self, preview: &OrderPreview) -> Result<Option<Order>, PlaceError> {
        if preview.simulated {
            return Err(PlaceError::NotSent("a simulated preview cannot be placed with a broker".into()));
        }
        if preview.expired(Utc::now()) {
            return Err(PlaceError::NotSent("the preview expired; preview the order again".into()));
        }
        let id = segment("trade id", &preview.trade_id).map_err(|e| PlaceError::NotSent(e.to_string()))?;
        // Never sent once the preview has expired, however long a rate-limit pause lasts.
        let left = (preview.expires() - Utc::now()).to_std().unwrap_or_default();
        let deadline = Some(std::time::Instant::now() + left);
        let body = Some(json!({ "wait_to_confirm": true }));
        match self.call_by(Method::POST, &format!("/trade/{id}"), &[], body, true, deadline).await {
            Ok(v) => Ok(parse_placed(&preview.request.account_id, &v)),
            Err(e) => Err(classify_place_error(e)),
        }
    }

    /// Cancel an open order.
    pub async fn cancel_order(&self, account_id: &str, brokerage_order_id: &str) -> Result<()> {
        let acct = segment("account id", account_id)?;
        let order = segment("order id", brokerage_order_id)?;
        let body = json!({ "brokerage_order_id": order });
        self.call(Method::POST, &format!("/accounts/{acct}/trading/cancel"), &[], Some(body), true).await.map(|_| ())
    }

    /// What each brokerage connection can do, in one request however many accounts it holds.
    pub async fn connection_capabilities(&self) -> Result<std::collections::HashMap<String, Capability>> {
        Ok(parse_connection_capabilities(&self.call(Method::GET, "/authorizations", &[], None, true).await?))
    }

    /// What each account can do: brokerage support for trading and the connection's access.
    pub async fn capabilities(&self) -> Result<std::collections::HashMap<String, Capability>> {
        let (accounts, auths) = futures::join!(
            self.call(Method::GET, "/accounts", &[], None, true),
            self.call(Method::GET, "/authorizations", &[], None, true)
        );
        Ok(parse_capabilities(&accounts?, &auths?))
    }

    /// A Connection Portal link that asks for trading access where the broker supports it
    /// (read-only otherwise), for connections that were made read-only.
    pub async fn trading_portal_url(&self, dark: bool) -> Result<String> {
        let body = json!({ "connectionType": "trade-if-available", "darkMode": dark });
        let v = self.call(Method::POST, "/snapTrade/login", &[], Some(body), true).await?;
        if v.get("encryptedSharedKey").is_some() {
            bail!("SnapTrade returned an encrypted login response; disable response encryption for this key");
        }
        v["redirectURI"].as_str().map(str::to_string).context("SnapTrade did not return a Connection Portal URL")
    }
}

// ---- offline simulator ---------------------------------------------------------------------

/// Preview against the simulated portfolio: no network, priced at `quote` (or the position's
/// last price), no commission.
pub fn simulate_preview(
    pf: &Portfolio,
    req: &OrderRequest,
    quote: Option<f64>,
    now: DateTime<Utc>,
) -> Result<OrderPreview, String> {
    req.validate()?;
    let account = pf.account(&req.account_id).ok_or("Unknown account.")?;
    let held = pf
        .positions
        .iter()
        .find(|p| p.account_id == req.account_id && p.symbol.eq_ignore_ascii_case(&req.symbol))
        .and_then(|p| p.price);
    let price = quote.or(held).filter(|p| positive(*p));
    let reference = req.reference_price(price).ok_or("No price for this symbol; use a limit order.")?;
    let units = match req.amount {
        Amount::Units(u) => u,
        Amount::Notional(n) => n / reference,
    };
    let value = units * reference;
    let cash = account.cash.unwrap_or(0.0);
    let remaining = match req.side {
        Side::Buy => cash - value,
        Side::Sell => cash + value,
    };
    let id = format!("sim-trade-{}", now.timestamp_nanos_opt().unwrap_or_default());
    Ok(OrderPreview {
        trade_id: id,
        request: req.clone(),
        symbol: SymbolMatch {
            id: format!("sim-{}", req.symbol),
            symbol: req.symbol.clone(),
            description: String::new(),
            currency: account.currency.clone(),
            exchange: "SIM".into(),
        },
        price,
        units: Some(units),
        commission: Some(0.0),
        forex_fees: None,
        remaining_cash: Some(remaining),
        currency: account.currency.clone(),
        created: now,
        simulated: true,
    })
}

/// Record a simulated order in the sample portfolio. Market orders fill at once (updating
/// cash); other orders rest as `PENDING` until cancelled.
pub fn simulate_place(pf: &mut Portfolio, preview: &OrderPreview, now: DateTime<Utc>) -> Result<Order, String> {
    if !preview.simulated || !pf.simulated {
        return Err("Only simulated previews can be placed in the sample portfolio.".into());
    }
    if preview.expired(now) {
        return Err("The preview expired; preview the order again.".into());
    }
    let req = &preview.request;
    let units = preview.units.unwrap_or(0.0);
    let market = req.order_type == OrderType::Market;
    let fill = market.then(|| req.reference_price(preview.price)).flatten();
    let order = Order {
        account_id: req.account_id.clone(),
        id: format!("sim-o-{}", now.timestamp_nanos_opt().unwrap_or_default()),
        symbol: req.symbol.clone(),
        description: String::new(),
        kind: InstrumentKind::Stock,
        action: req.side.api().into(),
        status: if fill.is_some() { "EXECUTED" } else { "PENDING" }.into(),
        order_type: req.order_type.api().into(),
        time_in_force: req.time_in_force.api().into(),
        quantity: Some(units),
        filled: Some(if fill.is_some() { units } else { 0.0 }),
        limit_price: req.limit.filter(|_| req.order_type.needs_limit()),
        stop_price: req.stop.filter(|_| req.order_type.needs_stop()),
        execution_price: fill,
        placed: Some(now),
        updated: Some(now),
        underlying: None,
        market: None,
    };
    if let Some(px) = fill {
        let signed = if req.side == Side::Buy { units } else { -units };
        if let Some(a) = pf.accounts.iter_mut().find(|a| a.id == req.account_id) {
            a.cash = a.cash.map(|c| c - signed * px);
            a.buying_power = a.buying_power.map(|b| b - signed * px);
        }
        apply_fill(pf, &req.account_id, &req.symbol, signed, px);
    }
    pf.orders.insert(0, order.clone());
    Ok(order)
}

/// Move a simulated position by `signed` units filled at `px`: adding to a position averages
/// its cost; reducing keeps it; crossing zero starts a new position at the fill price.
fn apply_fill(pf: &mut Portfolio, account: &str, symbol: &str, signed: f64, px: f64) {
    let held = pf.positions.iter().position(|p| p.account_id == account && p.symbol.eq_ignore_ascii_case(symbol));
    let Some(i) = held else {
        pf.positions.push(crate::model::Position {
            account_id: account.into(),
            symbol: symbol.into(),
            description: String::new(),
            kind: InstrumentKind::Stock,
            units: signed,
            price: Some(px),
            cost_basis: Some(px),
            multiplier: 1.0,
            currency: "USD".into(),
            cash_equivalent: false,
            underlying: None,
            market: None,
        });
        return;
    };
    let p = &mut pf.positions[i];
    let after = p.units + signed;
    if after.abs() < 1e-9 {
        pf.positions.remove(i);
        return;
    }
    p.cost_basis = if p.units.signum() == signed.signum() {
        p.cost_basis.map(|c| (c * p.units + px * signed) / after)
    } else if after.signum() != p.units.signum() {
        Some(px)
    } else {
        p.cost_basis
    };
    p.units = after;
    p.price = Some(px);
}

/// Cancel a resting simulated order.
pub fn simulate_cancel(pf: &mut Portfolio, account_id: &str, order_id: &str) -> Result<(), String> {
    let o = pf.orders.iter_mut().find(|o| o.account_id == account_id && o.id == order_id).ok_or("Order not found.")?;
    if !o.is_open() {
        return Err(format!("The order is {} and cannot be cancelled.", o.status.to_ascii_lowercase()));
    }
    o.status = "CANCELED".into();
    o.updated = Some(Utc::now());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> OrderRequest {
        OrderRequest {
            account_id: "sim-brokerage".into(),
            symbol: "AAPL".into(),
            side: Side::Buy,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            amount: Amount::Units(10.0),
            limit: None,
            stop: None,
        }
    }

    #[test]
    fn validation() {
        assert_eq!(req().validate(), Ok(()));
        let bad = |f: &dyn Fn(&mut OrderRequest)| {
            let mut r = req();
            f(&mut r);
            r.validate().unwrap_err()
        };
        assert!(bad(&|r| r.account_id.clear()).contains("account"));
        assert!(bad(&|r| r.account_id = "../x".into()).contains("account"));
        assert!(bad(&|r| r.symbol = " ".into()).contains("symbol"));
        assert!(bad(&|r| r.amount = Amount::Units(0.0)).contains("Quantity"));
        assert!(bad(&|r| r.amount = Amount::Units(f64::NAN)).contains("Quantity"));
        assert!(bad(&|r| r.order_type = OrderType::Limit).contains("limit price"));
        assert!(bad(&|r| r.order_type = OrderType::Stop).contains("stop price"));
        assert!(bad(&|r| r.time_in_force = TimeInForce::Gtc).contains("Market"));
        assert!(
            bad(&|r| {
                r.amount = Amount::Notional(500.0);
                r.order_type = OrderType::Limit;
                r.limit = Some(100.0);
            })
            .contains("Dollar")
        );
        assert!(
            bad(&|r| {
                r.order_type = OrderType::StopLimit;
                r.stop = Some(200.0);
                r.limit = Some(190.0);
            })
            .contains("at or above")
        );
        let mut sell = req();
        sell.side = Side::Sell;
        sell.order_type = OrderType::StopLimit;
        sell.stop = Some(200.0);
        sell.limit = Some(195.0);
        assert_eq!(sell.validate(), Ok(()));
    }

    #[test]
    fn impact_body_sends_only_relevant_prices() {
        let mut r = req();
        r.limit = Some(150.0);
        r.stop = Some(140.0);
        let b = r.impact_body("uuid-1");
        assert_eq!(b["order_type"], "Market");
        assert_eq!(b["price"], Value::Null, "a market order never sends a stale limit");
        assert_eq!(b["stop"], Value::Null);
        assert_eq!(b["units"], 10.0);
        assert_eq!(b["notional_value"], Value::Null);
        r.order_type = OrderType::StopLimit;
        r.side = Side::Sell;
        r.time_in_force = TimeInForce::Gtc;
        let b = r.impact_body("uuid-1");
        assert_eq!((b["action"].as_str(), b["time_in_force"].as_str()), (Some("SELL"), Some("GTC")));
        assert_eq!((b["price"].as_f64(), b["stop"].as_f64()), (Some(150.0), Some(140.0)));
        r.amount = Amount::Notional(250.0);
        let b = r.impact_body("uuid-1");
        assert_eq!((b["units"].clone(), b["notional_value"].as_f64()), (Value::Null, Some(250.0)));
        assert_eq!(req().summary(), "BUY 10 AAPL MKT DAY");
    }

    #[test]
    fn symbol_matching_is_exact() {
        let v = json!([
            {"id": "1", "symbol": "AAPL.NE", "currency": {"code": "CAD"}, "exchange": {"code": "NEO"}},
            {"id": "2", "symbol": "AAPL", "description": "Apple Inc", "currency": {"code": "USD"}, "exchange": {"code": "NASDAQ"}},
            {"symbol": "NOID"}
        ]);
        let found = parse_symbols(&v);
        assert_eq!(found.len(), 2);
        assert_eq!(pick_symbol(&found, "aapl").unwrap().id, "2");
        assert_eq!(pick_symbol(&found, "AAP"), None);
        let brk = parse_symbols(&json!([{"id": "9", "symbol": "BRK.B"}]));
        assert_eq!(pick_symbol(&brk, "BRK/B").unwrap().id, "9");
        assert_eq!(clean_symbol(" brk.b "), Some("BRK.B".into()));
        assert_eq!(clean_symbol("A;B"), None);
        assert!(valid_id("917c8734-8470-4a3e-a18f-57c3f2ee6631") && valid_id("1234567890"));
        assert!(!valid_id("../trade") && !valid_id("a/b") && !valid_id(""));
    }

    #[test]
    fn impact_parsing_and_estimates() {
        let v = json!({
            "trade": {"id": "139e307a-82f7-4402-b39e-4da7baa87758", "account": "a", "order_type": "Limit",
                      "time_in_force": "Day", "action": "BUY", "units": 10, "price": 150.5,
                      "symbol": {"universal_symbol_id": "2", "currency": {"code": "USD"}}},
            "trade_impacts": [{"account": "a", "currency": "c", "remaining_cash": 500.25,
                               "estimated_commission": 1.0, "forex_fees": 0.0}],
            "combined_remaining_balance": {"currency": {"code": "USD"}, "cash": 500.25}
        });
        let mut r = req();
        r.order_type = OrderType::Limit;
        r.limit = Some(150.5);
        let sym = SymbolMatch {
            id: "2".into(),
            symbol: "AAPL".into(),
            description: String::new(),
            currency: "USD".into(),
            exchange: String::new(),
        };
        let now = Utc::now();
        let p = parse_impact(&r, sym.clone(), &v, now).unwrap();
        assert_eq!(p.trade_id, "139e307a-82f7-4402-b39e-4da7baa87758");
        assert_eq!((p.commission, p.remaining_cash), (Some(1.0), Some(500.25)));
        assert_eq!(p.estimated_value(None), Some(1505.0));
        assert_eq!(p.estimated_total(None), Some(1506.0));
        assert!(!p.expired(now + Duration::seconds(200)));
        assert!(p.expired(now + Duration::seconds(PREVIEW_TTL_SECS)));
        // A market order without a broker price is estimated at the terminal's quote.
        let m = parse_impact(&req(), sym.clone(), &json!({"trade": {"id": "t1"}}), now).unwrap();
        assert_eq!(m.estimated_value(None), None);
        assert_eq!(m.estimated_value(Some(100.0)), Some(1000.0));
        assert!(parse_impact(&req(), sym, &json!({"trade": {}}), now).is_err(), "no trade id, nothing to place");
    }

    #[test]
    fn capabilities_follow_brokerage_and_connection() {
        let accounts = json!([
            {"id": "ws-1", "brokerage_authorization": "c-ws"},
            {"id": "ws-2", "brokerage_authorization": "c-ws"},
            {"id": "rh-1", "brokerage_authorization": "c-rh"},
            {"id": "x-1", "brokerage_authorization": "c-ro"},
            {"id": "orphan", "brokerage_authorization": "missing"}
        ]);
        let auths = json!([
            {"id": "c-ws", "type": "trade", "disabled": false,
             "brokerage": {"display_name": "Wealthsimple", "slug": "WEALTHSIMPLETRADE", "allows_trading": true, "allows_fractional_units": true}},
            {"id": "c-rh", "type": "read", "disabled": false,
             "brokerage": {"name": "Robinhood", "allows_trading": true}},
            {"id": "c-ro", "type": "trade", "disabled": true,
             "brokerage": {"name": "Readonly Bank", "allows_trading": false}}
        ]);
        let caps = parse_capabilities(&accounts, &auths);
        assert_eq!(caps.len(), 4);
        assert!(caps["ws-1"].can_trade() && caps["ws-2"].fractional);
        assert_eq!(caps["ws-1"].blocker(), None);
        assert!(!caps["rh-1"].can_trade());
        assert!(caps["rh-1"].blocker().unwrap().contains("read-only. Reconnect"));
        assert!(caps["x-1"].blocker().unwrap().contains("does not support trading"));
        assert!(!caps.contains_key("orphan"));
    }

    #[test]
    fn simulator_round_trip() {
        let mut pf = crate::sample::portfolio();
        let now = Utc::now();
        let cash0 = pf.account("sim-brokerage").unwrap().cash.unwrap();
        let bp0 = pf.account("sim-brokerage").unwrap().buying_power.unwrap();
        let p = simulate_preview(&pf, &req(), Some(100.0), now).unwrap();
        assert!(p.simulated);
        assert_eq!(p.estimated_total(None), Some(1000.0));
        assert_eq!(p.remaining_cash, Some(cash0 - 1000.0));
        let o = simulate_place(&mut pf, &p, now).unwrap();
        assert_eq!((o.status.as_str(), o.execution_price), ("EXECUTED", Some(100.0)));
        assert_eq!(pf.account("sim-brokerage").unwrap().cash, Some(cash0 - 1000.0));
        assert_eq!(pf.account("sim-brokerage").unwrap().buying_power, Some(bp0 - 1000.0), "buying power follows");
        let aapl = pf.positions.iter().find(|p| p.account_id == "sim-brokerage" && p.symbol == "AAPL").unwrap();
        assert_eq!(aapl.units, 130.0, "the fill adds to the position");
        assert!((aapl.cost_basis.unwrap() - (187.40 * 120.0 + 1000.0) / 130.0).abs() < 1e-9);
        assert!(simulate_place(&mut pf, &p, now + Duration::seconds(PREVIEW_TTL_SECS)).is_err());

        let mut lim = req();
        lim.side = Side::Sell;
        lim.order_type = OrderType::Limit;
        lim.time_in_force = TimeInForce::Gtc;
        lim.limit = Some(400.0);
        let p = simulate_preview(&pf, &lim, None, now).unwrap();
        let o = simulate_place(&mut pf, &p, now).unwrap();
        assert!(o.is_open());
        simulate_cancel(&mut pf, "sim-brokerage", &o.id).unwrap();
        assert_eq!(pf.orders[0].status, "CANCELED");
        assert!(simulate_cancel(&mut pf, "sim-brokerage", &o.id).is_err(), "already cancelled");

        let mut live = p.clone();
        live.simulated = false;
        assert!(simulate_place(&mut pf, &live, now).is_err(), "real previews never touch the sample");
        assert!(simulate_preview(&pf, &OrderRequest { symbol: "ZZZZ".into(), ..req() }, None, now).is_err());
    }
}
