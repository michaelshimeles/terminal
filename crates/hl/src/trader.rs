//! Everything that signs an L1 action goes through here, and every check happens here
//! first: the market must be tradable, sizes and prices are put on the grid, and the hard
//! caps in [`crate::caps`] are enforced before a signature exists.
//!
//! An order is planned ([`plan_order`]: rounded and checked, for the confirmation), then
//! signed ([`sign_order`]: checked again against the market at that moment). A plan can only
//! come from [`plan_order`], so nothing unchecked reaches the signer. Every L1 action carries
//! `expiresAfter` ≈ now + 30 s, so a signed action that is delayed can't land late.

use std::fmt;

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::actions::*;
use crate::assets::{Asset, AssetRegistry};
use crate::caps::{self, MAX_MARKET_SLIPPAGE, Refusal};
use crate::key::{SigningKey, address_hex};
use crate::network::{Network, SIGNATURE_CHAIN_ID};
use crate::nonce::NonceManager;
use crate::rounding::{Round, round_price, round_size, to_wire};
use crate::signing::{WireSignature, sign_l1};

/// How long a signed L1 action stays valid.
pub const EXPIRES_AFTER_MS: u64 = 30_000;
/// Default agent lifetime, and the most Hyperliquid allows.
pub const DEFAULT_AGENT_DAYS: u64 = 30;
pub const MAX_AGENT_DAYS: u64 = 180;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderKind {
    Limit {
        px: f64,
        tif: Tif,
    },
    /// An IOC limit [`MAX_MARKET_SLIPPAGE`] through the mid.
    Market,
    /// Take profit or stop loss: a market order once `trigger_px` trades.
    Trigger {
        trigger_px: f64,
        tpsl: Tpsl,
    },
}

/// What the user asked for, before rounding.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderIntent {
    pub coin: String,
    pub is_buy: bool,
    pub size: f64,
    pub kind: OrderKind,
    pub reduce_only: bool,
}

/// The account's leverage on one market (`activeAssetData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leverage {
    pub value: u32,
    pub cross: bool,
}

/// The market as the signer sees it now: the testnet mid and the account's leverage.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MarketCtx {
    pub mid: Option<f64>,
    pub leverage: Option<Leverage>,
}

/// An order rounded and checked. Only [`plan_order`] makes one.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderPlan {
    asset: Asset,
    intent: OrderIntent,
    size: f64,
    price: f64,
    trigger: Option<f64>,
    notional: f64,
    wire: OrderWire,
}

impl OrderPlan {
    pub fn asset(&self) -> &Asset {
        &self.asset
    }
    pub fn intent(&self) -> &OrderIntent {
        &self.intent
    }
    /// Size as sent (rounded down to the lot).
    pub fn size(&self) -> f64 {
        self.size
    }
    /// The limit price sent: the order's limit, or a market order's IOC cap.
    pub fn price(&self) -> f64 {
        self.price
    }
    pub fn trigger(&self) -> Option<f64> {
        self.trigger
    }
    /// Size × the higher of the price sent and the mid, in USD.
    pub fn notional(&self) -> f64 {
        self.notional
    }
    pub fn cloid(&self) -> &str {
        self.wire.c.as_deref().unwrap_or_default()
    }
    pub fn wire(&self) -> &OrderWire {
        &self.wire
    }
    /// `Limit GTC`, `Market (IOC, 1% cap)`, `Stop loss at 70000`.
    pub fn kind_label(&self) -> String {
        match self.intent.kind {
            OrderKind::Limit { tif, .. } => format!("Limit {}", tif.label()),
            OrderKind::Market => format!("Market (IOC, {:.0}% cap)", MAX_MARKET_SLIPPAGE * 100.0),
            OrderKind::Trigger { tpsl: Tpsl::Tp, .. } => "Take profit (market)".into(),
            OrderKind::Trigger { tpsl: Tpsl::Sl, .. } => "Stop loss (market)".into(),
        }
    }
}

