//! Hyperliquid inside LIVE, **testnet only**: the account next to SnapTrade's in the
//! portfolio panes, the agent (API wallet) lifecycle, the ticket's Hyperliquid variant with
//! one confirmation per action, the kill switch and the opt-in dead man's switch.
//!
//! Safety rails, in order:
//! 1. Off until enabled in Settings › Trading › Hyperliquid, with its own acknowledgement
//!    (SnapTrade's switch never turns it on, or the reverse).
//! 2. Orders are signed only by a Terminal-generated agent key the main wallet approved; the
//!    key lives in the macOS Keychain, the Linux Secret Service, or browser memory.
//! 3. Every order, modify, cancel, cancel-all and leverage change opens one confirmation.
//! 4. `tr_hl` checks the hard caps ($100 per order, 3× leverage, 1% market slippage) when the
//!    confirmation opens, and the worker checks them again against the market just before
//!    signing.
//! 5. `KILL` (or the button in `ORD`) cancels every open order and turns Hyperliquid trading
//!    off until it is enabled again.
//!
//! Testnet rows are priced only from testnet mids ([`HlState::mids`]), never from the
//! Terminal server's mainnet quotes, though both use the same market keys (`xyz:AAPL`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tr_broker::secrets::AgentKeyStore;
use tr_broker::{Account, Activity, InstrumentKind, Order, Portfolio, Position};
use tr_hl::actions::Tif;
use tr_hl::assets::{Asset, AssetRegistry, app_key};
use tr_hl::model::AssetData;
use tr_hl::nonce::{NonceManager, now_ms};
use tr_hl::stream::UserState;
use tr_hl::trader::{self, MarketCtx, OrderIntent, OrderKind, OrderPlan};
use tr_hl::{AgentKey, MainKey, SigningKey};
use web_time::Instant;
use zeroize::Zeroizing;

use super::TerminalApp;
use crate::hl_worker::{HlAct, HlRequest, HlResponse, HlRoles, Outcome};
use crate::prefs::HlAgent;
use crate::worker::Request;

/// Account ids of Hyperliquid testnet accounts: `hl:testnet:0x…`.
pub const ACCOUNT_PREFIX: &str = "hl:testnet:";
/// `Account::connection_id` of Hyperliquid accounts (not a SnapTrade connection).
pub const CONNECTION_ID: &str = "hyperliquid";
/// How often the ticket re-reads leverage and the mid while it's open.
const MARKET_EVERY: Duration = Duration::from_secs(10);
/// The dead man's switch keeps a cancel this far ahead, refreshed every half of it.
const DEADMAN_AHEAD_MS: u64 = 60_000;

pub fn account_id(master: &str) -> String {
    format!("{ACCOUNT_PREFIX}{}", master.trim().to_ascii_lowercase())
}

pub fn is_hl_account(id: &str) -> bool {
    id.starts_with("hl:")
}

/// A testnet account (every Hyperliquid account in this build).
pub fn is_testnet_account(id: &str) -> bool {
    id.starts_with(ACCOUNT_PREFIX)
}

/// The secret store item for an agent key: `testnet:<master>`.
pub fn store_account(master: &str) -> String {
    format!("testnet:{}", master.trim().to_ascii_lowercase())
}

/// The ticket's Hyperliquid fields (the rest is shared with SnapTrade's ticket).
#[derive(Debug, Clone, PartialEq)]
pub struct HlTicket {
    pub kind: HlKind,
    pub tif: Tif,
    pub reduce_only: bool,
    pub trigger: String,
    /// The dollar amount the size came from, as in `BUY xyz:AAPL $50`. Editing the size clears it.
    pub from_dollars: Option<f64>,
}

