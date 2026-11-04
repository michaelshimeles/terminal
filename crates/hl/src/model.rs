//! Account data from `/info` and the user stream: positions and margin per dex, open orders,
//! fills, funding and per-market leverage. Hyperliquid sends decimals as strings; anything
//! missing or malformed becomes `None` or 0 rather than failing the whole message.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::trader::Leverage;

pub(crate) fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn leverage(v: &Value) -> Option<Leverage> {
    let value = v.get("value")?.as_u64()? as u32;
    Some(Leverage { value, cross: v.get("type").and_then(Value::as_str) == Some("cross") })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Margin {
    pub account_value: f64,
    pub total_ntl_pos: f64,
    pub total_margin_used: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerpPosition {
    pub coin: String,
    /// Signed size: negative is short.
    pub szi: f64,
    pub entry_px: Option<f64>,
    pub position_value: f64,
    pub unrealized_pnl: f64,
    pub return_on_equity: f64,
    pub liquidation_px: Option<f64>,
    pub margin_used: f64,
    pub leverage: Option<Leverage>,
    pub cum_funding: f64,
}

impl PerpPosition {
    /// The mark implied by value and size.
    pub fn mark(&self) -> Option<f64> {
        (self.szi.abs() > 0.0).then(|| self.position_value / self.szi.abs())
    }
}

/// `clearinghouseState` for one dex.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Clearinghouse {
    pub dex: String,
    pub margin: Margin,
    pub withdrawable: f64,
    pub positions: Vec<PerpPosition>,
    pub time: u64,
}

pub fn parse_clearinghouse(dex: &str, v: &Value) -> Clearinghouse {
    let m = &v["marginSummary"];
    let positions = v["assetPositions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let p = &a["position"];
            let coin = text(&p["coin"]);
            let szi = num(&p["szi"])?;
            (!coin.is_empty()).then(|| PerpPosition {
                coin,
                szi,
                entry_px: num(&p["entryPx"]),
                position_value: num(&p["positionValue"]).unwrap_or(0.0),
                unrealized_pnl: num(&p["unrealizedPnl"]).unwrap_or(0.0),
                return_on_equity: num(&p["returnOnEquity"]).unwrap_or(0.0),
                liquidation_px: num(&p["liquidationPx"]),
                margin_used: num(&p["marginUsed"]).unwrap_or(0.0),
                leverage: leverage(&p["leverage"]),
                cum_funding: num(&p["cumFunding"]["allTime"]).unwrap_or(0.0),
            })
        })
        .collect();
    Clearinghouse {
        dex: dex.to_string(),
        margin: Margin {
            account_value: num(&m["accountValue"]).unwrap_or(0.0),
            total_ntl_pos: num(&m["totalNtlPos"]).unwrap_or(0.0),
            total_margin_used: num(&m["totalMarginUsed"]).unwrap_or(0.0),
        },
        withdrawable: num(&v["withdrawable"]).unwrap_or(0.0),
        positions,
        time: v["time"].as_u64().unwrap_or(0),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub coin: String,
    pub is_buy: bool,
    pub limit_px: f64,
    pub sz: f64,
    pub orig_sz: f64,
    pub oid: u64,
    pub timestamp: u64,
    pub cloid: Option<String>,
    pub reduce_only: bool,
    /// `Limit`, `Stop Market`, `Take Profit Market`, …
    pub order_type: String,
    pub tif: Option<String>,
    pub trigger_px: Option<f64>,
}

/// `openOrders` / `frontendOpenOrders` rows (the plain form lacks the type fields).
pub fn parse_open_orders(v: &Value) -> Vec<OpenOrder> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| {
            Some(OpenOrder {
                coin: Some(text(&o["coin"])).filter(|c| !c.is_empty())?,
                is_buy: o["side"].as_str() == Some("B"),
                limit_px: num(&o["limitPx"])?,
                sz: num(&o["sz"]).unwrap_or(0.0),
                orig_sz: num(&o["origSz"]).unwrap_or(0.0),
                oid: o["oid"].as_u64()?,
                timestamp: o["timestamp"].as_u64().unwrap_or(0),
                cloid: o["cloid"].as_str().map(str::to_string),
                reduce_only: o["reduceOnly"].as_bool().unwrap_or(false),
                order_type: Some(text(&o["orderType"])).filter(|t| !t.is_empty()).unwrap_or_else(|| "Limit".into()),
                tif: o["tif"].as_str().map(str::to_string),
                trigger_px: num(&o["triggerPx"]).filter(|p| *p > 0.0 && o["isTrigger"].as_bool() == Some(true)),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub coin: String,
    pub px: f64,
    pub sz: f64,
    pub is_buy: bool,
    pub time: u64,
    /// `Open Long`, `Close Short`, …
    pub dir: String,
    pub closed_pnl: f64,
    /// Includes any builder fee (always none from Terminal).
    pub fee: f64,
    pub oid: u64,
    /// The globally unique trade id.
    pub tid: u64,
}

pub fn parse_fills(v: &Value) -> Vec<Fill> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some(Fill {
                coin: Some(text(&f["coin"])).filter(|c| !c.is_empty())?,
                px: num(&f["px"])?,
                sz: num(&f["sz"])?,
                is_buy: f["side"].as_str() == Some("B"),
                time: f["time"].as_u64().unwrap_or(0),
                dir: text(&f["dir"]),
                closed_pnl: num(&f["closedPnl"]).unwrap_or(0.0),
                fee: num(&f["fee"]).unwrap_or(0.0),
                oid: f["oid"].as_u64().unwrap_or(0),
                tid: f["tid"].as_u64()?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Funding {
    pub time: u64,
    pub coin: String,
    pub usdc: f64,
    pub szi: f64,
    pub rate: f64,
}

pub fn parse_fundings(v: &Value) -> Vec<Funding> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some(Funding {
                time: f["time"].as_u64()?,
                coin: text(&f["coin"]),
                usdc: num(&f["usdc"]).unwrap_or(0.0),
                szi: num(&f["szi"]).unwrap_or(0.0),
                rate: num(&f["fundingRate"]).unwrap_or(0.0),
            })
        })
        .collect()
}

/// `activeAssetData`: the account's leverage and what it can trade on one market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetData {
    pub coin: String,
    pub leverage: Option<Leverage>,
    /// `[buy, sell]`.
    pub max_trade_szs: [f64; 2],
    pub available_to_trade: [f64; 2],
    pub mark_px: Option<f64>,
}

pub fn parse_asset_data(v: &Value) -> Option<AssetData> {
    let pair = |k: &str| [num(&v[k][0]).unwrap_or(0.0), num(&v[k][1]).unwrap_or(0.0)];
    Some(AssetData {
        coin: Some(text(&v["coin"])).filter(|c| !c.is_empty())?,
        leverage: leverage(&v["leverage"]),
        max_trade_szs: pair("maxTradeSzs"),
        available_to_trade: pair("availableToTrade"),
        mark_px: num(&v["markPx"]),
    })
}

/// A change to one order (`orderUpdates`): `open`, `filled`, `canceled`, `rejected`, …
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderUpdate {
    pub coin: String,
    pub oid: u64,
    pub cloid: Option<String>,
    pub status: String,
    pub is_buy: bool,
    pub limit_px: f64,
    pub sz: f64,
    pub time: u64,
}

pub fn parse_order_updates(v: &Value) -> Vec<OrderUpdate> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| {
            let o = &u["order"];
            Some(OrderUpdate {
                coin: text(&o["coin"]),
                oid: o["oid"].as_u64()?,
                cloid: o["cloid"].as_str().map(str::to_string),
                status: text(&u["status"]),
                is_buy: o["side"].as_str() == Some("B"),
                limit_px: num(&o["limitPx"]).unwrap_or(0.0),
                sz: num(&o["sz"]).unwrap_or(0.0),
                time: u["statusTimestamp"].as_u64().unwrap_or(0),
            })
        })
        .collect()
}