/// A signed action, ready to post. `Debug` shows what it is, never the signature.
#[derive(Clone)]
pub struct Signed {
    pub kind: &'static str,
    action: Value,
    pub nonce: u64,
    signature: WireSignature,
    pub expires_after: Option<u64>,
}

impl Signed {
    /// The `/exchange` request body.
    pub fn body(&self) -> Value {
        json!({
            "action": self.action,
            "nonce": self.nonce,
            "signature": self.signature,
            "vaultAddress": Value::Null,
            "expiresAfter": self.expires_after,
        })
    }

    pub fn signature(&self) -> &WireSignature {
        &self.signature
    }

    /// The same action with a damaged signature (the live check's control).
    pub fn corrupted(&self) -> Signed {
        let mut s = self.clone();
        let last = s.signature.s.pop().unwrap_or('0');
        s.signature.s.push(if last == '1' { '2' } else { '1' });
        s
    }
}

impl fmt::Debug for Signed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signed")
            .field("kind", &self.kind)
            .field("nonce", &self.nonce)
            .field("expires_after", &self.expires_after)
            .finish_non_exhaustive()
    }
}

/// Round and check an order for the confirmation.
pub fn plan_order(
    reg: &AssetRegistry,
    intent: &OrderIntent,
    ctx: &MarketCtx,
    cloid: &str,
) -> Result<OrderPlan, Refusal> {
    let asset = reg.resolve(&intent.coin).map_err(Refusal::Invalid)?.clone();
    if !valid_cloid(cloid) {
        return Err(Refusal::Invalid("internal: malformed client order id".into()));
    }
    if !intent.size.is_finite() || intent.size <= 0.0 {
        return Err(Refusal::Invalid("Enter a size greater than zero.".into()));
    }
    let size = round_size(intent.size, asset.sz_decimals);
    if size <= 0.0 {
        return Err(Refusal::Invalid(format!(
            "{} {} rounds to zero ({} trades in steps of {}).",
            intent.size,
            asset.coin,
            asset.coin,
            10f64.powi(-(asset.sz_decimals as i32))
        )));
    }
    let mid = ctx.mid.filter(|m| m.is_finite() && *m > 0.0);
    // A buy's cap rounds down and a sell's floor up, so the slippage never exceeds the cap.
    let through = |px: f64| {
        let (raw, dir) = if intent.is_buy {
            (px * (1.0 + MAX_MARKET_SLIPPAGE), Round::Down)
        } else {
            (px * (1.0 - MAX_MARKET_SLIPPAGE), Round::Up)
        };
        round_price(raw, asset.sz_decimals, dir)
    };
    let positive = |px: f64, what: &str| {
        if px.is_finite() && px > 0.0 {
            Ok(px)
        } else {
            Err(Refusal::Invalid(format!("The {what} price must be greater than zero.")))
        }
    };
    let small = |what: &str| {
        Refusal::Invalid(format!(
            "The {what} price is too small for {} (it rounds to zero at {} decimals).",
            asset.coin,
            crate::rounding::price_decimals(0.0, asset.sz_decimals)
        ))
    };
    let (price, trigger, t) = match intent.kind {
        OrderKind::Limit { px, tif } => {
            (round_price(positive(px, "limit")?, asset.sz_decimals, Round::Nearest), None, OrderTypeWire::limit(tif))
        }
        OrderKind::Market => {
            let m = mid.ok_or_else(|| Refusal::NoMid { coin: asset.coin.clone() })?;
            (through(m), None, OrderTypeWire::limit(Tif::Ioc))
        }
        OrderKind::Trigger { trigger_px, tpsl } => {
            let trig = round_price(positive(trigger_px, "trigger")?, asset.sz_decimals, Round::Nearest);
            if trig <= 0.0 {
                return Err(small("trigger"));
            }
            (through(trig), Some(trig), OrderTypeWire::trigger(true, wire(trig)?, tpsl))
        }
    };
    if price <= 0.0 {
        return Err(small("limit"));
    }
    let order = OrderWire {
        a: asset.id,
        b: intent.is_buy,
        p: wire(price)?,
        s: wire(size)?,
        r: intent.reduce_only,
        t,
        c: Some(cloid.to_string()),
    };
    let plan = OrderPlan { asset, intent: intent.clone(), size, price, trigger, notional: 0.0, wire: order };
    let notional = check(&plan, ctx)?;
    Ok(OrderPlan { notional, ..plan })
}