impl Default for HlTicket {
    fn default() -> Self {
        Self { kind: HlKind::Limit, tif: Tif::Gtc, reduce_only: false, trigger: String::new(), from_dollars: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlKind {
    Market,
    Limit,
    StopLoss,
    TakeProfit,
}

impl HlKind {
    pub const ALL: [HlKind; 4] = [HlKind::Market, HlKind::Limit, HlKind::StopLoss, HlKind::TakeProfit];

    pub fn label(self) -> &'static str {
        match self {
            HlKind::Market => "Market",
            HlKind::Limit => "Limit",
            HlKind::StopLoss => "Stop loss",
            HlKind::TakeProfit => "Take profit",
        }
    }
}

/// An order whose send got no clear answer: it may have reached Hyperliquid. Nothing else is
/// sent until `orderStatus` finds it, or its `expiresAfter` window passes without it.
#[derive(Debug, Clone, PartialEq)]
pub struct Uncertain {
    pub cloid: String,
    pub coin: String,
    /// When it was sent (ms).
    pub sent: u64,
    asked: Option<Instant>,
}

impl Uncertain {
    /// Hyperliquid hasn't seen it and can no longer accept it.
    pub fn expired(&self, now: u64) -> bool {
        now > self.sent + trader::EXPIRES_AFTER_MS + 5_000
    }
}

/// An action waiting for its confirmation.
#[derive(Debug, Clone, PartialEq)]
pub enum HlConfirm {
    Order {
        ticket: u64,
        plan: Box<OrderPlan>,
        modify: Option<u64>,
    },
    Cancel {
        coin: String,
        oid: u64,
        line: String,
    },
    /// The kill switch: cancel these and turn Hyperliquid trading off.
    Kill {
        orders: Vec<(String, u64)>,
    },
    Leverage {
        coin: String,
        leverage: u32,
        cross: bool,
        current: Option<tr_hl::trader::Leverage>,
    },
}

pub struct HlState {
    pub store: AgentKeyStore,
    /// The store can't hold keys (no keyring): trading stays off, with this reason.
    pub store_error: Option<String>,
    /// The agent key for the connected account, once loaded.
    pub key: Option<Arc<AgentKey>>,
    pub key_loading: bool,
    /// The store item already read (found or not), so it isn't read again every frame.
    key_tried: Option<String>,
    pub nonces: Arc<NonceManager>,
    pub registry: Option<Arc<AssetRegistry>>,
    pub registry_error: Option<String>,
    registry_at: Option<Instant>,
    /// The account as the stream last reported it.
    pub user: Option<Arc<UserState>>,
    generation: u64,
    streaming: Option<String>,
    pub stream_status: String,
    pub connected: bool,
    /// Testnet mids by terminal market key (`xyz:AAPL`, `BTCUSD Curncy`). Never mixed with
    /// the Terminal server's mainnet quotes.
    pub mids: HashMap<String, f64>,
    /// The ticket's market: `(mid, account data, when)` by coin, read over REST.
    pub market: HashMap<String, (Option<f64>, Option<AssetData>, Instant)>,
    market_asked: HashMap<String, Instant>,
    pub roles: Option<Result<HlRoles, String>>,
    pub roles_busy: bool,
    /// Settings forms and dialogs.
    pub enable_prompt: bool,
    pub ack: bool,
    pub connect_form: String,
    /// A pasted testnet main key, held only until it signs (then wiped).
    pub main_key_form: Zeroizing<String>,
    pub busy: Option<&'static str>,
    pub message: Option<(String, bool)>,
    pub confirm: Option<HlConfirm>,
    /// Ticket feedback for Hyperliquid accounts: the last refusal, result and pending action.
    pub sending: Option<u64>,
    /// The order in flight: `(request id, cloid, coin, sent ms)`.
    inflight: Option<(u64, String, String, u64)>,
    pub uncertain: Option<Uncertain>,
    /// Kill switches pressed; every signed action carries the count when it was confirmed.
    kill_epoch: u64,
    /// The account an approval in flight is for. A late answer for any other is ignored.
    pending_approval: Option<String>,
    next_id: u64,
    deadman_at: Option<Instant>,
    deadman_armed: bool,
    /// The panes need the rows rebuilt.
    pub dirty: bool,
    /// The browser wallet's account (web).
    #[cfg(target_arch = "wasm32")]
    pub wallet: Option<String>,
    /// For the page's promises to wake the UI (web).
    #[cfg(target_arch = "wasm32")]
    pub ctx: Option<egui::Context>,
}

impl HlState {
    pub fn new(store: AgentKeyStore) -> Self {
        Self {
            store,
            store_error: None,
            key: None,
            key_loading: false,
            key_tried: None,
            nonces: Arc::new(NonceManager::new()),
            registry: None,
            registry_error: None,
            registry_at: None,
            user: None,
            generation: 0,
            streaming: None,
            stream_status: String::new(),
            connected: false,
            mids: HashMap::new(),
            market: HashMap::new(),
            market_asked: HashMap::new(),
            roles: None,
            roles_busy: false,
            enable_prompt: false,
            ack: false,
            connect_form: String::new(),
            main_key_form: Zeroizing::new(String::new()),
            busy: None,
            message: None,
            confirm: None,
            sending: None,
            inflight: None,
            uncertain: None,
            kill_epoch: 0,
            pending_approval: None,
            next_id: 0,
            deadman_at: None,
            deadman_armed: false,
            dirty: true,
            #[cfg(target_arch = "wasm32")]
            wallet: None,
            #[cfg(target_arch = "wasm32")]
            ctx: None,
        }
    }

    /// The store for this platform: the Keychain or Secret Service on the desktop, memory in
    /// the browser (with an opt-in encrypted copy, see `crate::hl_web`).
    pub fn platform() -> Self {
        Self::new(if cfg!(target_arch = "wasm32") { AgentKeyStore::memory() } else { AgentKeyStore::platform() })
    }

    /// The ticket's view of a market: testnet mid (the stream's, else the last REST read)
    /// and the account's leverage.
    pub fn ctx(&self, coin: &str) -> MarketCtx {
        let rest = self.market.get(coin);
        let stream_mid = self.user.as_ref().and_then(|u| u.mids.get(coin).copied());
        MarketCtx {
            mid: stream_mid.or(rest.and_then(|r| r.0)),
            leverage: rest.and_then(|r| r.1.as_ref()).and_then(|d| d.leverage),
        }
    }
}

// ---- the panes' rows ------------------------------------------------------------------------

/// `Apple Inc perp · PERP · HL xyz (perpetual, not a share)`, `BTC perp · PERP · HL`.
pub fn describe(coin: &str) -> String {
    if tr_core::perps::is_perp(coin) {
        let name = tr_core::perps::link_for(coin).map_or_else(|| format!("{coin} perp"), |l| l.name.to_string());
        format!("{name} · {} (perpetual, not a share)", tr_core::perps::badge(coin))
    } else {
        format!("{coin} perp · PERP · HL (perpetual)")
    }
}

fn time(ms: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(ms as i64)
}

/// The account, positions, open orders and activity (fills and funding) for the panes.
pub fn rows(master: &str, user: &UserState) -> Portfolio {
    let id = account_id(master);
    let account = Account {
        id: id.clone(),
        name: "Hyperliquid testnet".into(),
        number: master.to_ascii_lowercase(),
        institution: "Hyperliquid".into(),
        currency: "USD".into(),
        total: Some(user.account_value()),
        // Collateral: equity less unrealised P&L, so repricing the perps (valued by their P&L in
        // `Portfolio::totals`) moves equity only by how much their P&L moves.
        cash: Some(user.account_value() - user.positions().map(|p| p.unrealized_pnl).sum::<f64>()),
        buying_power: Some(user.withdrawable()),
        kind: "Perps · TESTNET".into(),
        status: "testnet".into(),
        is_paper: true,
        synced: (!user.clearinghouse.is_empty()).then(chrono::Utc::now),
        connection_id: CONNECTION_ID.into(),
    };
    let positions = user
        .positions()
        .filter(|p| p.szi != 0.0)
        .map(|p| Position {
            account_id: id.clone(),
            symbol: p.coin.clone(),
            description: describe(&p.coin),
            kind: InstrumentKind::Future,
            units: p.szi,
            price: p.mark(),
            cost_basis: p.entry_px,
            multiplier: 1.0,
            currency: "USD".into(),
            cash_equivalent: false,
            underlying: None,
            market: Some(app_key(&p.coin)),
        })
        .collect();
    let mut orders: Vec<Order> = user
        .orders
        .values()
        .flatten()
        .map(|o| Order {
            account_id: id.clone(),
            id: o.oid.to_string(),
            symbol: o.coin.clone(),
            description: describe(&o.coin),
            kind: InstrumentKind::Future,
            action: if o.is_buy { "BUY" } else { "SELL" }.into(),
            status: "ACCEPTED".into(),
            order_type: o.order_type.clone(),
            time_in_force: o.tif.clone().unwrap_or_default().to_ascii_uppercase(),
            quantity: Some(if o.orig_sz > 0.0 { o.orig_sz } else { o.sz }),
            filled: Some((o.orig_sz - o.sz).max(0.0)),
            limit_price: Some(o.limit_px),
            stop_price: o.trigger_px,
            execution_price: None,
            placed: time(o.timestamp),
            updated: None,
            underlying: None,
            market: Some(app_key(&o.coin)),
        })
        .collect();
    orders.sort_by_key(|o| std::cmp::Reverse(o.placed));
    let mut activities: Vec<Activity> = user
        .fills
        .iter()
        .map(|f| Activity {
            account_id: id.clone(),
            id: f.tid.to_string(),
            kind: if f.is_buy { "BUY" } else { "SELL" }.into(),
            symbol: Some(f.coin.clone()),
            description: if f.closed_pnl != 0.0 {
                format!("{} · realised P&L {:+.2}", f.dir, f.closed_pnl)
            } else {
                f.dir.clone()
            },
            units: Some(f.sz),
            price: Some(f.px),
            // A perp fill moves cash by its realised P&L less the fee, not its notional.
            amount: Some(f.closed_pnl - f.fee),
            fee: Some(f.fee),
            currency: "USD".into(),
            trade_date: time(f.time),
            settlement_date: None,
            market: Some(app_key(&f.coin)),
        })
        .chain(user.fundings.iter().map(|f| Activity {
            account_id: id.clone(),
            id: format!("funding-{}-{}", f.coin, f.time),
            kind: "FUNDING".into(),
            symbol: Some(f.coin.clone()),
            description: format!("Funding at {:+.4}%", f.rate * 100.0),
            units: Some(f.szi),
            price: None,
            amount: Some(f.usdc),
            fee: None,
            currency: "USD".into(),
            trade_date: time(f.time),
            settlement_date: None,
            market: Some(app_key(&f.coin)),
        }))
        .collect();
    activities.sort_by_key(|a| std::cmp::Reverse(a.trade_date));
    Portfolio {
        accounts: vec![account],
        positions,
        orders,
        activities,
        fetched: Some(chrono::Utc::now()),
        ..Default::default()
    }
}

/// Replace the Hyperliquid rows in `pf` with `hl` (or remove them).
pub fn splice(pf: &mut Portfolio, hl: Option<Portfolio>) {
    pf.accounts.retain(|a| !is_hl_account(&a.id));
    pf.positions.retain(|p| !is_hl_account(&p.account_id));
    pf.orders.retain(|o| !is_hl_account(&o.account_id));
    pf.activities.retain(|a| !is_hl_account(&a.account_id));
    if let Some(h) = hl {
        pf.accounts.extend(h.accounts);
        pf.positions.extend(h.positions);
        pf.orders.extend(h.orders);
        pf.activities.extend(h.activities);
    }
}

/// Testnet mids by terminal market key, for the perps the registry knows.
pub fn testnet_quotes(user: &UserState, reg: &AssetRegistry) -> HashMap<String, f64> {
    reg.assets().filter_map(|a| Some((a.app_key(), *user.mids.get(&a.coin)?))).collect()
}

/// The Hyperliquid coin for what the ticket says: `BTC`, `BTCUSD`, `xyz:aapl`. A stock with a
/// linked perp is never traded as that perp silently.
pub fn resolve_coin(symbol: &str, reg: &AssetRegistry) -> Result<String, String> {
    let s = symbol.trim();
    if s.is_empty() {
        return Err("Enter a market: BTC, ETH or xyz:AAPL.".into());
    }
    if s.contains(':') || s.contains('/') || s.starts_with('@') {
        return reg.resolve(s).map(|a| a.coin.clone());
    }
    if let Some(a) = reg.get(s).filter(|a| !a.delisted) {
        return Ok(a.coin.clone());
    }
    let up = s.to_ascii_uppercase();
    if let Some(base) = up.strip_suffix("USD").filter(|b| !b.is_empty())
        && let Some(a) = reg.get(base).filter(|a| !a.delisted)
    {
        return Ok(a.coin.clone());
    }
    if let Some(link) = tr_core::perps::perp_for(&format!("{up} US Equity")) {
        return Err(format!(
            "{up} is a stock. Hyperliquid has the perp {} ({}). Trade that instead?",
            link.coin,
            tr_core::perps::badge(link.coin)
        ));
    }
    reg.resolve(s).map(|a| a.coin.clone())
}

fn parse_num(s: &str) -> Option<f64> {
    s.trim().trim_start_matches('$').replace(',', "").parse::<f64>().ok()
}

/// A dollar amount on a Hyperliquid ticket, as in `BUY xyz:AAPL $50`, becomes a size at the
/// testnet mid, rounded down to the market's size step so the order is never worth more than
/// the amount. Until there is a mid, or when the amount is under one step, it stays a dollar
/// amount, which [`intent`] refuses. Returns why it isn't a size yet.
pub fn settle_size(t: &mut super::trading::Ticket, mid: Option<f64>, asset: Option<&Asset>) -> Option<String> {
    if !t.dollars {
        return None;
    }
    let amount = parse_num(&t.qty).filter(|a| a.is_finite() && *a > 0.0)?;
    let dollars = tr_hl::rounding::to_wire(amount).unwrap_or_else(|_| format!("{amount:.2}"));
    let (Some(a), Some(mid)) = (asset, mid.filter(|m| m.is_finite() && *m > 0.0)) else {
        return Some(format!("Waiting for the testnet mid to size ${dollars}…"));
    };
    let units = tr_hl::rounding::round_size(amount / mid, a.sz_decimals);
    let Some(size) = tr_hl::rounding::to_wire(units).ok().filter(|_| units > 0.0) else {
        let step = 10f64.powi(-(a.sz_decimals as i32));
        return Some(format!(
            "${dollars} is less than the smallest {} size, {}, which is about ${:.2} at the testnet mid.",
            a.coin,
            tr_hl::rounding::to_wire(step).unwrap_or_default(),
            step * mid
        ));
    };
    t.qty = size;
    t.dollars = false;
    t.hl.from_dollars = Some(amount);
    None
}

/// The order a Hyperliquid ticket describes.
pub fn intent(t: &super::trading::Ticket, reg: &AssetRegistry) -> Result<OrderIntent, String> {
    let coin = resolve_coin(&t.symbol, reg)?;
    // Never read a dollar figure as a number of coins or contracts.
    if t.dollars {
        return Err(match parse_num(&t.qty) {
            Some(a) => format!(
                "${} is a dollar amount, not a size. The ticket sizes it at the testnet mid first.",
                tr_hl::rounding::to_wire(a).unwrap_or_else(|_| t.qty.trim().to_string())
            ),
            None => "Enter an amount in dollars.".into(),
        });
    }
    let size =
        parse_num(&t.qty).filter(|s| s.is_finite() && *s > 0.0).ok_or("Enter a size (in coins or contracts).")?;
    let price = |s: &str, what: &str| parse_num(s).filter(|p| *p > 0.0).ok_or(format!("Enter a {what} price."));
    let kind = match t.hl.kind {
        HlKind::Market => OrderKind::Market,
        HlKind::Limit => OrderKind::Limit { px: price(&t.limit, "limit")?, tif: t.hl.tif },
        HlKind::StopLoss => {
            OrderKind::Trigger { trigger_px: price(&t.hl.trigger, "trigger")?, tpsl: tr_hl::actions::Tpsl::Sl }
        }
        HlKind::TakeProfit => {
            OrderKind::Trigger { trigger_px: price(&t.hl.trigger, "trigger")?, tpsl: tr_hl::actions::Tpsl::Tp }
        }
    };
    // Stops and take-profits close a position; they never open one.
    let reduce_only = t.hl.reduce_only || matches!(t.hl.kind, HlKind::StopLoss | HlKind::TakeProfit);
    Ok(OrderIntent { coin, is_buy: t.side == tr_broker::trading::Side::Buy, size, kind, reduce_only })
}

/// Why this device can't sign for the account (`None`: it can).
fn signing_blocker(h: &crate::prefs::HlPrefs, hl: &HlState, now: u64) -> Option<String> {
    if h.master.is_none() {
        return Some("Connect a Hyperliquid testnet account in Settings › Trading.".into());
    }
    if let Some(e) = &hl.store_error {
        return Some(format!("Hyperliquid trading is off on this device: {e}"));
    }
    let Some(agent) = &h.agent else {
        return Some("Read-only: approve an agent in Settings › Trading › Hyperliquid to trade.".into());
    };
    if agent.valid_until <= now {
        return Some("The agent expired: approve a new one in Settings › Trading › Hyperliquid.".into());
    }
    if hl.key.is_none() {
        return Some(if hl.key_loading {
            "Loading the agent key…".into()
        } else {
            "This device has no key for the approved agent: approve a new one in Settings › Trading.".into()
        });
    }
    if !h.enabled {
        return Some("Hyperliquid orders are off. Enable them in Settings › Trading › Hyperliquid.".into());
    }
    None
}

/// Why a new order (or other confirmed action) can't be sent: [`signing_blocker`], or an
/// earlier order whose fate is still unknown.
fn placement_blocker(h: &crate::prefs::HlPrefs, hl: &HlState, now: u64) -> Option<String> {
    signing_blocker(h, hl, now).or_else(|| {
        hl.uncertain.as_ref().map(|u| {
            format!(
                "The last {} order got no clear answer; checking whether it reached Hyperliquid (client id {}) before sending anything else.",
                u.coin, u.cloid
            )
        })
    })
}

/// What stops a confirmed action: new orders wait for an uncertain one to be resolved;
/// cancels and leverage only need signing; the kill switch always goes ahead.
fn confirm_blocker(c: &HlConfirm, h: &crate::prefs::HlPrefs, hl: &HlState, now: u64) -> Option<String> {
    match c {
        HlConfirm::Order { .. } => placement_blocker(h, hl, now),
        HlConfirm::Cancel { .. } | HlConfirm::Leverage { .. } => signing_blocker(h, hl, now),
        HlConfirm::Kill { .. } => None,
    }
}

/// Keep the dead man's switch armed? Only whether this device can sign matters: an order
/// whose fate is unknown is exactly when the timed cancel must stay in place.
fn deadman_wanted(h: &crate::prefs::HlPrefs, hl: &HlState, now: u64) -> bool {
    h.dead_mans_switch && signing_blocker(h, hl, now).is_none()
}

/// Read the agent key from the store? Once per account: a store that has none for an
/// approved agent is not asked again every frame.
fn should_load_key(agent: bool, h: &HlState, account: &str) -> bool {
    agent && h.key.is_none() && !h.key_loading && h.key_tried.as_deref() != Some(account)
}

/// `terminal-<this machine>`: the host name, or `web` in the browser.
fn device() -> String {
    if cfg!(target_arch = "wasm32") {
        return "web".into();
    }
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .arg("-s")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "desktop".into())
}

impl TerminalApp {
    fn hl_send(&self, r: HlRequest) {
        self.worker.send(Request::Hl(r));
    }

