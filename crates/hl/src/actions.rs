//! Exchange actions as Hyperliquid hashes them.
//!
//! L1 actions are msgpack-encoded with named fields **in declaration order**, and that order
//! is part of the signature: each struct below lists its fields exactly as the Python SDK
//! builds them. Optional fields are left out rather than sent as `null` or `false`
//! (Hyperliquid rejects a cancel hashed with `f: false`, for instance). There is no
//! `builder` field anywhere: this build never charges a builder fee.

use serde::{Deserialize, Serialize};

/// Limit time in force: `Gtc`, `Ioc` or `Alo` (post-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tif {
    Gtc,
    Ioc,
    Alo,
}

impl Tif {
    pub fn label(self) -> &'static str {
        match self {
            Tif::Gtc => "GTC",
            Tif::Ioc => "IOC",
            Tif::Alo => "Post-only",
        }
    }
}

/// Trigger kind: take profit or stop loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tpsl {
    #[serde(rename = "tp")]
    Tp,
    #[serde(rename = "sl")]
    Sl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LimitWire {
    pub tif: Tif,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerWire {
    pub is_market: bool,
    pub trigger_px: String,
    pub tpsl: Tpsl,
}

/// `{"limit": {...}}` or `{"trigger": {...}}`: one of the two, never both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderTypeWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerWire>,
}

impl OrderTypeWire {
    pub fn limit(tif: Tif) -> Self {
        Self { limit: Some(LimitWire { tif }), trigger: None }
    }

    pub fn trigger(is_market: bool, trigger_px: String, tpsl: Tpsl) -> Self {
        Self { limit: None, trigger: Some(TriggerWire { is_market, trigger_px, tpsl }) }
    }
}

/// One order: asset, is buy, price, size, reduce only, type, client order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderWire {
    pub a: u32,
    pub b: bool,
    pub p: String,
    pub s: String,
    pub r: bool,
    pub t: OrderTypeWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub orders: Vec<OrderWire>,
    pub grouping: &'static str,
}

impl OrderAction {
    pub(crate) fn new(orders: Vec<OrderWire>) -> Self {
        Self { kind: "order", orders, grouping: "na" }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelWire {
    pub a: u32,
    pub o: u64,
}

/// Cancel by order id. `f` (fast) is left out: hashed as `false` it is rejected, and `true`
/// is refused for trigger orders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub cancels: Vec<CancelWire>,
}

impl CancelAction {
    pub fn new(cancels: Vec<CancelWire>) -> Self {
        Self { kind: "cancel", cancels }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelByCloidWire {
    pub asset: u32,
    pub cloid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelByCloidAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub cancels: Vec<CancelByCloidWire>,
}

impl CancelByCloidAction {
    pub fn new(cancels: Vec<CancelByCloidWire>) -> Self {
        Self { kind: "cancelByCloid", cancels }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModifyAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub oid: u64,
    pub order: OrderWire,
}

impl ModifyAction {
    pub(crate) fn new(oid: u64, order: OrderWire) -> Self {
        Self { kind: "modify", oid, order }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateLeverageAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub asset: u32,
    pub is_cross: bool,
    pub leverage: u32,
}

impl UpdateLeverageAction {
    pub(crate) fn new(asset: u32, is_cross: bool, leverage: u32) -> Self {
        Self { kind: "updateLeverage", asset, is_cross, leverage }
    }
}

/// The dead man's switch: cancel every open order at `time` (ms), or clear the schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduleCancelAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<u64>,
}

impl ScheduleCancelAction {
    pub fn new(time: Option<u64>) -> Self {
        Self { kind: "scheduleCancel", time }
    }
}

/// Uses up a nonce without doing anything (invalidates an in-flight action).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoopAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl Default for NoopAction {
    fn default() -> Self {
        Self { kind: "noop" }
    }
}

/// Approve an API wallet. A user-signed action (EIP-712 `HyperliquidTransaction:ApproveAgent`),
/// signed by the main wallet, so it is JSON on the wire and not msgpack-hashed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApproveAgentAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub hyperliquid_chain: String,
    /// The chain the wallet signs on, as `0x…` hex: always `0x66eee` (testnet).
    pub signature_chain_id: String,
    /// Lower-case `0x…`.
    pub agent_address: String,
    /// `terminal-<device> valid_until <ms>`: always named, never the unnamed slot.
    pub agent_name: String,
    pub nonce: u64,
}

impl ApproveAgentAction {
    pub fn chain_id(&self) -> anyhow::Result<u64> {
        let hex = self.signature_chain_id.trim_start_matches("0x");
        Ok(u64::from_str_radix(hex, 16)?)
    }
}

/// A 128-bit client order id, `0x` and 32 hex digits.
pub fn cloid(n: u128) -> String {
    format!("0x{n:032x}")
}

/// Whether `s` is a well-formed client order id.
pub fn valid_cloid(s: &str) -> bool {
    s.len() == 34 && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shapes_match_the_exchange_docs() {
        let o = OrderWire {
            a: 3,
            b: true,
            p: "50000".into(),
            s: "0.0002".into(),
            r: false,
            t: OrderTypeWire::limit(Tif::Gtc),
            c: Some(cloid(1)),
        };
        let json = serde_json::to_string(&OrderAction::new(vec![o.clone()])).unwrap();
        assert_eq!(
            json,
            r#"{"type":"order","orders":[{"a":3,"b":true,"p":"50000","s":"0.0002","r":false,"t":{"limit":{"tif":"Gtc"}},"c":"0x00000000000000000000000000000001"}],"grouping":"na"}"#
        );
        assert!(!json.contains("builder"), "no builder fee is ever sent");
        let trig = OrderTypeWire::trigger(true, "103".into(), Tpsl::Sl);
        assert_eq!(
            serde_json::to_string(&trig).unwrap(),
            r#"{"trigger":{"isMarket":true,"triggerPx":"103","tpsl":"sl"}}"#
        );
        let cancel = serde_json::to_string(&CancelAction::new(vec![CancelWire { a: 3, o: 42 }])).unwrap();
        assert_eq!(cancel, r#"{"type":"cancel","cancels":[{"a":3,"o":42}]}"#, "no `f` field");
        let by = CancelByCloidAction::new(vec![CancelByCloidWire { asset: 750009, cloid: cloid(7) }]);
        assert_eq!(
            serde_json::to_string(&by).unwrap(),
            r#"{"type":"cancelByCloid","cancels":[{"asset":750009,"cloid":"0x00000000000000000000000000000007"}]}"#
        );
        let lev = serde_json::to_string(&UpdateLeverageAction::new(3, true, 3)).unwrap();
        assert_eq!(lev, r#"{"type":"updateLeverage","asset":3,"isCross":true,"leverage":3}"#);
        assert_eq!(serde_json::to_string(&ScheduleCancelAction::new(None)).unwrap(), r#"{"type":"scheduleCancel"}"#);
        assert_eq!(serde_json::to_string(&NoopAction::default()).unwrap(), r#"{"type":"noop"}"#);
        let m = serde_json::to_string(&ModifyAction::new(9, o)).unwrap();
        assert!(m.starts_with(r#"{"type":"modify","oid":9,"order":{"a":3,"#), "{m}");
    }

    #[test]
    fn cloids_are_128_bit_hex() {
        assert_eq!(cloid(1), "0x00000000000000000000000000000001");
        assert!(valid_cloid(&cloid(u128::MAX)));
        assert!(!valid_cloid("0x1") && !valid_cloid(&format!("0x{}", "g".repeat(32))));
    }
}
