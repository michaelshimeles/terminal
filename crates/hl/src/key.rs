//! Private keys that sign: the agent (API wallet) key Terminal generates and keeps, and a
//! main-wallet key that signs one `approveAgent` and is dropped.
//!
//! Neither type is `Clone`, `Serialize` or printable: `Debug` and `Display` show only the
//! masked address. The secret scalar lives in a k256 `SigningKey`, which zeroizes itself on
//! drop, and the only way out of an [`AgentKey`] is [`AgentKey::to_hex`] for the OS secret
//! store, which hands back a buffer that is zeroized when dropped. [`MainKey`] has no way
//! out at all.

use std::fmt;

use alloy_primitives::{Address, B256};
use anyhow::{Result, bail};
use k256::ecdsa::SigningKey as EcdsaKey;
use zeroize::Zeroizing;

mod sealed {
    pub trait Sealed {
        fn signer(&self) -> &k256::ecdsa::SigningKey;
    }
}

/// A key this crate can sign with. Sealed: only [`AgentKey`] and [`MainKey`].
pub trait SigningKey: sealed::Sealed {
    fn address(&self) -> Address {
        Address::from_private_key(self.signer())
    }
}

/// Sign a 32-byte hash (low-s, with the recovery parity), as Ethereum wallets do.
pub(crate) fn sign_hash(key: &impl SigningKey, hash: &B256) -> anyhow::Result<alloy_primitives::Signature> {
    let (sig, recid) = key.signer().sign_prehash_recoverable(hash.as_slice())?;
    let (r, s) = sig.split_bytes();
    Ok(alloy_primitives::Signature::new(
        alloy_primitives::U256::from_be_slice(&r),
        alloy_primitives::U256::from_be_slice(&s),
        recid.is_y_odd(),
    ))
}

fn parse(hex: &str) -> Result<EcdsaKey> {
    let s = Zeroizing::new(hex.trim().trim_start_matches("0x").to_string());
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        // Never echo the input: it is a secret, or a typo of one.
        bail!("a private key is 64 hex digits");
    }
    let mut bytes = Zeroizing::new([0u8; 32]);
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?;
    }
    EcdsaKey::from_slice(bytes.as_slice()).map_err(|_| anyhow::anyhow!("not a valid secp256k1 private key"))
}

/// The API wallet key that signs orders. Generated on this device; approved once by the
/// main wallet; stored only in the OS secret store (or browser memory).
pub struct AgentKey(EcdsaKey);

impl AgentKey {
    /// A fresh random key. Never reuse an agent address: rotation always generates one.
    pub fn generate() -> Self {
        Self(EcdsaKey::random(&mut rand_core::OsRng))
    }

    /// Load a key the secret store kept (`0x`-prefixed or bare hex).
    pub fn from_hex(hex: &str) -> Result<Self> {
        parse(hex).map(Self)
    }

    /// The hex form for the OS secret store. The buffer is wiped when dropped; never log it.
    pub fn to_hex(&self) -> Zeroizing<String> {
        let bytes = Zeroizing::new(<[u8; 32]>::from(self.0.to_bytes()));
        let mut s = Zeroizing::new(String::with_capacity(66));
        s.push_str("0x");
        for b in bytes.iter() {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl sealed::Sealed for AgentKey {
    fn signer(&self) -> &EcdsaKey {
        &self.0
    }
}
impl SigningKey for AgentKey {}

/// A main-wallet key, held only while it signs `approveAgent` (testnet throwaway keys on
/// the desktop). It cannot be exported or stored.
pub struct MainKey(EcdsaKey);

impl MainKey {
    pub fn from_hex(hex: &str) -> Result<Self> {
        parse(hex).map(Self)
    }

    /// A fresh random key (tests and the testnet check only).
    pub fn generate() -> Self {
        Self(EcdsaKey::random(&mut rand_core::OsRng))
    }
}

impl sealed::Sealed for MainKey {
    fn signer(&self) -> &EcdsaKey {
        &self.0
    }
}
impl SigningKey for MainKey {}

/// `0x1234…abcd`: enough to recognise an address without printing it in full.
pub fn mask_address(a: &Address) -> String {
    let s = format!("{a:#x}");
    format!("{}…{}", &s[..6], &s[s.len() - 4..])
}

/// `text` with every `0x` + 40 hex digits masked (`0x1234…abcd`), for anything shown or
/// logged: Hyperliquid's errors name full addresses.
pub fn mask_addresses(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"0x")
            && b.len() >= i + 42
            && b[i + 2..i + 42].iter().all(u8::is_ascii_hexdigit)
            && b.get(i + 42).is_none_or(|c| !c.is_ascii_hexdigit())
            && let Ok(a) = parse_address(&text[i..i + 42])
        {
            out.push_str(&mask_address(&a));
            i += 42;
            continue;
        }
        let ch = text[i..].chars().next().expect("on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Lower-case `0x…` hex, as Hyperliquid wants addresses.
pub fn address_hex(a: &Address) -> String {
    format!("{a:#x}")
}

/// Parse a `0x…` address in any case.
pub fn parse_address(s: &str) -> Result<Address> {
    let t = s.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("an address is 0x followed by 40 hex digits");
    }
    Ok(hex.to_ascii_lowercase().parse::<Address>()?)
}

macro_rules! redacted {
    ($t:ident) => {
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($t), "({})"), mask_address(&Address::from_private_key(&self.0)))
            }
        }
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(self, f)
            }
        }
    };
}
redacted!(AgentKey);
redacted!(MainKey);