    /// The connected account (master address), if any.
    pub(super) fn hl_master(&self) -> Option<String> {
        self.prefs.trading.hl.master.clone()
    }

    /// Why Hyperliquid orders can't be sent right now (`None`: they can).
    pub(super) fn hl_blocker(&self) -> Option<String> {
        placement_blocker(&self.prefs.trading.hl, &self.hl, now_ms())
    }

    /// Why nothing can be signed at all (cancels and leverage only need this).
    pub(super) fn hl_signing_blocker(&self) -> Option<String> {
        signing_blocker(&self.prefs.trading.hl, &self.hl, now_ms())
    }

    /// Start everything for the saved account (at launch, or after connecting).
    pub(super) fn hl_start(&mut self) {
        let Some(master) = self.hl_master() else { return };
        self.hl_registry(false);
        if self.hl.streaming.as_deref() != Some(&master) {
            self.hl.generation += 1;
            self.hl.streaming = Some(master.clone());
            self.hl.user = None;
            self.hl.dirty = true;
            self.hl_send(HlRequest::Stream { generation: self.hl.generation, user: master.clone() });
        }
        let account = store_account(&master);
        if should_load_key(self.prefs.trading.hl.agent.is_some(), &self.hl, &account) {
            self.hl.key_loading = true;
            self.hl.key_tried = Some(account.clone());
            self.hl_send(HlRequest::LoadKey { store: self.hl.store.clone(), account });
        }
    }

