//! The one network this build can reach: Hyperliquid's testnet.
//!
//! There is no mainnet variant, constant or switch. Everything that names a network (the
//! endpoints, the phantom agent's `source`, `hyperliquidChain` in user-signed actions)
//! comes from [`Network::Testnet`], so nothing in this crate can address or sign for
//! mainnet. A preference file that says `"mainnet"` still reads as testnet (see the
//! `Deserialize` impl).

use serde::{Deserialize, Deserializer, Serialize};

/// Testnet REST base (`/info`, `/exchange`).
pub const API_URL: &str = "https://api.hyperliquid-testnet.xyz";
/// Testnet websocket.
pub const WS_URL: &str = "wss://api.hyperliquid-testnet.xyz/ws";
/// The chain id user-signed actions are signed under (as the Python SDK does). It only
/// names the EIP-712 domain the wallet signs in; `hyperliquidChain` picks the network.
pub const SIGNATURE_CHAIN_ID: u64 = 0x66eee;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
pub enum Network {
    #[default]
    Testnet,
}

impl Network {
    pub fn info_url(self) -> String {
        format!("{API_URL}/info")
    }

    pub fn exchange_url(self) -> String {
        format!("{API_URL}/exchange")
    }

    pub fn ws_url(self) -> &'static str {
        WS_URL
    }

    /// The phantom agent's `source` for L1 actions: `"b"` is testnet.
    pub fn phantom_source(self) -> &'static str {
        "b"
    }

    /// `hyperliquidChain` in user-signed actions such as `approveAgent`.
    pub fn hyperliquid_chain(self) -> &'static str {
        "Testnet"
    }

    /// The badge every surface shows.
    pub fn label(self) -> &'static str {
        "TESTNET"
    }
}

/// Any stored value reads as testnet, including `"mainnet"` or garbage: this build has no
/// other network. Failing the whole preferences file instead would reset unrelated settings.
impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if v != serde_json::Value::String("Testnet".into()) {
            tracing::warn!("Hyperliquid network {v} is not available in this build; using testnet");
        }
        Ok(Network::Testnet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_the_testnet_hosts() {
        assert_eq!(API_URL, "https://api.hyperliquid-testnet.xyz");
        assert_eq!(WS_URL, "wss://api.hyperliquid-testnet.xyz/ws");
        let n = Network::default();
        assert_eq!(n.info_url(), "https://api.hyperliquid-testnet.xyz/info");
        assert_eq!(n.exchange_url(), "https://api.hyperliquid-testnet.xyz/exchange");
        assert_eq!(n.ws_url(), WS_URL);
        for url in [n.info_url(), n.exchange_url(), n.ws_url().to_string()] {
            assert!(url.contains("hyperliquid-testnet.xyz"), "{url}");
        }
    }

    #[test]
    fn signing_names_testnet() {
        assert_eq!(Network::Testnet.phantom_source(), "b");
        assert_eq!(Network::Testnet.hyperliquid_chain(), "Testnet");
    }

    #[test]
    fn no_stored_value_selects_mainnet() {
        for json in [r#""Testnet""#, r#""mainnet""#, r#""Mainnet""#, r#""MAINNET""#, r#""a""#, "1", "null", "{}"] {
            let n: Network = serde_json::from_str(json).unwrap();
            assert_eq!(n, Network::Testnet, "{json}");
        }
        assert_eq!(serde_json::to_string(&Network::Testnet).unwrap(), r#""Testnet""#);
    }
}
