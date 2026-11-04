//! The device's Turnkey credential: a P-256 key made here, and the session Turnkey issued for
//! its public key.
//!
//! Sign-in happens in the browser (see `docs/architecture/trading-hyperliquid.md` §4.5).
//! The browser never sees this key: it only asks Turnkey for a session bound to the key's
//! **public** half (`stamp_login` with `publicKey`), and hands back the session token. The
//! token says which organization and user the session belongs to and when it ends; it can't
//! authorize anything by itself, because every request must carry a stamp made with the
//! private key ([`SessionKey::stamp`]).
//!
//! The key is redacted in `Debug`, never serialized except through [`StoredSession`] for
//! the OS secret store, and its buffers are wiped on drop.

use std::fmt;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use turnkey_api_key_stamper::{Stamp, TurnkeyP256ApiKey};
use zeroize::Zeroizing;

/// How long the sign-in page asks Turnkey to keep a session: 15 minutes, Turnkey's default.
/// Long enough to approve a trading key; nothing else needs a session.
pub const SESSION_SECONDS: u64 = 900;

/// A P-256 key generated on this device. Turnkey knows only its public half.
pub struct SessionKey(TurnkeyP256ApiKey);

impl SessionKey {
    pub fn generate() -> Self {
        Self(TurnkeyP256ApiKey::generate())
    }

    /// From the hex the secret store kept.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let bytes = Zeroizing::new(hex::decode(hex.trim()).map_err(|_| anyhow::anyhow!("a session key is hex"))?);
        TurnkeyP256ApiKey::from_bytes(bytes.as_slice(), None)
            .map(Self)
            .map_err(|_| anyhow::anyhow!("not a valid P-256 private key"))
    }

    /// The hex form for the secret store. Wiped when dropped; never log it.
    pub fn to_hex(&self) -> Zeroizing<String> {
        let bytes = Zeroizing::new(self.0.private_key());
        Zeroizing::new(hex::encode(bytes.as_slice()))
    }

    /// The compressed public key as Turnkey names it (66 hex digits).
    pub fn public_hex(&self) -> String {
        hex::encode(self.0.compressed_public_key())
    }

    /// The `X-Stamp` header for a request body.
    pub fn stamp(&self, body: &[u8]) -> Result<(String, String)> {
        let h = self.0.stamp(body).map_err(|e| anyhow::anyhow!("stamping the request: {e}"))?;
        Ok((h.name, h.value))
    }
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.public_hex();
        write!(f, "SessionKey({}…{})", &p[..6], &p[p.len() - 4..])
    }
}

/// What a Turnkey session token says (its JWT payload). Turnkey signs the token, but
/// Terminal doesn't rely on that: anything the token claims is checked by Turnkey again on
/// every stamped request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The user's sub-organization.
    pub organization_id: String,
    pub user_id: String,
    /// The compressed P-256 key the session is bound to.
    pub public_key: String,
    /// Expiry, Unix seconds.
    pub exp: u64,
    #[serde(default)]
    pub session_type: String,
}

impl Claims {
    /// Read a session token's payload. The token's signature is not checked here (see the type).
    pub fn parse(token: &str) -> Result<Self> {
        let mut parts = token.trim().split('.');
        let (Some(_), Some(payload), Some(_), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            bail!("the session token is not a JWT");
        };
        let json = BASE64_URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .context("the session token's payload is not base64url")?;
        serde_json::from_slice(&json).context("the session token's payload is missing fields")
    }
}

/// A session this device can use: its key, the token, and what the token says.
pub struct Session {
    pub key: SessionKey,
    pub token: String,
    pub claims: Claims,
}

impl Session {
    /// Accept a token from the sign-in page for `key`. Refuses a token bound to any other
    /// key (another device's, or one a page made up), and one that has already expired.
    pub fn accept(key: SessionKey, token: &str, now_secs: u64) -> Result<Self> {
        let claims = Claims::parse(token)?;
        ensure!(
            claims.public_key.eq_ignore_ascii_case(&key.public_hex()),
            "the sign-in answer is for a different key; nothing was saved"
        );
        ensure!(claims.exp > now_secs, "the session had already expired; sign in again");
        ensure!(!claims.organization_id.is_empty(), "the session names no organization");
        Ok(Self { key, token: token.trim().to_string(), claims })
    }

    /// A long-lived Turnkey API key used as a session (the live integration tests). It has
    /// no token and doesn't expire on this side.
    pub fn for_api_key(key: SessionKey, organization_id: &str) -> Self {
        let claims = Claims {
            organization_id: organization_id.to_string(),
            user_id: String::new(),
            public_key: key.public_hex(),
            exp: u64::MAX,
            session_type: "API_KEY".into(),
        };
        Self { key, token: String::new(), claims }
    }

