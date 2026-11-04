//! Built-in wallets on [Turnkey](https://turnkey.com), for Hyperliquid **testnet** trading.
//!
//! Each user has a Turnkey sub-organization with one Ethereum wallet, created when they first
//! sign in on Terminal's sign-in page. That wallet is their Hyperliquid account. Turnkey
//! signs for it only rarely: approving (or replacing) the device's trading key. Orders are
//! signed on the device by that trading key, never by Turnkey.
//!
//! - [`session`]: the device's P-256 key, which stamps every Turnkey request, and the
//!   short session Turnkey issues for it.
//! - [`client`]: `whoami`, the wallet's Ethereum account, and signing one digest.
//! - [`callback`] (desktop): the loopback listener the browser returns the session to.
//! - `mock` (feature `mock`): a local stand-in for tests and screenshots.
//!
//! This crate knows nothing about Hyperliquid. What may be signed is decided by `tr-hl`
//! (`signing::sign_approve_agent_with`), which hands it only the digest of a testnet
//! `approveAgent`.

pub mod client;
pub mod session;

#[cfg(not(target_arch = "wasm32"))]
pub mod callback;
#[cfg(all(feature = "mock", not(target_arch = "wasm32")))]
pub mod mock;

pub use client::Client;
pub use session::{Session, SessionKey};

/// Terminal's sign-in page.
pub const SIGN_IN_PAGE: &str = "https://terminal.rasmic.xyz/wallet/";

/// The sign-in page, unless `TR_WALLET_SIGN_IN_URL` names a local mock (desktop only).
pub fn sign_in_page() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(u) = std::env::var("TR_WALLET_SIGN_IN_URL").ok().and_then(|u| client::loopback_only(&u)) {
        tracing::warn!("turnkey: using a local mock sign-in page at {u}");
        return format!("{u}/");
    }
    SIGN_IN_PAGE.to_string()
}

/// The page that signs a desktop in: it returns the session for `public_key` to
/// `http://127.0.0.1:<port>/callback` with `state`. Only public values are in the URL.
pub fn desktop_sign_in_url(page: &str, public_key: &str, state: &str, port: u16) -> String {
    format!("{page}?flow=desktop&pk={public_key}&state={state}&port={port}")
}

/// The page in export mode: the user signs in again there, and Turnkey's export iframe shows
/// the recovery phrase. Terminal is not involved.
pub fn export_url(page: &str) -> String {
    format!("{page}?mode=export")
}