fn wire(x: f64) -> Result<String, Refusal> {
    to_wire(x).map_err(|e| Refusal::Invalid(e.to_string()))
}

/// The caps, against the market in `ctx`. Returns the notional. Every order needs a current
/// mid: a limit through the market fills at the market, so its limit alone can't value it.
fn check(plan: &OrderPlan, ctx: &MarketCtx) -> Result<f64, Refusal> {
    let mid = ctx
        .mid
        .filter(|m| m.is_finite() && *m > 0.0)
        .ok_or_else(|| Refusal::NoMid { coin: plan.asset.coin.clone() })?;
    // Reducing a position is never blocked by its leverage; anything else needs 3× or less.
    if !plan.intent.reduce_only {
        let lev = ctx.leverage.ok_or_else(|| Refusal::LeverageUnknown { coin: plan.asset.coin.clone() })?;
        caps::check_leverage(lev.value)?;
    }
    // Value at the highest of the order's price, its trigger and the mid.
    let reference = [Some(plan.price), plan.trigger, Some(mid)].into_iter().flatten().fold(0.0, f64::max);
    let notional = plan.size * reference;
    caps::check_notional(notional)?;
    if plan.intent.kind == OrderKind::Market {
        caps::check_slippage((plan.price / mid - 1.0).abs())?;
    }
    Ok(notional)
}

fn signed<A: Serialize>(
    kind: &'static str,
    key: &impl SigningKey,
    nonces: &NonceManager,
    action: &A,
    now: u64,
) -> Result<Signed, Refusal> {
    let nonce = nonces.next(now);
    let expires_after = Some(now + EXPIRES_AFTER_MS);
    let signature = sign_l1(key, action, nonce, expires_after).map_err(|e| Refusal::Invalid(format!("{e:#}")))?;
    let action = serde_json::to_value(action).map_err(|e| Refusal::Invalid(e.to_string()))?;
    Ok(Signed { kind, action, nonce, signature, expires_after })
}

/// Sign a planned order, after checking it again against the market now.
pub fn sign_order(
    key: &impl SigningKey,
    nonces: &NonceManager,
    plan: &OrderPlan,
    ctx: &MarketCtx,
    now: u64,
) -> Result<Signed, Refusal> {
    check(plan, ctx)?;
    signed("order", key, nonces, &OrderAction::new(vec![plan.wire.clone()]), now)
}

/// Sign a modify of order `oid` into `plan`, with the same checks as a new order.
pub fn sign_modify(
    key: &impl SigningKey,
    nonces: &NonceManager,
    oid: u64,
    plan: &OrderPlan,
    ctx: &MarketCtx,
    now: u64,
) -> Result<Signed, Refusal> {
    check(plan, ctx)?;
    signed("modify", key, nonces, &ModifyAction::new(oid, plan.wire.clone()), now)
}

/// Cancel one order by id.
pub fn sign_cancel(
    key: &impl SigningKey,
    nonces: &NonceManager,
    asset: &Asset,
    oid: u64,
    now: u64,
) -> Result<Signed, Refusal> {
    signed("cancel", key, nonces, &CancelAction::new(vec![CancelWire { a: asset.id, o: oid }]), now)
}

/// Cancel one order by client order id.
pub fn sign_cancel_by_cloid(
    key: &impl SigningKey,
    nonces: &NonceManager,
    asset: &Asset,
    cloid: &str,
    now: u64,
) -> Result<Signed, Refusal> {
    if !valid_cloid(cloid) {
        return Err(Refusal::Invalid("malformed client order id".into()));
    }
    let action = CancelByCloidAction::new(vec![CancelByCloidWire { asset: asset.id, cloid: cloid.into() }]);
    signed("cancelByCloid", key, nonces, &action, now)
}

