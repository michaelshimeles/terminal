//! Asset ids and trading rules per market, read from the network every session.
//!
//! - A core perp's id is its index in `meta.universe`.
//! - A HIP-3 perp's id is `100000 + dex_index × 10000 + index`, where `dex_index` is the dex's
//!   position in `perpDexs` (index 0 is the core dex) and `index` its place in `meta{dex}`.
//!
//! Ids differ between networks and change as dexes are deployed, so none is hard-coded
//! (testnet's `xyz` is dex 65 today, mainnet's is 1). This build trades the core dex and the
//! `xyz` HIP-3 dex only: `para`, `io`, `mkts`, other dexes and spot are refused with a reason.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// HIP-3 dexes this build trades.
pub const SUPPORTED_DEXES: [&str; 1] = ["xyz"];
/// HIP-3 dexes that exist on mainnet but are refused in this build.
pub const REFUSED_DEXES: [&str; 3] = ["para", "io", "mkts"];

/// One perp market.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    /// Exactly as Hyperliquid names it: `BTC`, `kPEPE`, `xyz:AAPL`.
    pub coin: String,
    pub id: u32,
    /// `""` for the core dex, else the HIP-3 dex (`xyz`).
    pub dex: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    /// Isolated margin only (no cross).
    pub only_isolated: bool,
    pub delisted: bool,
}

impl Asset {
    pub fn is_hip3(&self) -> bool {
        !self.dex.is_empty()
    }

    /// `PERP · HL xyz` for HIP-3, `PERP · HL` for the core dex.
    pub fn label(&self) -> String {
        if self.is_hip3() { format!("PERP · HL {}", self.dex) } else { "PERP · HL".into() }
    }

    /// The terminal's market key for charts and quotes: `xyz:AAPL` for a HIP-3 perp, the
    /// crypto pair (`BTCUSD Curncy`) for a core perp.
    pub fn app_key(&self) -> String {
        app_key(&self.coin)
    }
}

/// [`Asset::app_key`] for a coin name.
pub fn app_key(coin: &str) -> String {
    if coin.contains(':') { coin.to_string() } else { format!("{}USD Curncy", coin.to_ascii_uppercase()) }
}

/// Why a market can't be traded, in words for the ticket.
pub fn refusal(coin: &str) -> Option<String> {
    let c = coin.trim();
    if c.contains('/') || c.starts_with('@') {
        return Some(format!("{c} is a spot market. This build trades Hyperliquid perps only (no spot)."));
    }
    if let Some((dex, _)) = c.split_once(':') {
        let dex = dex.to_ascii_lowercase();
        if REFUSED_DEXES.contains(&dex.as_str()) {
            return Some(format!(
                "{c} is on the {dex} HIP-3 dex. This build trades the core Hyperliquid perps and the xyz dex only."
            ));
        }
        if !SUPPORTED_DEXES.contains(&dex.as_str()) {
            return Some(format!("{c}: this build trades the core Hyperliquid perps and the xyz dex only."));
        }
    }
    None
}

/// Every tradable perp on the core and `xyz` dexes, by name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssetRegistry {
    by_name: HashMap<String, Asset>,
    /// `(perpDexs index, name)` of the dexes loaded, core first.
    dexes: Vec<(usize, String)>,
}

impl AssetRegistry {
    /// Build from `perpDexs`, the core `meta`, and `meta{dex}` for each supported dex.
    pub fn from_info(perp_dexs: &Value, core_meta: &Value, dex_metas: &[(&str, &Value)]) -> Result<Self> {
        let dexes = perp_dexs.as_array().context("perpDexs is not a list")?;
        let mut reg = Self::default();
        reg.add(0, "", core_meta)?;
        for (name, meta) in dex_metas {
            if !SUPPORTED_DEXES.contains(name) {
                bail!("the {name} dex is not supported in this build");
            }
            let index = dexes
                .iter()
                .position(|d| d.get("name").and_then(Value::as_str) == Some(*name))
                .with_context(|| format!("perpDexs has no {name} dex"))?;
            reg.add(index, name, meta)?;
        }
        Ok(reg)
    }

    fn add(&mut self, dex_index: usize, dex: &str, meta: &Value) -> Result<()> {
        let universe = meta.get("universe").and_then(Value::as_array).context("meta has no universe")?;
        for (i, a) in universe.iter().enumerate() {
            let Some(coin) = a.get("name").and_then(Value::as_str) else { continue };
            let id = if dex.is_empty() { i } else { 100_000 + dex_index * 10_000 + i };
            let int = |k: &str| a.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            let asset = Asset {
                coin: coin.to_string(),
                id: u32::try_from(id).context("asset id out of range")?,
                dex: dex.to_string(),
                sz_decimals: int("szDecimals"),
                max_leverage: int("maxLeverage").max(1),
                only_isolated: a.get("onlyIsolated").and_then(Value::as_bool).unwrap_or(false),
                delisted: a.get("isDelisted").and_then(Value::as_bool).unwrap_or(false),
            };
            self.by_name.insert(coin.to_ascii_lowercase(), asset);
        }
        self.dexes.push((dex_index, dex.to_string()));
        Ok(())
    }