    pub fn organization_id(&self) -> &str {
        &self.claims.organization_id
    }

    /// Seconds left (0 once expired).
    pub fn remaining(&self, now_secs: u64) -> u64 {
        self.claims.exp.saturating_sub(now_secs)
    }

    pub fn expired(&self, now_secs: u64) -> bool {
        self.remaining(now_secs) == 0
    }

    /// The form the OS secret store keeps (JSON with the key's hex). Wiped when dropped.
    pub fn to_stored(&self) -> Zeroizing<String> {
        let key = self.key.to_hex();
        Zeroizing::new(
            serde_json::to_string(&StoredSession { key: key.as_str(), token: &self.token }).expect("plain strings"),
        )
    }

    /// Back from the secret store; `None` once expired (the caller deletes it).
    pub fn from_stored(text: &str, now_secs: u64) -> Result<Option<Self>> {
        let s: StoredSessionOwned = serde_json::from_str(text).context("the stored session is unreadable")?;
        let hex = Zeroizing::new(s.key);
        let key = SessionKey::from_hex(&hex)?;
        match Self::accept(key, &s.token, now_secs) {
            Ok(session) => Ok(Some(session)),
            Err(_) if Claims::parse(&s.token).is_ok_and(|c| c.exp <= now_secs) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("key", &self.key).field("org", &self.claims.organization_id).finish()
    }
}

#[derive(Serialize)]
struct StoredSession<'a> {
    key: &'a str,
    token: &'a str,
}

#[derive(Deserialize)]
struct StoredSessionOwned {
    key: String,
    token: String,
}

/// 32 random bytes as hex: the sign-in's `state`, which the callback must echo.
pub fn random_state() -> String {
    let mut b = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
    hex::encode(b)
}

/// Equal in constant time (for `state`).
pub fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A session token for tests and the mock: the claims, unsigned.
#[cfg(any(test, feature = "mock"))]
pub fn unsigned_token(c: &Claims) -> String {
    let h = BASE64_URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","typ":"JWT"}"#);
    let p = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(c).expect("claims"));
    format!("{h}.{p}.mock-signature")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(key: &SessionKey, exp: u64) -> Claims {
        Claims {
            organization_id: "b0c1e8d6-0000-4000-8000-000000000001".into(),
            user_id: "u-1".into(),
            public_key: key.public_hex(),
            exp,
            session_type: "SESSION_TYPE_READ_WRITE".into(),
        }
    }

    #[test]
    fn a_session_is_accepted_only_for_this_key_and_before_it_expires() {
        let key = SessionKey::generate();
        let token = unsigned_token(&claims(&key, 2_000));
        let other = SessionKey::generate();
        let theirs = unsigned_token(&claims(&other, 2_000));
        assert!(Session::accept(SessionKey::from_hex(&key.to_hex()).unwrap(), &theirs, 1_000).is_err());
        assert!(Session::accept(SessionKey::from_hex(&key.to_hex()).unwrap(), &token, 2_000).is_err(), "expired");
        let s = Session::accept(key, &token, 1_000).unwrap();
        assert_eq!(s.remaining(1_000), 1_000);
        assert_eq!(s.organization_id(), "b0c1e8d6-0000-4000-8000-000000000001");
    }

    #[test]
    fn the_stored_form_round_trips_and_lapses() {
        let key = SessionKey::generate();
        let public = key.public_hex();
        let token = unsigned_token(&claims(&key, 2_000));
        let s = Session::accept(key, &token, 1_000).unwrap();
        let stored = s.to_stored();
        let back = Session::from_stored(&stored, 1_500).unwrap().unwrap();
        assert_eq!(back.key.public_hex(), public);
        assert_eq!(back.token, token);
        assert!(Session::from_stored(&stored, 2_000).unwrap().is_none(), "expired sessions come back as none");
    }

    #[test]
    fn debug_never_shows_the_private_key() {
        let key = SessionKey::generate();
        let hex = key.to_hex();
        let token = unsigned_token(&claims(&key, 2_000));
        let s = Session::accept(SessionKey::from_hex(&hex).unwrap(), &token, 1_000).unwrap();
        for text in [format!("{key:?}"), format!("{s:?}")] {
            assert!(!text.contains(hex.as_str()), "{text}");
            assert!(!text.contains(&hex[..16]), "{text}");
        }
    }

    #[test]
    fn tokens_must_be_jwts_with_the_fields() {
        for bad in ["", "a.b", "a.b.c.d", "x.!!!.y", "x.e30.y"] {
            assert!(Claims::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn states_are_random_and_compared_in_full() {
        let (a, b) = (random_state(), random_state());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(same_secret(&a, &a.clone()));
        assert!(!same_secret(&a, &b) && !same_secret(&a, &a[..63]));
    }
}