/// The kill switch: cancel every order in `orders` (`(coin, oid)`), one action per dex.
/// A coin the registry doesn't know (listed after it loaded) is skipped and returned, so one
/// stale entry never stops the other cancels.
pub fn sign_cancel_all(
    key: &impl SigningKey,
    nonces: &NonceManager,
    reg: &AssetRegistry,
    orders: &[(String, u64)],
    now: u64,
) -> Result<(Vec<Signed>, Vec<String>), Refusal> {
    // Delisted markets stay in the registry, so their orders still cancel.
    let skipped: Vec<String> = orders.iter().filter(|(c, _)| reg.get(c).is_none()).map(|(c, _)| c.clone()).collect();
    let mut out = Vec::new();
    for dex in reg.dexes() {
        let cancels: Vec<CancelWire> = orders
            .iter()
            .filter_map(|(coin, oid)| reg.get(coin).filter(|a| a.dex == dex).map(|a| CancelWire { a: a.id, o: *oid }))
            .collect();
        if !cancels.is_empty() {
            out.push(signed("cancel", key, nonces, &CancelAction::new(cancels), now)?);
        }
    }
    Ok((out, skipped))
}

/// Set leverage on one market: 1× to 3× (and no more than the market allows).
pub fn sign_update_leverage(
    key: &impl SigningKey,
    nonces: &NonceManager,
    asset: &Asset,
    cross: bool,
    leverage: u32,
    now: u64,
) -> Result<Signed, Refusal> {
    caps::check_leverage(leverage)?;
    if leverage == 0 || leverage > asset.max_leverage {
        return Err(Refusal::Invalid(format!("{} allows 1× to {}×.", asset.coin, asset.max_leverage)));
    }
    if cross && asset.only_isolated {
        return Err(Refusal::Invalid(format!("{} is isolated margin only.", asset.coin)));
    }
    signed("updateLeverage", key, nonces, &UpdateLeverageAction::new(asset.id, cross, leverage), now)
}

/// The dead man's switch: cancel all open orders at `at` (at least 5 s ahead), or clear it.
pub fn sign_schedule_cancel(
    key: &impl SigningKey,
    nonces: &NonceManager,
    at: Option<u64>,
    now: u64,
) -> Result<Signed, Refusal> {
    if at.is_some_and(|t| t < now + 5_000) {
        return Err(Refusal::Invalid("A scheduled cancel must be at least 5 seconds ahead.".into()));
    }
    signed("scheduleCancel", key, nonces, &ScheduleCancelAction::new(at), now)
}

/// Use up a nonce.
pub fn sign_noop(key: &impl SigningKey, nonces: &NonceManager, now: u64) -> Result<Signed, Refusal> {
    signed("noop", key, nonces, &NoopAction::default(), now)
}

/// `terminal-<device> valid_until <ms>`, the device reduced to 14 lower-case letters,
/// digits and dashes.
pub fn agent_name(device: &str, valid_until: u64) -> String {
    let mut d: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .map(|c| c.to_ascii_lowercase())
        .take(14)
        .collect();
    if d.is_empty() {
        d = "device".into();
    }
    format!("terminal-{d} valid_until {valid_until}")
}

/// The name an agent was approved under, without its `valid_until` suffix.
pub fn agent_base_name(name: &str) -> &str {
    name.split(" valid_until ").next().unwrap_or(name).trim()
}

/// The expiry in an agent name, if it has one.
pub fn agent_valid_until(name: &str) -> Option<u64> {
    name.split(" valid_until ").nth(1)?.trim().parse().ok()
}

/// An `approveAgent` for `agent`, named for this device, valid `days` (1 to 180).
pub fn approve_agent(agent: &Address, device: &str, days: u64, now: u64) -> ApproveAgentAction {
    let until = now + days.clamp(1, MAX_AGENT_DAYS) * DAY_MS;
    approve(agent, agent_name(device, until), now)
}

fn approve(agent: &Address, name: String, now: u64) -> ApproveAgentAction {
    ApproveAgentAction {
        kind: "approveAgent".into(),
        hyperliquid_chain: Network::Testnet.hyperliquid_chain().into(),
        signature_chain_id: format!("{SIGNATURE_CHAIN_ID:#x}"),
        agent_address: address_hex(agent),
        agent_name: name,
        nonce: now,
    }
}