    fn hl_registry(&mut self, force: bool) {
        let stale = self.hl.registry_at.is_none_or(|t| t.elapsed() > Duration::from_secs(3600));
        if force || (stale && self.hl.registry_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30))) {
            self.hl.registry_at = Some(Instant::now());
            self.hl_send(HlRequest::Registry);
        }
    }

    /// Show a testnet account read-only (no key involved).
    pub(super) fn hl_connect(&mut self, address: &str) {
        let addr = match tr_hl::key::parse_address(address) {
            Ok(a) => tr_hl::key::address_hex(&a),
            Err(e) => return self.hl.message = Some((format!("{e}."), true)),
        };
        if self.hl_master().as_deref() != Some(&addr) {
            // Another account: its agent (if any) is not this one's.
            self.prefs.trading.hl.agent = None;
            self.prefs.trading.hl.enabled = false;
            self.hl.key = None;
        }
        self.prefs.trading.hl.master = Some(addr.clone());
        self.save_hl_prefs();
        self.hl.key_tried = None;
        self.hl.pending_approval = None;
        self.hl.message = Some((
            format!(
                "Showing Hyperliquid testnet account {} (read-only).",
                tr_hl::mask_address(&tr_hl::key::parse_address(&addr).expect("parsed"))
            ),
            false,
        ));
        self.hl_start();
    }

    /// Stop showing the account. The agent key stays in the store unless revoked.
    pub(super) fn hl_disconnect(&mut self) {
        self.hl.pending_approval = None;
        self.hl.uncertain = None;
        self.prefs.trading.hl.master = None;
        self.prefs.trading.hl.enabled = false;
        self.save_hl_prefs();
        self.hl.key = None;
        self.hl.user = None;
        self.hl.streaming = None;
        self.hl.mids.clear();
        self.hl.dirty = true;
        self.hl_send(HlRequest::StopStream);
        self.hl.message = Some(("Hyperliquid account removed from Terminal.".into(), false));
    }

    fn save_hl_prefs(&mut self) {
        if let Err(e) = self.prefs.save(&super::prefs_path()) {
            self.status = format!("Could not save preferences: {e}");
        }
    }

    /// Desktop, testnet only: sign `approveAgent` for a fresh agent with a pasted **testnet
    /// throwaway** main key, which is wiped as soon as it has signed and is never stored.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn hl_approve_with_pasted_key(&mut self) {
        let pasted = std::mem::replace(&mut self.hl.main_key_form, Zeroizing::new(String::new()));
        let main = match MainKey::from_hex(&pasted) {
            Ok(k) => k,
            Err(e) => return self.hl.message = Some((format!("{e}."), true)),
        };
        drop(pasted);
        let agent = AgentKey::generate();
        let action = trader::approve_agent(&agent.address(), &device(), trader::DEFAULT_AGENT_DAYS, now_ms());
        let signature = match tr_hl::signing::sign_approve_agent(&main, &action) {
            Ok(s) => s,
            Err(e) => return self.hl.message = Some((format!("Could not sign: {e:#}"), true)),
        };
        let master = tr_hl::key::address_hex(&main.address());
        drop(main);
        let body = trader::user_signed_body(&action, &signature);
        self.hl_submit_approval(master, agent, &action, body);
    }

    /// Post a signed `approveAgent` (from the pasted key or the browser wallet).
    pub(super) fn hl_submit_approval(
        &mut self,
        master: String,
        agent: AgentKey,
        action: &tr_hl::actions::ApproveAgentAction,
        body: serde_json::Value,
    ) {
        if self.hl_master().as_deref() != Some(&master) {
            self.prefs.trading.hl.master = Some(master.clone());
            self.prefs.trading.hl.enabled = false;
            self.save_hl_prefs();
        }
        let meta = HlAgent {
            address: action.agent_address.clone(),
            name: action.agent_name.clone(),
            valid_until: trader::agent_valid_until(&action.agent_name).unwrap_or(0),
            network: tr_hl::Network::Testnet,
        };
        self.hl.busy = Some("Approving the agent on Hyperliquid testnet…");
        self.hl.message = None;
        self.hl.pending_approval = Some(master.clone());
        self.hl_send(HlRequest::Approve {
            store: self.hl.store.clone(),
            master,
            key: Arc::new(agent),
            body,
            agent: meta,
        });
        self.hl_start();
    }

    /// Revoke: delete the key here, and (with a pasted main key) replace the agent on
    /// Hyperliquid by approving a throwaway address under the same name.
    pub(super) fn hl_revoke(&mut self, on_chain: bool) {
        let Some(master) = self.hl_master() else { return };
        let name = self.prefs.trading.hl.agent.as_ref().map(|a| trader::agent_base_name(&a.name).to_string());
        let body = if on_chain {
            let pasted = std::mem::replace(&mut self.hl.main_key_form, Zeroizing::new(String::new()));
            let main = match MainKey::from_hex(&pasted) {
                Ok(k) => k,
                Err(e) => return self.hl.message = Some((format!("{e}."), true)),
            };
            if tr_hl::key::address_hex(&main.address()) != master {
                return self.hl.message = Some(("That key is not this account's main wallet.".into(), true));
            }
            let base = name.clone().unwrap_or_else(|| format!("terminal-{}", device()));
            let action = trader::revoke_agent(&base, now_ms());
            match tr_hl::signing::sign_approve_agent(&main, &action) {
                Ok(sig) => Some(trader::user_signed_body(&action, &sig)),
                Err(e) => return self.hl.message = Some((format!("Could not sign: {e:#}"), true)),
            }
        } else {
            None
        };
        self.hl_revoke_send(body);
    }

    /// Forget the agent here and post the signed revoke, if there is one.
    pub(super) fn hl_revoke_send(&mut self, body: Option<serde_json::Value>) {
        let Some(master) = self.hl_master() else { return };
        self.hl.key = None;
        self.hl.key_tried = None;
        self.prefs.trading.hl.agent = None;
        self.prefs.trading.hl.enabled = false;
        self.save_hl_prefs();
        self.hl.busy = Some("Revoking…");
        self.hl_send(HlRequest::Revoke { store: self.hl.store.clone(), account: store_account(&master), body });
    }

    /// `userRole` and `extraAgents`, for Settings.
    pub(super) fn hl_check_roles(&mut self) {
        let Some(master) = self.hl_master() else { return };
        self.hl.roles_busy = true;
        let agent = self.prefs.trading.hl.agent.as_ref().map(|a| a.address.clone());
        self.hl_send(HlRequest::Roles { master, agent });
    }

    /// Keep the ticket's market fresh, the rows current, and the dead man's switch armed.
    pub(super) fn hl_periodic(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if self.hl_master().is_none() {
            self.hl_web_poll();
        }
        if self.hl_master().is_none() {
            if self.hl.dirty {
                self.hl.dirty = false;
                if let Some(pf) = self.broker.portfolio.as_deref_mut() {
                    splice(pf, None);
                }
            }
            return;
        }
        self.hl_start();
        #[cfg(target_arch = "wasm32")]
        self.hl_web_poll();
        self.hl_sync_portfolio();
        self.hl_ticket_market();
        self.hl_deadman();
        self.hl_resolve_uncertain();
    }

    /// Ask Hyperliquid about an uncertain order every few seconds until it is found, or its
    /// `expiresAfter` window has passed without it.
    fn hl_resolve_uncertain(&mut self) {
        let Some(master) = self.hl_master() else { return };
        let Some(u) = self.hl.uncertain.as_mut() else { return };
        if u.asked.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        u.asked = Some(Instant::now());
        let cloid = u.cloid.clone();
        self.hl_send(HlRequest::OrderStatus { user: master, cloid });
    }

    /// Put the Hyperliquid rows into the live portfolio (after a stream update, or after a
    /// SnapTrade reload replaced the portfolio).
    fn hl_sync_portfolio(&mut self) {
        let Some(master) = self.hl_master() else { return };
        let id = account_id(&master);
        let missing = self.broker.portfolio.as_ref().is_none_or(|p| p.account(&id).is_none());
        if !self.hl.dirty && !missing {
            return;
        }
        self.hl.dirty = false;
        let user = self.hl.user.clone().unwrap_or_default();
        let rows = rows(&master, &user);
        match self.broker.portfolio.as_deref_mut() {
            Some(pf) => splice(pf, Some(rows)),
            None => self.broker.portfolio = Some(Box::new(rows)),
        }
    }

    fn hl_ticket_market(&mut self) {
        let Some(master) = self.hl_master() else { return };
        let Some(reg) = self.hl.registry.clone() else { return };
        let coin = self
            .trading
            .ticket
            .as_ref()
            .filter(|t| t.account.as_deref().is_some_and(is_hl_account))
            .and_then(|t| resolve_coin(&t.symbol, &reg).ok());
        let Some(coin) = coin else { return };
        if self.hl.market_asked.get(&coin).is_some_and(|t| t.elapsed() < MARKET_EVERY) {
            return;
        }
        self.hl.market_asked.insert(coin.clone(), Instant::now());
        let dex = reg.get(&coin).map(|a| a.dex.clone()).unwrap_or_default();
        self.hl_send(HlRequest::Market { user: master, coin, dex });
    }

    fn hl_deadman(&mut self) {
        let want = deadman_wanted(&self.prefs.trading.hl, &self.hl, now_ms());
        if want && self.hl.deadman_at.is_none_or(|t| t.elapsed() > Duration::from_millis(DEADMAN_AHEAD_MS / 2)) {
            self.hl.deadman_at = Some(Instant::now());
            self.hl.deadman_armed = true;
            self.hl_act(HlAct::ScheduleCancel(Some(now_ms() + DEADMAN_AHEAD_MS)));
        } else if !want && std::mem::take(&mut self.hl.deadman_armed) {
            self.hl.deadman_at = None;
            if self.hl.key.is_some() {
                self.hl_act(HlAct::ScheduleCancel(None));
            }
        }
    }

    /// Send a confirmed action to the worker, which signs it after one more check.
    fn hl_act(&mut self, act: HlAct) {
        let (Some(key), Some(master)) = (self.hl.key.clone(), self.hl_master()) else {
            return self.trade_toast("Hyperliquid: no agent key on this device", false);
        };
        // An agent key that isn't this account's approved agent never signs.
        if self.prefs.trading.hl.agent.as_ref().map(|a| a.address.as_str())
            != Some(tr_hl::key::address_hex(&key.address()).as_str())
        {
            return self.trade_toast("Hyperliquid: the agent key here isn't this account's approved agent", false);
        }
        self.hl.next_id += 1;
        let id = self.hl.next_id;
        if !matches!(act, HlAct::ScheduleCancel(_)) {
            self.hl.sending = Some(id);
            self.status = format!("Hyperliquid TESTNET: signing {}…", act.label());
        }
        if let HlAct::Order(p) | HlAct::Modify { plan: p, .. } = &act {
            self.hl.inflight = Some((id, p.cloid().to_string(), p.asset().coin.clone(), now_ms()));
        }
        let kill_epoch = self.hl.kill_epoch;
        self.hl_send(HlRequest::Act { id, key, nonces: self.hl.nonces.clone(), user: master, act, kill_epoch });
    }

    // ---- confirmations ------------------------------------------------------------------

    /// The ticket's Review button: plan (round and check the caps), then confirm.
    pub(super) fn hl_review(&mut self) {
        let Some(t) = self.trading.ticket.as_ref() else { return };
        let id = t.id;
        let planned = match (self.hl_blocker(), self.hl.registry.clone()) {
            (Some(b), _) => Err(b),
            (None, None) => Err("Loading Hyperliquid testnet markets…".into()),
            (None, Some(reg)) => intent(t, &reg).and_then(|i| {
                let ctx = self.hl.ctx(&i.coin);
                let cloid = tr_hl::actions::cloid(u128::from(now_ms()) << 32 | u128::from(id as u32));
                trader::plan_order(&reg, &i, &ctx, &cloid).map_err(|r| r.to_string())
            }),
        };
        match planned {
            Ok(plan) => {
                self.hl.confirm = Some(HlConfirm::Order { ticket: id, plan: Box::new(plan), modify: None });
                if let Some(t) = self.trading.ticket.as_mut() {
                    t.error = None;
                }
            }
            Err(e) => {
                if let Some(t) = self.trading.ticket.as_mut() {
                    t.error = Some(e);
                    t.done = None;
                }
            }
        }
    }

    /// A Cancel on an open Hyperliquid order (from `ORD`).
    pub(super) fn hl_request_cancel(&mut self, order_id: &str) {
        let order = self
            .broker
            .portfolio
            .as_ref()
            .and_then(|pf| pf.orders.iter().find(|o| is_hl_account(&o.account_id) && o.id == order_id))
            .cloned();
        let Some(o) = order else { return self.status = "Order not found".into() };
        if let Some(b) = self.hl_signing_blocker() {
            return self.status = b;
        }
        let Ok(oid) = o.id.parse::<u64>() else { return };
        let line = format!(
            "{} {} {} limit {}",
            o.action,
            o.quantity.map_or("—".into(), super::portfolio_view::qty),
            o.symbol,
            o.limit_price.map_or("—".into(), super::portfolio_view::price)
        );
        self.hl.confirm = Some(HlConfirm::Cancel { coin: o.symbol.clone(), oid, line });
    }

    /// Close a position: a reduce-only market order for all of it, through the ticket (so it
    /// is still reviewed and confirmed).
    pub(super) fn hl_close_position(&mut self, account: &str, key: &str) {
        let held = self
            .broker
            .portfolio
            .as_ref()
            .and_then(|pf| {
                pf.positions.iter().find(|p| p.account_id == account && p.market_key().as_deref() == Some(key))
            })
            .map(|p| (p.symbol.clone(), p.units));
        let Some((coin, units)) = held.filter(|(_, u)| *u != 0.0) else {
            return self.status = format!("No {key} position to close");
        };
        let args = crate::command::TradeArgs { buy: Some(units < 0.0), units: Some(units.abs()), ..Default::default() };
        self.open_ticket(args, Some(account.into()));
        if let Some(t) = self.trading.ticket.as_mut() {
            t.symbol = coin;
            t.hl.kind = HlKind::Market;
            t.hl.reduce_only = true;
        }
    }

    /// `KILL` and the kill button: confirm cancelling every open order.
    pub(super) fn hl_kill(&mut self) {
        if self.hl_master().is_none() {
            return self.status = "KILL: no Hyperliquid account is connected (Settings › Trading).".into();
        }
        let orders = self.hl.user.as_ref().map(|u| u.open_order_ids()).unwrap_or_default();
        self.hl.confirm = Some(HlConfirm::Kill { orders });
    }

    /// Ask to set leverage on the ticket's market (capped at 3×).
    pub(super) fn hl_request_leverage(&mut self, coin: &str, leverage: u32) {
        let reg = self.hl.registry.clone();
        let Some(asset) = reg.as_ref().and_then(|r| r.get(coin)) else { return };
        let current = self.hl.ctx(coin).leverage;
        let cross = !asset.only_isolated && current.is_none_or(|l| l.cross);
        self.hl.confirm = Some(HlConfirm::Leverage { coin: coin.into(), leverage, cross, current });
    }

    /// The user confirmed.
    pub(super) fn hl_confirmed(&mut self) {
        let Some(c) = self.hl.confirm.take() else { return };
        if let Some(b) = confirm_blocker(&c, &self.prefs.trading.hl, &self.hl, now_ms()) {
            return self.trade_toast(format!("Hyperliquid: {b}"), false);
        }
        let reg = self.hl.registry.clone();
        match c {
            HlConfirm::Order { plan, modify: None, .. } => self.hl_act(HlAct::Order(plan)),
            HlConfirm::Order { plan, modify: Some(oid), .. } => self.hl_act(HlAct::Modify { oid, plan }),
            HlConfirm::Cancel { coin, oid, .. } => match reg.as_ref().and_then(|r| r.get(&coin)) {
                Some(asset) => self.hl_act(HlAct::Cancel { asset: asset.clone(), oid }),
                None => self.trade_toast(format!("Hyperliquid: {coin} is not a known market"), false),
            },
            HlConfirm::Kill { orders } => {
                // Trading goes off first, whatever happens to the cancels.
                self.prefs.trading.hl.enabled = false;
                self.save_hl_prefs();
                // The worker reads the open orders again before cancelling, so a stream that
                // hasn't caught up never makes KILL skip an order.
                match (reg, self.hl.key.is_some()) {
                    (Some(reg), true) => {
                        // Orders confirmed before this, not yet signed, are refused in the worker.
                        self.hl.kill_epoch += 1;
                        self.hl_act(HlAct::CancelAll { registry: reg });
                    }
                    _ if orders.is_empty() => self
                        .trade_toast("KILL: Hyperliquid trading is off (no agent key here, no open orders seen)", true),
                    _ => self.trade_toast(
                        "KILL: Hyperliquid trading is off, but there's no agent key here to cancel with",
                        false,
                    ),
                }
            }
            HlConfirm::Leverage { coin, leverage, cross, .. } => match reg.as_ref().and_then(|r| r.get(&coin)) {
                Some(asset) => self.hl_act(HlAct::Leverage { asset: asset.clone(), cross, leverage }),
                None => self.trade_toast(format!("Hyperliquid: {coin} is not a known market"), false),
            },
        }
    }

    // ---- worker answers -----------------------------------------------------------------

    pub(super) fn on_hl(&mut self, r: HlResponse) {
        match r {
            HlResponse::Registry(Ok(reg)) => {
                self.hl.registry = Some(reg);
                self.hl.registry_error = None;
                self.hl_refresh_mids();
            }
            HlResponse::Registry(Err(e)) => {
                tracing::warn!("hyperliquid markets: {e}");
                self.hl.registry_error = Some(e);
            }
            HlResponse::State { generation, state } if generation == self.hl.generation => {
                self.hl.user = Some(Arc::new(*state));
                self.hl.dirty = true;
                self.hl_refresh_mids();
            }
            HlResponse::State { .. } => {}
            HlResponse::StreamStatus { generation, connected, status } if generation == self.hl.generation => {
                self.hl.connected = connected;
                self.hl.stream_status = status;
            }
            HlResponse::StreamStatus { .. } => {}
            HlResponse::Market { coin, mid, data } => {
                let data = data.map_err(|e| tracing::debug!("hyperliquid {coin}: {e}")).ok().flatten();
                self.hl.market.insert(coin, (mid, data, Instant::now()));
            }
            HlResponse::Roles(r) => {
                self.hl.roles_busy = false;
                self.hl.roles = Some(r.map_err(|e| tr_hl::mask_addresses(&e)));
            }
            HlResponse::KeyLoaded { account, result } => {
                self.hl.key_loading = false;
                match result {
                    Ok(key) if self.hl_master().is_some_and(|m| store_account(&m) == account) => {
                        let expected = self.prefs.trading.hl.agent.as_ref().map(|a| a.address.clone());
                        self.hl.key = key.filter(|k| Some(tr_hl::key::address_hex(&k.address())) == expected);
                        self.hl.store_error = None;
                    }
                    Ok(_) => {}
                    Err(e) => self.hl.store_error = Some(e),
                }
                #[cfg(target_arch = "wasm32")]
                self.hl_web_restore_key();
            }
            HlResponse::Approved { master, agent, key, result, saved } => {
                self.hl.busy = None;
                // Only the account that asked, and only while it is still the one shown.
                let current = self.hl.pending_approval.take();
                if current.as_deref() != Some(master.as_str()) || self.hl_master().as_deref() != Some(master.as_str()) {
                    self.hl.message = Some((
                        format!(
                            "An approval for {} finished after the account changed; it was not applied here.",
                            tr_hl::mask_addresses(&master)
                        ),
                        true,
                    ));
                    return;
                }
                match result {
                    Ok(summary) => {
                        self.prefs.trading.hl.agent = Some(agent.clone());
                        self.save_hl_prefs();
                        self.hl.key = Some(key);
                        self.hl.key_tried = None;
                        let note = match saved {
                            Ok(()) => format!("kept in {}", self.hl.store.describe()),
                            Err(e) => format!("but it could not be saved ({e}), so it lasts only this session"),
                        };
                        self.hl.message =
                            Some((format!("Agent {} approved ({summary}); its key is {note}.", agent.name), false));
                        #[cfg(target_arch = "wasm32")]
                        self.hl_web_remember_key();
                    }
                    Err(e) => {
                        let e = tr_hl::mask_addresses(&e);
                        self.hl.message = Some((format!("Hyperliquid testnet refused the approval: {e}"), true));
                    }
                }
                self.hl_check_roles();
            }
            HlResponse::Revoked(result) => {
                self.hl.busy = None;
                self.hl.message = Some(match result {
                    Ok(m) => (tr_hl::mask_addresses(&m), false),
                    Err(e) => (tr_hl::mask_addresses(&e), true),
                });
                #[cfg(target_arch = "wasm32")]
                self.hl_web_forget_key();
                self.hl_check_roles();
            }
            HlResponse::Done { id, label, outcome } => self.on_hl_done(id, label, outcome),
            HlResponse::OrderStatus { cloid, result } => {
                let Some(u) = self.hl.uncertain.clone().filter(|u| u.cloid == cloid) else { return };
                match result {
                    Ok(Some(status)) => {
                        self.hl.uncertain = None;
                        self.trade_toast(
                            format!(
                                "Hyperliquid TESTNET: the {} order did reach Hyperliquid ({status}); see ORD",
                                u.coin
                            ),
                            true,
                        );
                    }
                    Ok(None) if u.expired(now_ms()) => {
                        self.hl.uncertain = None;
                        self.trade_toast(
                            format!(
                                "Hyperliquid TESTNET: the {} order never reached Hyperliquid; it can be sent again",
                                u.coin
                            ),
                            true,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("hyperliquid order status: {e}"),
                }
            }
        }
    }

    fn hl_refresh_mids(&mut self) {
        if let (Some(u), Some(reg)) = (&self.hl.user, &self.hl.registry) {
            self.hl.mids = testnet_quotes(u, reg);
        }
    }

    fn on_hl_done(&mut self, id: u64, label: String, outcome: Outcome) {
        let mine = self.hl.sending == Some(id);
        if mine {
            self.hl.sending = None;
        }
        if let Some((inflight, cloid, coin, sent)) = self.hl.inflight.take_if(|f| f.0 == id) {
            let _ = inflight;
            // No clear answer: the order may exist. Find it before anything else is sent.
            if matches!(&outcome, Outcome::Failed(tr_hl::client::SendError::Uncertain(_))) {
                self.hl.uncertain = Some(Uncertain { cloid, coin, sent, asked: None });
            }
        }
        if label.starts_with("dead man") {
            if let Outcome::Answered(r) = &outcome
                && r.first_error().is_none()
            {
                return;
            }
            return self
                .trade_toast(format!("Hyperliquid TESTNET dead man's switch: {}", outcome_text(&outcome)), false);
        }
        let ok = matches!(&outcome, Outcome::Answered(r) if r.ok && r.first_error().is_none());
        let text = format!("Hyperliquid TESTNET {label}: {}", outcome_text(&outcome));
        self.trade_toast(text.clone(), ok);
        if let Some(t) = self.trading.ticket.as_mut().filter(|t| t.account.as_deref().is_some_and(is_hl_account)) {
            if ok {
                t.done = Some(text);
                t.error = None;
            } else {
                t.error = Some(text);
                t.done = None;
            }
        }
        // Leverage changed: read it again so the ticket can go on.
        self.hl.market_asked.clear();
    }
}

/// One line for a toast.
pub fn outcome_text(o: &Outcome) -> String {
    tr_hl::mask_addresses(&outcome_raw(o))
}

fn outcome_raw(o: &Outcome) -> String {
    match o {
        Outcome::Answered(r) => match r.first_error() {
            Some(e) => format!("rejected: {e}"),
            None => r.summary(),
        },
        Outcome::Refused(why) => format!("not sent: {why}"),
        Outcome::Failed(e) if matches!(e, tr_hl::client::SendError::Uncertain(_)) => {
            format!("{e}. It may have reached Hyperliquid; check ORD before sending again.")
        }
        Outcome::Failed(e) => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "0x00000000000000000000000000000000000f107e";

    fn recorded() -> UserState {
        let path = format!("{}/../crates/hl/tests/fixtures/testnet_ws_user_stream.jsonl", env!("CARGO_MANIFEST_DIR"));
        let mut s = UserState::default();
        for line in std::fs::read_to_string(path).unwrap().lines() {
            s.apply(MASTER, line);
        }
        s
    }

    fn registry() -> AssetRegistry {
        let f = |n: &str| -> serde_json::Value {
            let p = format!("{}/../crates/hl/tests/fixtures/{n}", env!("CARGO_MANIFEST_DIR"));
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        AssetRegistry::from_info(
            &f("testnet_perp_dexs.json"),
            &f("testnet_meta_core.json"),
            &[("xyz", &f("testnet_meta_xyz.json"))],
        )
        .unwrap()
    }

    #[test]
    fn a_testnet_account_becomes_portfolio_rows() {
        let pf = rows(MASTER, &recorded());
        let a = &pf.accounts[0];
        assert_eq!(a.id, account_id(MASTER));
        assert!(a.is_paper && a.kind.contains("TESTNET") && a.masked_number().ends_with("107e"));
        let upnl: f64 = recorded().positions().map(|p| p.unrealized_pnl).sum();
        assert!((a.cash.unwrap() + upnl - a.total.unwrap()).abs() < 1e-9, "cash is equity less unrealised P&L");
        // Repriced at each position's own mark, the account is worth what Hyperliquid reports
        // (within a cent: Hyperliquid rounds the entry price to five significant figures).
        let at_mark = pf.totals(None, |p| p.price);
        assert!((at_mark.value - a.total.unwrap()).abs() < 0.01, "{} vs {:?}", at_mark.value, a.total);
        assert_eq!(pf.positions.len(), 3);
        let tsla = pf.positions.iter().find(|p| p.symbol == "xyz:TSLA").unwrap();
        assert_eq!(tsla.kind, InstrumentKind::Future, "a perp, never a stock");
        assert_eq!(tsla.market_key().as_deref(), Some("xyz:TSLA"));
        assert!(tsla.description.contains("PERP · HL xyz (perpetual, not a share)"), "{}", tsla.description);
        assert!(!tsla.description.to_ascii_lowercase().contains("shares"));
        assert_eq!(tsla.units, -1.483);
        let o = &pf.orders[0];
        assert!(o.is_open() && !o.is_buy());
        assert_eq!((o.id.as_str(), o.market_key().as_deref()), ("63919996139", Some("xyz:AAPL")));
        assert_eq!(pf.activities.iter().filter(|a| a.kind == "SELL" || a.kind == "BUY").count(), 4);
    }

    #[test]
    fn splicing_replaces_only_hyperliquid_rows() {
        let mut pf = tr_broker::sample::portfolio();
        let n = (pf.accounts.len(), pf.positions.len(), pf.orders.len());
        splice(&mut pf, Some(rows(MASTER, &recorded())));
        assert_eq!(pf.accounts.len(), n.0 + 1);
        assert_eq!(pf.positions.len(), n.1 + 3);
        splice(&mut pf, Some(rows(MASTER, &recorded())));
        assert_eq!(pf.positions.len(), n.1 + 3, "a second update replaces, never duplicates");
        splice(&mut pf, None);
        assert_eq!((pf.accounts.len(), pf.positions.len(), pf.orders.len()), n);
    }

    #[test]
    fn tickets_resolve_hyperliquid_coins_and_refuse_stocks() {
        let reg = registry();
        assert_eq!(resolve_coin("btc", &reg).as_deref(), Ok("BTC"));
        assert_eq!(resolve_coin("BTCUSD", &reg).as_deref(), Ok("BTC"), "BUY BTCUSD CRNCY");
        assert_eq!(resolve_coin("XYZ:aapl", &reg).as_deref(), Ok("xyz:AAPL"));
        let e = resolve_coin("AAPL", &reg).unwrap_err();
        assert_eq!(e, "AAPL is a stock. Hyperliquid has the perp xyz:AAPL (PERP · HL xyz). Trade that instead?");
        assert!(resolve_coin("para:TOTAL2", &reg).unwrap_err().contains("xyz dex only"));
        assert!(resolve_coin("HYPE/USDC", &reg).unwrap_err().contains("spot"));
        let mut t = super::super::trading::Ticket::new(1);
        t.symbol = "xyz:AAPL".into();
        t.qty = "0.05".into();
        t.hl.kind = HlKind::StopLoss;
        t.hl.trigger = "300".into();
        let i = intent(&t, &reg).unwrap();
        assert!(i.reduce_only, "stops only close");
        assert!(matches!(i.kind, OrderKind::Trigger { trigger_px: 300.0, .. }));
        t.hl.kind = HlKind::Limit;
        assert!(intent(&t, &reg).unwrap_err().contains("limit price"));
    }

    /// Greptile #37 (f636b7c): `BUY xyz:INTC $10` put 10 in the size, and the Hyperliquid
    /// ticket read it as 10 contracts. At a $2 mid that is $20, which the $100 cap lets through.
    #[test]
    fn a_dollar_amount_becomes_units_at_the_testnet_mid() {
        use super::super::trading::Ticket;
        let reg = registry();
        let intc = reg.get("xyz:INTC").unwrap().clone();
        let dollars = |coin: &str, amount: f64| {
            let mut t = Ticket::new(1);
            t.apply(&crate::command::TradeArgs {
                buy: Some(true),
                symbol: Some(coin.into()),
                notional: Some(amount),
                ..Default::default()
            });
            t
        };
        // With no testnet mid yet, the amount stays in dollars and intent() refuses it.
        let mut t = dollars("xyz:INTC", 10.0);
        let note = settle_size(&mut t, None, Some(&intc));
        let e = intent(&t, &reg).expect_err("$10 is never read as 10 contracts");
        assert!(e.contains("$10") && e.contains("dollar"), "{e}");
        assert!(t.dollars);
        assert_eq!(note.as_deref(), Some("Waiting for the testnet mid to size $10…"));
        // At a $2 mid, $10 is 5 contracts.
        assert_eq!(settle_size(&mut t, Some(2.0), Some(&intc)), None);
        assert!(!t.dollars);
        assert_eq!((t.qty.as_str(), t.hl.from_dollars), ("5", Some(10.0)));
        assert_eq!(intent(&t, &reg).unwrap().size, 5.0);
        // Rounded down to the size step (2 decimals), so it is never worth more than $10.
        let mut t = dollars("xyz:INTC", 10.0);
        settle_size(&mut t, Some(3.0), Some(&intc));
        assert_eq!(intent(&t, &reg).unwrap().size, 3.33);
        // BTC has five size decimals, so $10 at $76,000 is 0.00013 BTC.
        let btc = reg.get("BTC").unwrap().clone();
        let mut t = dollars("BTC", 10.0);
        settle_size(&mut t, Some(76_000.0), Some(&btc));
        assert_eq!(intent(&t, &reg).unwrap().size, 0.00013);
        // An amount under one step stays in dollars. It never rounds to zero or turns into units.
        let mut t = dollars("BTC", 0.5);
        let why = settle_size(&mut t, Some(76_000.0), Some(&btc)).expect("too small");
        assert!(why.contains("smallest BTC size") && t.dollars, "{why}");
        assert!(intent(&t, &reg).is_err());
        // A size typed in units is left alone.
        let mut t = Ticket::new(2);
        t.symbol = "BTC".into();
        t.qty = "0.001".into();
        assert_eq!(settle_size(&mut t, Some(76_000.0), Some(&btc)), None);
        assert_eq!((t.qty.as_str(), t.hl.from_dollars), ("0.001", None));
    }

    #[test]
    fn an_uncertain_order_blocks_until_its_window_passes() {
        let u = Uncertain { cloid: tr_hl::actions::cloid(1), coin: "BTC".into(), sent: 1_000_000, asked: None };
        assert!(!u.expired(1_000_000 + trader::EXPIRES_AFTER_MS), "it could still land");
        assert!(u.expired(1_000_000 + trader::EXPIRES_AFTER_MS + 5_001), "now it never can");
    }

    /// Greptile #37: an order with no clear answer blocked new sends and also cleared the dead
    /// man's switch, so the possibly accepted order lost its timed cancel.
    #[test]
    fn an_uncertain_order_keeps_the_dead_mans_switch_armed() {
        let now = 1_793_667_000_000;
        let agent = AgentKey::generate();
        let prefs = crate::prefs::HlPrefs {
            enabled: true,
            master: Some(MASTER.into()),
            agent: Some(HlAgent {
                address: tr_hl::key::address_hex(&agent.address()),
                name: trader::agent_name("t", now + 86_400_000),
                valid_until: now + 86_400_000,
                network: tr_hl::Network::Testnet,
            }),
            dead_mans_switch: true,
            ..Default::default()
        };
        let mut hl = HlState::new(AgentKeyStore::memory());
        hl.key = Some(Arc::new(agent));
        assert!(deadman_wanted(&prefs, &hl, now) && placement_blocker(&prefs, &hl, now).is_none());
        hl.uncertain = Some(Uncertain { cloid: tr_hl::actions::cloid(1), coin: "BTC".into(), sent: now, asked: None });
        assert!(placement_blocker(&prefs, &hl, now).is_some(), "no new order while one is unresolved");
        assert!(deadman_wanted(&prefs, &hl, now), "but the timed cancel stays armed");
        let off = crate::prefs::HlPrefs { enabled: false, ..prefs.clone() };
        assert!(!deadman_wanted(&off, &hl, now), "KILL (orders off) disarms it");
        // Cancels still go through while the order is unresolved; new orders don't.
        let cancel = HlConfirm::Cancel { coin: "BTC".into(), oid: 1, line: String::new() };
        assert_eq!(confirm_blocker(&cancel, &prefs, &hl, now), None);
        assert!(confirm_blocker(&HlConfirm::Kill { orders: Vec::new() }, &off, &hl, now).is_none(), "KILL always");
        let reg = registry();
        let intent = OrderIntent {
            coin: "BTC".into(),
            is_buy: true,
            size: 0.0002,
            kind: OrderKind::Limit { px: 50_000.0, tif: Tif::Gtc },
            reduce_only: false,
        };
        let ctx = MarketCtx { mid: Some(76_000.0), leverage: Some(trader::Leverage { value: 3, cross: true }) };
        let plan = trader::plan_order(&reg, &intent, &ctx, &tr_hl::actions::cloid(2)).unwrap();
        let order = HlConfirm::Order { ticket: 1, plan: Box::new(plan), modify: None };
        assert!(confirm_blocker(&order, &prefs, &hl, now).is_some());
    }

    #[test]
    fn the_key_store_is_read_once_per_account() {
        let mut h = HlState::new(AgentKeyStore::memory());
        assert!(!should_load_key(false, &h, "testnet:0xa"), "no agent, nothing to load");
        assert!(should_load_key(true, &h, "testnet:0xa"));
        h.key_tried = Some("testnet:0xa".into());
        assert!(!should_load_key(true, &h, "testnet:0xa"), "found nothing once: not every frame");
        assert!(should_load_key(true, &h, "testnet:0xb"), "another account is read");
        h.key_loading = true;
        assert!(!should_load_key(true, &h, "testnet:0xb"));
    }

    #[test]
    fn testnet_mids_key_by_market_key() {
        let q = testnet_quotes(&recorded(), &registry());
        assert!(q.keys().all(|k| k.starts_with("xyz:")), "the fixture's mids are xyz's");
        assert!(q.values().all(|p| *p > 0.0));
    }
}