    /// The market for `coin` (any case: `btc`, `XYZ:aapl`), or why it can't be traded.
    pub fn resolve(&self, coin: &str) -> Result<&Asset, String> {
        let c = coin.trim();
        if c.is_empty() {
            return Err("Enter a market (BTC, xyz:AAPL).".into());
        }
        if let Some(why) = refusal(c) {
            return Err(why);
        }
        match self.by_name.get(&c.to_ascii_lowercase()) {
            Some(a) if a.delisted => Err(format!("{} is delisted on Hyperliquid.", a.coin)),
            Some(a) => Ok(a),
            None => {
                // `AAPL` on Hyperliquid is the xyz perp, never the stock.
                let hip3 = format!("xyz:{c}").to_ascii_lowercase();
                match self.by_name.get(&hip3).filter(|a| !a.delisted) {
                    Some(a) => Err(format!(
                        "{} is not a core Hyperliquid market. Hyperliquid has the perp {} ({}). Trade that instead?",
                        c.to_ascii_uppercase(),
                        a.coin,
                        a.label()
                    )),
                    None => Err(format!("{c} is not a Hyperliquid perp on testnet.")),
                }
            }
        }
    }

    pub fn get(&self, coin: &str) -> Option<&Asset> {
        self.by_name.get(&coin.trim().to_ascii_lowercase())
    }

    /// The loaded dexes, core (`""`) first.
    pub fn dexes(&self) -> impl Iterator<Item = &str> {
        self.dexes.iter().map(|(_, d)| d.as_str())
    }

    /// A dex's position in `perpDexs`.
    pub fn dex_index(&self, dex: &str) -> Option<usize> {
        self.dexes.iter().find(|(_, d)| d == dex).map(|(i, _)| *i)
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn assets(&self) -> impl Iterator<Item = &Asset> {
        self.by_name.values()
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub fn json(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    /// The registry from testnet responses recorded on 2026-11-02.
    pub fn registry() -> AssetRegistry {
        AssetRegistry::from_info(
            &json("testnet_perp_dexs.json"),
            &json("testnet_meta_core.json"),
            &[("xyz", &json("testnet_meta_xyz.json"))],
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn ids_come_from_perp_dexs_and_meta() {
        let r = registry();
        assert_eq!(r.dex_index("xyz"), Some(65), "xyz is dex 65 on testnet (recorded)");
        let btc = r.resolve("btc").unwrap();
        assert_eq!((btc.id, btc.sz_decimals, btc.max_leverage, btc.dex.as_str()), (3, 5, 40, ""));
        let aapl = r.resolve("XYZ:aapl").unwrap();
        assert_eq!((aapl.id, aapl.sz_decimals, aapl.coin.as_str()), (750009, 3, "xyz:AAPL"));
        assert_eq!(aapl.label(), "PERP · HL xyz");
        assert_eq!(aapl.app_key(), "xyz:AAPL");
        assert_eq!(btc.app_key(), "BTCUSD Curncy");
        assert!(r.resolve("xyz:COIN").unwrap().only_isolated, "isolated-only HIP-3 markets are flagged");
        assert_eq!(r.dexes().collect::<Vec<_>>(), ["", "xyz"]);
    }

    #[test]
    fn the_same_market_gets_another_id_on_another_dex_index() {
        // perpDexs with xyz at index 1, as on mainnet: ids move with it.
        let dexes = serde_json::json!([null, {"name": "xyz"}]);
        let r = AssetRegistry::from_info(
            &dexes,
            &json("testnet_meta_core.json"),
            &[("xyz", &json("testnet_meta_xyz.json"))],
        )
        .unwrap();
        assert_eq!(r.resolve("xyz:AAPL").unwrap().id, 110009);
    }

    #[test]
    fn unsupported_markets_are_refused_with_a_reason() {
        let r = registry();
        for (coin, says) in [
            ("para:TOTAL2", "para HIP-3 dex"),
            ("io:OAI", "io HIP-3 dex"),
            ("mkts:US500", "mkts HIP-3 dex"),
            ("abc:XYZ", "xyz dex only"),
            ("HYPE/USDC", "spot"),
            ("@107", "spot"),
            ("AAPL", "Hyperliquid has the perp xyz:AAPL (PERP · HL xyz)"),
            ("NOPE", "not a Hyperliquid perp"),
        ] {
            let e = r.resolve(coin).unwrap_err();
            assert!(e.contains(says), "{coin}: {e}");
        }
        let delisted = r.assets().find(|a| a.delisted).unwrap().coin.clone();
        assert!(r.resolve(&delisted).unwrap_err().contains("delisted"));
        assert!(
            AssetRegistry::from_info(
                &json("testnet_perp_dexs.json"),
                &json("testnet_meta_core.json"),
                &[("para", &json("testnet_meta_xyz.json"))]
            )
            .is_err()
        );
    }
}