/// Revoke: approve a fresh address under the same name, then drop its key, so the old agent
/// is replaced and nobody holds a key for the new one.
pub fn revoke_agent(base_name: &str, now: u64) -> ApproveAgentAction {
    // The key is dropped (and zeroized) at the end of this statement.
    let throwaway = crate::key::AgentKey::generate().address();
    approve(&throwaway, format!("{} valid_until {}", agent_base_name(base_name), now + DAY_MS), now)
}

/// A user-signed action's `/exchange` body.
pub fn user_signed_body(action: &ApproveAgentAction, signature: &WireSignature) -> Value {
    json!({
        "action": action,
        "nonce": action.nonce,
        "signature": signature,
        "vaultAddress": Value::Null,
        "expiresAfter": Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::fixtures::registry;
    use crate::caps::{MAX_LEVERAGE, MAX_ORDER_NOTIONAL_USD};
    use crate::key::AgentKey;
    use crate::signing::recover_l1;

    const NOW: u64 = 1_793_667_000_000;
    const CLOID: &str = "0x0000000000000000000000000000002a";

    fn lev(value: u32) -> Option<Leverage> {
        Some(Leverage { value, cross: true })
    }

    fn ctx(mid: f64) -> MarketCtx {
        MarketCtx { mid: Some(mid), leverage: lev(3) }
    }

    fn limit(coin: &str, buy: bool, size: f64, px: f64) -> OrderIntent {
        OrderIntent {
            coin: coin.into(),
            is_buy: buy,
            size,
            kind: OrderKind::Limit { px, tif: Tif::Gtc },
            reduce_only: false,
        }
    }

    #[test]
    fn a_far_limit_is_rounded_and_signed() {
        let reg = registry();
        let k = AgentKey::generate();
        let n = NonceManager::new();
        // BTC (szDecimals 5) far below a 76,662 mid: 0.000256 BTC → 0.00025 at 50,000 = $12.50.
        let plan = plan_order(&reg, &limit("btc", true, 0.000256, 50_000.0), &ctx(76_662.5), CLOID).unwrap();
        assert_eq!((plan.size(), plan.price(), plan.asset().id), (0.00025, 50_000.0, 3));
        assert!((plan.notional() - 76_662.5 * 0.00025).abs() < 1e-9, "valued at the higher of price and mid");
        assert_eq!(plan.wire().p, "50000");
        assert_eq!(plan.wire().s, "0.00025");
        assert_eq!(plan.cloid(), CLOID);
        let s = sign_order(&k, &n, &plan, &ctx(76_662.5), NOW).unwrap();
        assert_eq!(s.kind, "order");
        assert_eq!(s.expires_after, Some(NOW + EXPIRES_AFTER_MS));
        let body = s.body();
        assert_eq!(body["action"]["orders"][0]["a"], 3);
        assert!(body["action"].get("builder").is_none(), "never a builder fee");
        let action = OrderAction::new(vec![plan.wire().clone()]);
        assert_eq!(recover_l1(&action, s.nonce, s.expires_after, s.signature()).unwrap(), k.address());
        // xyz:AAPL (750009, szDecimals 3).
        let aapl = plan_order(&reg, &limit("xyz:AAPL", true, 0.0512, 250.123), &ctx(322.15), CLOID).unwrap();
        assert_eq!((aapl.asset().id, aapl.size(), aapl.price()), (750009, 0.051, 250.12));
        assert!(format!("{s:?}").contains("order") && !format!("{s:?}").contains(&s.signature().r));
    }

    #[test]
    fn orders_over_100_dollars_are_refused_before_signing() {
        let reg = registry();
        // 0.002 BTC at 76,662 is $153: refused even with a limit far below the market.
        let e = plan_order(&reg, &limit("BTC", true, 0.002, 50_000.0), &ctx(76_662.5), CLOID).unwrap_err();
        assert!(matches!(e, Refusal::Notional { .. }), "{e}");
        assert!(e.to_string().contains("$100"));
        // A sell limit below the market fills at the market, so the mid counts.
        let e = plan_order(&reg, &limit("xyz:AAPL", false, 0.32, 300.0), &ctx(322.15), CLOID).unwrap_err();
        assert!(matches!(e, Refusal::Notional { .. }), "{e}");
        // Exactly at the cap is fine.
        let ok = plan_order(&reg, &limit("xyz:AAPL", true, 0.4, 250.0), &ctx(250.0), CLOID).unwrap();
        assert!((ok.notional() - MAX_ORDER_NOTIONAL_USD).abs() < 1e-9);
        // Checked again at signing, against the market then.
        let plan = plan_order(&reg, &limit("xyz:AAPL", true, 0.3, 300.0), &ctx(320.0), CLOID).unwrap();
        let moved = MarketCtx { mid: Some(400.0), leverage: lev(3) };
        let e = sign_order(&AgentKey::generate(), &NonceManager::new(), &plan, &moved, NOW).unwrap_err();
        assert!(matches!(e, Refusal::Notional { .. }), "{e}");
    }

    #[test]
    fn leverage_over_3x_is_refused() {
        let reg = registry();
        let at = |l| MarketCtx { mid: Some(76_662.5), leverage: lev(l) };
        let o = limit("BTC", true, 0.0002, 50_000.0);
        let e = plan_order(&reg, &o, &at(20), CLOID).unwrap_err();
        assert_eq!(e, Refusal::Leverage { value: 20 }, "Hyperliquid's default 20× is over the cap");
        let unknown = MarketCtx { mid: Some(76_662.5), leverage: None };
        assert!(matches!(plan_order(&reg, &o, &unknown, CLOID).unwrap_err(), Refusal::LeverageUnknown { .. }));
        assert!(plan_order(&reg, &o, &at(MAX_LEVERAGE), CLOID).is_ok());
        // Reducing a position is never blocked by its leverage.
        let close = OrderIntent { reduce_only: true, is_buy: false, ..o.clone() };
        assert!(plan_order(&reg, &close, &at(20), CLOID).is_ok());
        // And Terminal never sets more than 3×.
        let (k, n) = (AgentKey::generate(), NonceManager::new());
        let btc = reg.resolve("BTC").unwrap();
        assert_eq!(sign_update_leverage(&k, &n, btc, true, 4, NOW).unwrap_err(), Refusal::Leverage { value: 4 });
        assert!(sign_update_leverage(&k, &n, btc, true, 0, NOW).is_err());
        let s = sign_update_leverage(&k, &n, btc, true, 3, NOW).unwrap();
        assert_eq!(s.body()["action"], json!({"type": "updateLeverage", "asset": 3, "isCross": true, "leverage": 3}));
        let isolated_only = reg.resolve("xyz:COIN").unwrap();
        assert!(sign_update_leverage(&k, &n, isolated_only, true, 2, NOW).is_err(), "cross on an isolated-only market");
        assert!(sign_update_leverage(&k, &n, isolated_only, false, 2, NOW).is_ok());
    }

    #[test]
    fn market_orders_are_ioc_within_1_percent() {
        let reg = registry();
        let m = OrderIntent { kind: OrderKind::Market, ..limit("xyz:AAPL", true, 0.1, 0.0) };
        let p = plan_order(&reg, &m, &ctx(322.15), CLOID).unwrap();
        assert_eq!(p.wire().t, OrderTypeWire::limit(Tif::Ioc));
        assert!(p.price() <= 322.15 * 1.01 && p.price() > 322.15 * 1.009, "{}", p.price());
        let sell = plan_order(&reg, &OrderIntent { is_buy: false, ..m.clone() }, &ctx(322.15), CLOID).unwrap();
        assert!(sell.price() >= 322.15 * 0.99 && sell.price() < 322.15 * 0.991, "{}", sell.price());
        let no_mid = MarketCtx { mid: None, leverage: lev(3) };
        assert!(matches!(plan_order(&reg, &m, &no_mid, CLOID).unwrap_err(), Refusal::NoMid { .. }));
        // The mid fell more than 1% below the confirmed cap: review again.
        let fell = MarketCtx { mid: Some(310.0), leverage: lev(3) };
        let e = sign_order(&AgentKey::generate(), &NonceManager::new(), &p, &fell, NOW).unwrap_err();
        assert!(matches!(e, Refusal::Slippage { .. }), "{e}");
    }

    #[test]
    fn triggers_are_market_stops_with_a_slippage_cap() {
        let reg = registry();
        let sl = OrderIntent {
            kind: OrderKind::Trigger { trigger_px: 300.0, tpsl: Tpsl::Sl },
            reduce_only: true,
            ..limit("xyz:AAPL", false, 0.1, 0.0)
        };
        let p = plan_order(&reg, &sl, &ctx(322.15), CLOID).unwrap();
        assert_eq!(p.wire().t, OrderTypeWire::trigger(true, "300".into(), Tpsl::Sl));
        assert_eq!(p.trigger(), Some(300.0));
        assert_eq!(p.price(), 297.0, "the fill limit is 1% through the trigger");
    }

    #[test]
    fn bad_orders_are_refused_with_reasons() {
        let reg = registry();
        let c = ctx(76_662.5);
        for (o, says) in [
            (limit("para:TOTAL2", true, 1.0, 1.0), "para"),
            (limit("HYPE/USDC", true, 1.0, 1.0), "spot"),
            (limit("BTC", true, 0.000001, 50_000.0), "rounds to zero"),
            (limit("BTC", true, 0.0002, -1.0), "price"),
            (limit("BTC", true, f64::NAN, 50_000.0), "size"),
        ] {
            let e = plan_order(&reg, &o, &c, CLOID).unwrap_err().to_string();
            assert!(e.contains(says), "{}: {e}", o.coin);
        }
        assert!(plan_order(&reg, &limit("BTC", true, 0.0002, 50_000.0), &c, "42").is_err(), "cloid format");
    }

    #[test]
    fn cancels_schedule_cancel_and_noop() {
        let reg = registry();
        let (k, n) = (AgentKey::generate(), NonceManager::new());
        let aapl = reg.resolve("xyz:AAPL").unwrap();
        let c = sign_cancel(&k, &n, aapl, 1, NOW).unwrap();
        assert_eq!(c.body()["action"], json!({"type": "cancel", "cancels": [{"a": 750009, "o": 1}]}));
        let c2 = sign_cancel_by_cloid(&k, &n, aapl, CLOID, NOW).unwrap();
        assert_eq!(c2.body()["action"]["cancels"][0]["cloid"], CLOID);
        assert!(c2.nonce > c.nonce, "nonces increase");
        let (all, skipped) = sign_cancel_all(
            &k,
            &n,
            &reg,
            &[
                ("BTC".into(), 1),
                ("xyz:AAPL".into(), 2),
                ("ETH".into(), 3),
                ("xyz:NEWLY".into(), 9),
                ("xyz:TSLA".into(), 4),
            ],
            NOW,
        )
        .unwrap();
        assert_eq!(all.len(), 2, "one cancel per dex");
        assert_eq!(all[0].body()["action"]["cancels"].as_array().unwrap().len(), 2);
        assert_eq!(
            all[1].body()["action"]["cancels"].as_array().unwrap().len(),
            2,
            "the unknown coin doesn't stop the rest"
        );
        assert_eq!(skipped, ["xyz:NEWLY"]);
        assert!(sign_cancel_all(&k, &n, &reg, &[], NOW).unwrap().0.is_empty());
        assert!(sign_schedule_cancel(&k, &n, Some(NOW + 1_000), NOW).is_err(), "at least 5 s ahead");
        let sc = sign_schedule_cancel(&k, &n, Some(NOW + 60_000), NOW).unwrap();
        assert_eq!(sc.body()["action"], json!({"type": "scheduleCancel", "time": NOW + 60_000}));
        assert_eq!(
            sign_schedule_cancel(&k, &n, None, NOW).unwrap().body()["action"],
            json!({"type": "scheduleCancel"})
        );
        let noop = sign_noop(&k, &n, NOW).unwrap();
        assert_eq!(noop.body()["action"], json!({"type": "noop"}));
        assert_eq!(noop.body()["expiresAfter"], NOW + EXPIRES_AFTER_MS);
        assert_eq!(noop.body()["vaultAddress"], Value::Null);
    }

    /// Greptile #37: without a mid, a sell limit below the market was valued at its limit.
    #[test]
    fn orders_need_a_current_mid_to_be_valued() {
        let reg = registry();
        let no_mid = MarketCtx { mid: None, leverage: lev(3) };
        let sell = limit("xyz:AAPL", false, 0.32, 300.0);
        assert!(matches!(plan_order(&reg, &sell, &no_mid, CLOID).unwrap_err(), Refusal::NoMid { .. }));
        let plan = plan_order(&reg, &limit("xyz:AAPL", true, 0.1, 250.0), &ctx(322.15), CLOID).unwrap();
        let e = sign_order(&AgentKey::generate(), &NonceManager::new(), &plan, &no_mid, NOW).unwrap_err();
        assert!(matches!(e, Refusal::NoMid { .. }), "checked again at signing: {e}");
    }

    /// Greptile #37: a price that rounds to zero was signed as "p":"0" (and panicked in debug).
    #[test]
    fn prices_that_round_to_zero_are_refused() {
        let reg = registry();
        let e = plan_order(&reg, &limit("BTC", true, 0.001, 0.00000001), &ctx(76_662.5), CLOID).unwrap_err();
        assert!(e.to_string().contains("too small"), "{e}");
        let stop = OrderIntent {
            kind: OrderKind::Trigger { trigger_px: 0.00000001, tpsl: Tpsl::Sl },
            reduce_only: true,
            ..limit("BTC", false, 0.001, 0.0)
        };
        assert!(plan_order(&reg, &stop, &ctx(76_662.5), CLOID).unwrap_err().to_string().contains("too small"));
    }

    #[test]
    fn agents_are_named_and_expire() {
        let agent: Address = "0x5e9ee1089755c3435139848e47e6635505d5a13a".parse().unwrap();
        let a = approve_agent(&agent, "Micky's MacBook Pro", DEFAULT_AGENT_DAYS, NOW);
        let until = NOW + DEFAULT_AGENT_DAYS * DAY_MS;
        assert_eq!(a.agent_name, format!("terminal-mickysmacbookp valid_until {until}"));
        assert_eq!((a.kind.as_str(), a.hyperliquid_chain.as_str(), a.nonce), ("approveAgent", "Testnet", NOW));
        assert_eq!(a.signature_chain_id, "0x66eee");
        assert_eq!(a.agent_address, "0x5e9ee1089755c3435139848e47e6635505d5a13a");
        assert_eq!(agent_base_name(&a.agent_name), "terminal-mickysmacbookp");
        assert_eq!(agent_valid_until(&a.agent_name), Some(until));
        let capped = approve_agent(&agent, "", 999, NOW);
        assert_eq!(agent_valid_until(&capped.agent_name), Some(NOW + MAX_AGENT_DAYS * DAY_MS));
        assert!(capped.agent_name.starts_with("terminal-device "), "{}", capped.agent_name);
        assert_eq!(agent_valid_until(&approve_agent(&agent, "x", 0, NOW).agent_name), Some(NOW + DAY_MS));
        // Revoking reuses the name with an address nobody holds a key for.
        let r = revoke_agent("terminal-mickysmacbookp", NOW + 5);
        assert_eq!(agent_base_name(&r.agent_name), "terminal-mickysmacbookp");
        assert_ne!(r.agent_address, a.agent_address);
        assert!(crate::key::parse_address(&r.agent_address).is_ok());
        let body = user_signed_body(&a, &WireSignature { r: "0x1".into(), s: "0x2".into(), v: 27 });
        assert_eq!(body["nonce"], NOW);
        assert_eq!(body["action"]["agentName"], a.agent_name);
    }
}