/// An approved API wallet (`extraAgents`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtraAgent {
    pub address: String,
    pub name: String,
    pub valid_until: Option<u64>,
}

pub fn parse_extra_agents(v: &Value) -> Vec<ExtraAgent> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(ExtraAgent {
                address: a["address"].as_str()?.to_ascii_lowercase(),
                name: text(&a["name"]),
                valid_until: a["validUntil"].as_u64(),
            })
        })
        .collect()
}

/// USDC in the account's spot balances (`spotClearinghouseState`): what a deposit lands in.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpotUsdc {
    pub total: f64,
    /// Held by open spot orders.
    pub hold: f64,
}

pub fn spot_usdc(v: &Value) -> SpotUsdc {
    v["balances"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["coin"] == "USDC")
        .map(|b| SpotUsdc { total: num(&b["total"]).unwrap_or(0.0), hold: num(&b["hold"]).unwrap_or(0.0) })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::fixtures::json;

    /// The response shape from Hyperliquid's info-endpoint docs (spot), not a recording.
    #[test]
    fn spot_usdc_is_read_from_the_spot_balances() {
        let v: Value = serde_json::from_str(
            r#"{"balances":[{"coin":"USDC","token":0,"hold":"1.5","total":"14.625485","entryNtl":"0.0"},
                {"coin":"PURR","token":1,"hold":"0","total":"2000","entryNtl":"1234.56"}]}"#,
        )
        .unwrap();
        assert_eq!(spot_usdc(&v), SpotUsdc { total: 14.625485, hold: 1.5 });
        assert_eq!(spot_usdc(&serde_json::json!({"balances": []})), SpotUsdc::default());
        assert_eq!(spot_usdc(&serde_json::json!({})), SpotUsdc::default());
    }

    #[test]
    fn recorded_testnet_account_parses() {
        let c = parse_clearinghouse("xyz", &json("testnet_clearinghouse_xyz.json"));
        assert_eq!(c.positions.len(), 3);
        let tsla = c.positions.iter().find(|p| p.coin == "xyz:TSLA").unwrap();
        assert_eq!((tsla.szi, tsla.entry_px), (-1.483, Some(404.349)));
        assert_eq!(tsla.leverage, Some(Leverage { value: 10, cross: true }));
        assert!((tsla.mark().unwrap() - 668.92198 / 1.483).abs() < 1e-9);
        assert!(c.margin.account_value > 0.0 && c.withdrawable > 0.0);
        let o = parse_open_orders(&json("testnet_frontend_open_orders_xyz.json"));
        assert_eq!(o.len(), 1);
        assert_eq!(
            (o[0].coin.as_str(), o[0].is_buy, o[0].limit_px, o[0].oid),
            ("xyz:AAPL", false, 346.21, 63919996139)
        );
        assert_eq!((o[0].tif.as_deref(), o[0].trigger_px), (Some("Alo"), None));
        assert!(parse_open_orders(&serde_json::json!([{"coin": "BTC"}])).is_empty(), "incomplete rows are skipped");
    }

    #[test]
    fn streams_and_agents_parse() {
        let a = parse_asset_data(&serde_json::json!({
            "coin": "BTC", "leverage": {"type": "isolated", "value": 3},
            "maxTradeSzs": ["0.1", "0.2"], "availableToTrade": ["10", "20"], "markPx": "76613.0"
        }))
        .unwrap();
        assert_eq!(a.leverage, Some(Leverage { value: 3, cross: false }));
        assert_eq!((a.max_trade_szs, a.mark_px), ([0.1, 0.2], Some(76613.0)));
        let u = parse_order_updates(&serde_json::json!([{
            "order": {"coin": "BTC", "side": "B", "limitPx": "50000", "sz": "0.0002", "oid": 7, "timestamp": 1, "origSz": "0.0002", "cloid": "0x0000000000000000000000000000002a"},
            "status": "canceled", "statusTimestamp": 5
        }]));
        assert_eq!((u[0].status.as_str(), u[0].oid, u[0].is_buy), ("canceled", 7, true));
        let agents =
            parse_extra_agents(&serde_json::json!([{"address": "0xABC", "name": "terminal-mac", "validUntil": 9}]));
        assert_eq!(
            agents[0],
            ExtraAgent { address: "0xabc".into(), name: "terminal-mac".into(), valid_until: Some(9) }
        );
        let f = parse_fundings(
            &serde_json::json!([{"time": 1, "coin": "BTC", "usdc": "-0.01", "szi": "0.1", "fundingRate": "0.0001"}]),
        );
        assert_eq!(f[0].usdc, -0.01);
    }
}