#[cfg(test)]
mod tests {
    use super::*;

    /// The Python SDK's public test key (signing_test.py), never funded.
    const SDK_KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

    #[test]
    fn keys_round_trip_through_hex() {
        let k = AgentKey::generate();
        let back = AgentKey::from_hex(&k.to_hex()).unwrap();
        assert_eq!(back.address(), k.address());
        let bare = AgentKey::from_hex(SDK_KEY.trim_start_matches("0x")).unwrap();
        assert_eq!(bare.address(), AgentKey::from_hex(SDK_KEY).unwrap().address());
        assert_ne!(AgentKey::generate().address(), AgentKey::generate().address());
    }

    #[test]
    fn debug_and_display_never_show_the_secret() {
        let k = AgentKey::from_hex(SDK_KEY).unwrap();
        let m = MainKey::from_hex(SDK_KEY).unwrap();
        for s in [format!("{k:?}"), format!("{k}"), format!("{m:?}"), format!("{m}")] {
            assert!(!s.contains("0123456789012345678901234567890123456789"), "{s}");
            assert!(s.contains('…'), "{s}");
        }
        let addr = k.address();
        assert!(format!("{k:?}").contains(&mask_address(&addr)));
    }

    #[test]
    fn bad_keys_are_refused_without_echoing_them() {
        for bad in ["", "0x12", "zz23456789012345678901234567890123456789012345678901234567890123"] {
            assert_eq!(AgentKey::from_hex(bad).unwrap_err().to_string(), "a private key is 64 hex digits");
        }
        let zero = AgentKey::from_hex(&format!("0x{}", "0".repeat(64))).unwrap_err().to_string();
        assert_eq!(zero, "not a valid secp256k1 private key");
    }

    #[test]
    fn errors_are_masked_for_display() {
        let e = "User or API Wallet 0x5E9EE1089755C3435139848E47E6635505D5A13A does not exist. Must deposit… User: 0x5e9ee1089755c3435139848e47e6635505d5a13a";
        assert_eq!(mask_addresses(e), "User or API Wallet 0x5e9e…a13a does not exist. Must deposit… User: 0x5e9e…a13a");
        let hash = format!("0x{}", "ab".repeat(32));
        assert_eq!(mask_addresses(&hash), hash, "64-digit hashes are not addresses");
        assert_eq!(
            mask_addresses("cloid 0x0000000000000000000000000000002a ×3"),
            "cloid 0x0000000000000000000000000000002a ×3"
        );
    }

    #[test]
    fn addresses_parse_and_mask() {
        let a = parse_address("0x5E9EE1089755C3435139848E47E6635505D5A13A").unwrap();
        assert_eq!(address_hex(&a), "0x5e9ee1089755c3435139848e47e6635505d5a13a");
        assert_eq!(mask_address(&a), "0x5e9e…a13a");
        assert!(parse_address("0x123").is_err() && parse_address("hello").is_err());
    }
}
