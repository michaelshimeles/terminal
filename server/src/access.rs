//! Who may use what: the optional token in front of `/stats` and `/metrics`, and the cap on
//! connections from one client address.
//!
//! Client addresses only ever live in memory as map keys. They are never logged or reported.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderName, header};

/// A secret from the environment. Its `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Arc<str>);

impl Secret {
    pub fn new(s: &str) -> Self {
        Self(s.into())
    }

    /// Whether `given` is the secret, in time that doesn't depend on where they differ.
    pub fn matches(&self, given: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), given.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Whether a request may read `/stats` and `/metrics`: always without a token, and with one
/// only when it sends `Authorization: Bearer <token>`. The scheme's case doesn't matter (RFC
/// 9110 §11.1); the token's does.
pub fn operator(token: Option<&Secret>, headers: &HeaderMap) -> bool {
    let Some(token) = token else { return true };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .is_some_and(|(_, given)| token.matches(given.trim()))
}

/// The client's address: the last entry of `header` when it is set and parses (the entry the
/// edge in front of the gateway added), otherwise the peer the connection came from.
pub fn client_ip(peer: SocketAddr, header: Option<&HeaderName>, headers: &HeaderMap) -> IpAddr {
    header
        .and_then(|h| headers.get_all(h).iter().next_back())
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer.ip())
        .to_canonical()
}

/// One IPv6 host usually has a whole /64, so connections count against their /64.
fn bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Connections open per client address, capped at `max` (0 turns the cap off).
#[derive(Clone, Default)]
pub struct PerIp {
    max: usize,
    open: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

impl PerIp {
    pub fn new(max: usize) -> Self {
        Self { max, open: Arc::default() }
    }

    /// Count a connection from `ip` until the slot drops, or `None` when `ip` is at the cap.
    pub fn admit(&self, ip: IpAddr) -> Option<IpSlot> {
        let key = bucket(ip);
        if self.max > 0 {
            let mut open = self.open.lock().expect("per-ip lock");
            let n = open.entry(key).or_insert(0);
            if *n >= self.max {
                return None;
            }
            *n += 1;
        }
        Some(IpSlot { key, owner: self.clone() })
    }

    /// Addresses with a connection open now.
    pub fn addresses(&self) -> usize {
        self.open.lock().expect("per-ip lock").len()
    }
}

/// One admitted connection.
pub struct IpSlot {
    key: IpAddr,
    owner: PerIp,
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        if self.owner.max == 0 {
            return;
        }
        let mut open = self.owner.open.lock().expect("per-ip lock");
        if let Some(n) = open.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[test]
    fn the_token_must_match_exactly() {
        let t = Secret::new("s3cret");
        assert!(operator(None, &headers(&[])), "no token configured: open");
        assert!(!operator(Some(&t), &headers(&[])));
        assert!(!operator(Some(&t), &headers(&[("authorization", "Bearer s3cre")])));
        assert!(!operator(Some(&t), &headers(&[("authorization", "Bearer s3cret2")])));
        assert!(!operator(Some(&t), &headers(&[("authorization", "Basic s3cret")])));
        assert!(!operator(Some(&t), &headers(&[("authorization", "s3cret")])));
        assert!(operator(Some(&t), &headers(&[("authorization", "Bearer s3cret")])));
        assert!(operator(Some(&t), &headers(&[("authorization", "bearer s3cret")])), "the scheme ignores case");
        assert!(operator(Some(&t), &headers(&[("authorization", "BEARER  s3cret")])));
        assert!(!operator(Some(&t), &headers(&[("authorization", "Bearer S3CRET")])), "the token doesn't");
        assert!(!operator(Some(&t), &headers(&[("authorization", "Bearers3cret")])));
        assert_eq!(format!("{t:?}"), "Secret(..)", "Debug never shows the value");
    }

    #[test]
    fn the_client_address_comes_from_the_trusted_header_when_set() {
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let xff = HeaderName::from_static("x-forwarded-for");
        let h = headers(&[("x-forwarded-for", "203.0.113.9, 198.51.100.7")]);
        assert_eq!(client_ip(peer, None, &h), peer.ip(), "no header configured: the peer");
        assert_eq!(client_ip(peer, Some(&xff), &h).to_string(), "198.51.100.7", "the entry the edge added");
        let two = headers(&[("x-forwarded-for", "203.0.113.9"), ("x-forwarded-for", "198.51.100.8")]);
        assert_eq!(client_ip(peer, Some(&xff), &two).to_string(), "198.51.100.8");
        assert_eq!(client_ip(peer, Some(&xff), &headers(&[("x-forwarded-for", "junk")])), peer.ip());
        assert_eq!(client_ip(peer, Some(&xff), &headers(&[])), peer.ip());
        let mapped = headers(&[("x-forwarded-for", "::ffff:198.51.100.7")]);
        assert_eq!(client_ip(peer, Some(&xff), &mapped).to_string(), "198.51.100.7");
    }

    #[test]
    fn connections_per_address_are_capped_and_released() {
        let p = PerIp::new(2);
        let a: IpAddr = "198.51.100.7".parse().unwrap();
        let b: IpAddr = "198.51.100.8".parse().unwrap();
        let s1 = p.admit(a).unwrap();
        let _s2 = p.admit(a).unwrap();
        assert!(p.admit(a).is_none(), "a third from one address is refused");
        let _other = p.admit(b).expect("another address has its own allowance");
        drop(s1);
        let _s3 = p.admit(a).expect("a closed connection frees its slot");
        assert_eq!(p.addresses(), 2);
        drop((_s2, _s3, _other));
        assert_eq!(p.addresses(), 0, "nothing is kept for addresses that left");
    }

    #[test]
    fn ipv6_counts_per_64() {
        let p = PerIp::new(1);
        let _s = p.admit("2001:db8:1:2::1".parse().unwrap()).unwrap();
        assert!(p.admit("2001:db8:1:2:ffff::9".parse().unwrap()).is_none(), "same /64");
        assert!(p.admit("2001:db8:1:3::1".parse().unwrap()).is_some(), "another /64");
    }

    #[test]
    fn zero_turns_the_cap_off() {
        let p = PerIp::new(0);
        let a: IpAddr = "198.51.100.7".parse().unwrap();
        let slots: Vec<_> = (0..100).map(|_| p.admit(a).unwrap()).collect();
        assert_eq!((slots.len(), p.addresses()), (100, 0));
    }
}
