//! Order entry on Hyperliquid, **testnet only**.
//!
//! - [`network`]: the testnet endpoints, and the only [`Network`] there is.
//! - [`key`]: the agent (API wallet) key, and a main-wallet key for tests and the
//!   developer-only `testnet_check` example, both redacted. The app's main wallet is the
//!   built-in Turnkey wallet, which signs through [`signing::sign_approve_agent_with`].
//! - [`signing`]: L1 phantom-agent and user-signed EIP-712 signatures.
//! - [`actions`]: order, cancel, cancelByCloid, modify, updateLeverage, scheduleCancel, noop
//!   and approveAgent, with the field order Hyperliquid hashes.
//!
//! Mainnet is not reachable from this crate: there is no mainnet URL, the phantom agent's
//! source is always `"b"`, `hyperliquidChain` is always `"Testnet"`, and
//! `tests/mainnet_impossible.rs` greps the source for mainnet hosts.

pub mod actions;
pub mod assets;
pub mod caps;
pub mod client;
pub mod key;
pub mod model;
pub mod network;
pub mod nonce;
pub mod rounding;
pub mod signing;
pub mod stream;
pub mod trader;

#[cfg(test)]
mod logs_test;
#[cfg(test)]
mod vectors;

pub use key::{AgentKey, MainKey, SigningKey, mask_address, mask_addresses};
pub use network::Network;
