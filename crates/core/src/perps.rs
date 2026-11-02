//! Hyperliquid HIP-3 perpetuals, such as `xyz:AAPL`: their market keys, the venue label every
//! surface shows with them, and the reviewed table that links an instrument the app already
//! has (`AAPL US Equity`) to the perp on the same underlying.
//!
//! A perp is its own instrument. `AAPL US Equity` stays the listed stock, with its own quotes
//! and chart; the table only lets that view point at `xyz:AAPL`. Which perps exist, and which
//! are live, comes from the Terminal server at run time, so a row here never makes a perp
//! appear by itself.
//!
//! A row needs all of these (docs/architecture/server.md §7.3):
//! - the venue's own docs say the perp references the underlying the app key means;
//! - the perp is live (not `isDelisted`) on Hyperliquid;
//! - the `xyz` dex, unless xyz has no market on that underlying and another dex's match is
//!   unambiguous;
//! - its price tracks the app instrument's.

/// A perp linked to an app instrument with the same underlying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerpLink {
    /// The perp's market key, as Hyperliquid names the coin.
    pub coin: &'static str,
    /// The app market key with the same underlying.
    pub underlying: &'static str,
    /// Display name.
    pub name: &'static str,
    /// What the perp references, in the venue's words.
    pub reference: &'static str,
}

const fn link(coin: &'static str, underlying: &'static str, name: &'static str, reference: &'static str) -> PerpLink {
    PerpLink { coin, underlying, name, reference }
}

/// Checked on 2026-11-02 against trade.xyz's specification index, Hyperliquid's `meta` (all
/// live) and the app instrument's price (within 0.6%; oil differs by its contract month).
///
/// Left out on purpose: `xyz:XYZ100` and `xyz:JP225` track the Nasdaq-100 and the Nikkei 225
/// closely, but the venue's docs name neither index. `xyz:HO` tracks HO1, but the docs describe
/// the diesel market under another name and never tie it to `HO`. `xyz:KR200` is the KOSPI 200,
/// which the app doesn't have.
pub const LINKS: &[PerpLink] = &[
    link("xyz:AAPL", "AAPL US Equity", "Apple Inc perp", "1 share of Apple Inc. common stock (Nasdaq: AAPL)"),
    link(
        "xyz:MSFT",
        "MSFT US Equity",
        "Microsoft Corp perp",
        "1 share of Microsoft Corporation common stock (Nasdaq: MSFT)",
    ),
    link("xyz:NVDA", "NVDA US Equity", "NVIDIA Corp perp", "1 share of NVIDIA Corporation common stock (Nasdaq: NVDA)"),
    link(
        "xyz:AMZN",
        "AMZN US Equity",
        "Amazon.com Inc perp",
        "1 share of Amazon.com, Inc. common stock (Nasdaq: AMZN)",
    ),
    link(
        "xyz:GOOGL",
        "GOOGL US Equity",
        "Alphabet Inc Class A perp",
        "1 share of Alphabet Inc. Class A common stock (Nasdaq: GOOGL)",
    ),
    link(
        "xyz:META",
        "META US Equity",
        "Meta Platforms Inc perp",
        "1 share of Meta Platforms, Inc. Class A common stock (Nasdaq: META)",
    ),
    link("xyz:TSLA", "TSLA US Equity", "Tesla Inc perp", "1 share of Tesla, Inc. common stock (Nasdaq: TSLA)"),
    link(
        "xyz:LLY",
        "LLY US Equity",
        "Eli Lilly & Co perp",
        "1 share of Eli Lilly and Company common stock (NYSE: LLY)",
    ),
    link("xyz:AVGO", "AVGO US Equity", "Broadcom Inc perp", "1 share of Broadcom Inc. common stock (Nasdaq: AVGO)"),
    link(
        "xyz:AMD",
        "AMD US Equity",
        "Advanced Micro Devices perp",
        "1 share of Advanced Micro Devices, Inc. common stock (Nasdaq: AMD)",
    ),
    link("xyz:NFLX", "NFLX US Equity", "Netflix Inc perp", "1 share of Netflix, Inc. common stock (Nasdaq: NFLX)"),
    link(
        "xyz:COIN",
        "COIN US Equity",
        "Coinbase Global Inc perp",
        "1 share of Coinbase Global, Inc. Class A common stock (Nasdaq: COIN)",
    ),
    link(
        "xyz:MSTR",
        "MSTR US Equity",
        "Strategy Inc perp",
        "1 share of Strategy Inc Class A common stock (Nasdaq: MSTR)",
    ),
    link(
        "xyz:TLT",
        "TLT US Equity",
        "iShares 20+ Year Treasury Bond ETF perp",
        "1 share of the iShares 20+ Year Treasury Bond ETF (Nasdaq: TLT)",
    ),
    link(
        "xyz:SMH",
        "SMH US Equity",
        "VanEck Semiconductor ETF perp",
        "1 share of the VanEck Semiconductor ETF (Nasdaq: SMH)",
    ),
    link(
        "xyz:XLE",
        "XLE US Equity",
        "Energy Select Sector SPDR perp",
        "1 share of the Energy Select Sector SPDR ETF (NYSE Arca: XLE)",
    ),
    link("xyz:SP500", "SPX Index", "S&P 500 perp", "the S&P 500 Index"),
    link(
        "xyz:CL",
        "CL1 Comdty",
        "WTI crude oil perp",
        "1 barrel of WTI Light Sweet Crude Oil, from a futures contract it rolls monthly",
    ),
    link(
        "xyz:BRENTOIL",
        "CO1 Comdty",
        "Brent crude oil perp",
        "1 barrel of Brent crude oil, from a futures contract it rolls monthly",
    ),
    link(
        "xyz:NATGAS",
        "NG1 Comdty",
        "Natural gas perp",
        "1 MMBtu of Henry Hub natural gas, from a futures contract it rolls monthly",
    ),
    link(
        "xyz:COPPER",
        "HG1 Comdty",
        "Copper perp",
        "1 pound of high-grade copper, from a futures contract it rolls monthly",
    ),
    link("xyz:GOLD", "GC1 Comdty", "Gold perp", "the spot price of 1 troy ounce of gold (XAU/USD), not the future"),
    link(
        "xyz:SILVER",
        "SI1 Comdty",
        "Silver perp",
        "the spot price of 1 troy ounce of silver (XAG/USD), not the future",
    ),
    link(
        "xyz:PLATINUM",
        "PL1 Comdty",
        "Platinum perp",
        "the spot price of 1 troy ounce of platinum (XPT/USD), not the future",
    ),
    link("xyz:EUR", "EURUSD Curncy", "EUR/USD perp", "the EUR/USD spot rate, US dollars per euro"),
    link("xyz:GBP", "GBPUSD Curncy", "GBP/USD perp", "the GBP/USD spot rate, US dollars per pound"),
    link("xyz:JPY", "USDJPY Curncy", "USD/JPY perp", "the USD/JPY spot rate, yen per US dollar"),
];

/// The canonical key for a perp written in any case: `XYZ:aapl` is `xyz:AAPL`. `None` unless
/// it looks like `{dex}:{coin}`: a lowercase-able dex name of letters, then letters and digits.
pub fn normalize(input: &str) -> Option<String> {
    let (dex, coin) = input.trim().split_once(':')?;
    let ok = |s: &str, digits: bool| {
        !s.is_empty() && s.len() <= 16 && s.chars().all(|c| c.is_ascii_alphabetic() || (digits && c.is_ascii_digit()))
    };
    (ok(dex, false) && ok(coin, true)).then(|| format!("{}:{}", dex.to_ascii_lowercase(), coin.to_ascii_uppercase()))
}

/// Whether `key` names a HIP-3 perp.
pub fn is_perp(key: &str) -> bool {
    normalize(key).is_some()
}

/// The dex a perp trades on: `xyz` for `xyz:AAPL`.
pub fn venue(key: &str) -> Option<&str> {
    key.split_once(':').map(|(dex, _)| dex).filter(|_| is_perp(key))
}

/// The label every surface shows next to a perp, so it never passes for the listed
/// instrument: `PERP · HL xyz`.
pub fn badge(key: &str) -> String {
    match venue(key) {
        Some(dex) => format!("PERP · HL {}", dex.to_ascii_lowercase()),
        None => "PERP · HL".into(),
    }
}

/// The perp linked to an app market key.
pub fn perp_for(key: &str) -> Option<&'static PerpLink> {
    LINKS.iter().find(|l| l.underlying == key)
}

/// The link for a perp, by its key in any case.
pub fn link_for(key: &str) -> Option<&'static PerpLink> {
    let k = normalize(key)?;
    LINKS.iter().find(|l| l.coin == k)
}

/// The perps among `keys` that match every word of `query` in their key, coin or linked name,
/// best first. `PERP` in the query matches every perp; a yellow key (`GOLD CMDTY`) asks for
/// listed instruments, so it matches none.
pub fn search<'a>(query: &str, keys: impl IntoIterator<Item = &'a str>, limit: usize) -> Vec<&'a str> {
    let words: Vec<String> = query.split_whitespace().map(str::to_ascii_uppercase).collect();
    if words.is_empty() || words.iter().any(|w| crate::YellowKey::parse(w).is_some()) {
        return Vec::new();
    }
    let words: Vec<&String> = words.iter().filter(|w| !matches!(w.as_str(), "PERP" | "PERPS")).collect();
    let mut scored: Vec<(i32, &'a str)> = keys
        .into_iter()
        .filter_map(|key| {
            let up = key.to_ascii_uppercase();
            let coin = up.split_once(':').map_or(up.as_str(), |(_, c)| c);
            let name = link_for(key).map(|l| l.name.to_ascii_uppercase()).unwrap_or_default();
            let mut score = 1;
            for w in &words {
                score += if coin == w.as_str() {
                    1000
                } else if coin.starts_with(w.as_str()) {
                    600 - 10 * (coin.len() - w.len()) as i32
                } else if up.starts_with(w.as_str()) {
                    500
                } else if name.split(|c: char| c.is_whitespace() || c == '/').any(|n| n.starts_with(w.as_str())) {
                    300
                } else {
                    return None;
                };
            }
            Some((score, key))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, k)| k).collect()
}

/// Where `PERP` takes a pane on `key`: from a linked perp to its instrument, or from an
/// instrument to its perp while `live` says the server lists that perp.
pub fn switch_target(key: &str, live: impl Fn(&str) -> bool) -> Option<&'static str> {
    match link_for(key) {
        Some(l) => Some(l.underlying),
        None => perp_for(key).map(|l| l.coin).filter(|c| live(c)),
    }
}

/// The other side of a link: an instrument's perp, or a perp's instrument.
pub fn counterpart(key: &str) -> Option<&'static str> {
    match link_for(key) {
        Some(l) => Some(l.underlying),
        None => perp_for(key).map(|l| l.coin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Security, catalog};

    #[test]
    fn keys_normalise_and_label_their_venue() {
        assert_eq!(normalize("XYZ:aapl").as_deref(), Some("xyz:AAPL"));
        assert_eq!(normalize(" xyz:XYZ100 ").as_deref(), Some("xyz:XYZ100"));
        assert_eq!(normalize("para:10Y").as_deref(), Some("para:10Y"));
        for not in ["AAPL", "xyz:", ":AAPL", "x1:AAPL", "xyz:AA-PL", "xyz:AAPL:B", "AAPL US Equity"] {
            assert_eq!(normalize(not), None, "{not}");
        }
        assert_eq!(venue("xyz:AAPL"), Some("xyz"));
        assert_eq!(venue("BTC"), None);
        assert_eq!(badge("xyz:AAPL"), "PERP · HL xyz");
        assert_eq!(badge("para:10Y"), "PERP · HL para");
    }

    #[test]
    fn links_point_at_app_instruments_and_back() {
        assert_eq!(perp_for("AAPL US Equity").map(|l| l.coin), Some("xyz:AAPL"));
        assert_eq!(perp_for("SPX Index").map(|l| l.coin), Some("xyz:SP500"));
        assert_eq!(perp_for("USDJPY Curncy").map(|l| l.coin), Some("xyz:JPY"));
        assert_eq!(perp_for("CL1 Comdty").map(|l| l.coin), Some("xyz:CL"));
        assert_eq!(link_for("XYZ:eur").map(|l| l.underlying), Some("EURUSD Curncy"));
        // Not linked: the docs don't name these indices, or the app lacks the instrument.
        for unlinked in ["NDX Index", "NKY Index", "HO1 Comdty", "KOSPI Index", "JPM US Equity", "BTCUSD Curncy"] {
            assert_eq!(perp_for(unlinked), None, "{unlinked}");
        }
        assert_eq!(link_for("xyz:XYZ100"), None);
        assert_eq!(counterpart("AAPL US Equity"), Some("xyz:AAPL"));
        assert_eq!(counterpart("xyz:aapl"), Some("AAPL US Equity"));
        assert_eq!(counterpart("xyz:SKHX"), None);
    }

    #[test]
    fn switching_follows_links_to_live_perps_only() {
        let live = |c: &str| c == "xyz:AAPL";
        assert_eq!(switch_target("AAPL US Equity", live), Some("xyz:AAPL"));
        assert_eq!(switch_target("GC1 Comdty", live), None, "xyz:GOLD isn't listed live here");
        assert_eq!(switch_target("xyz:GOLD", live), Some("GC1 Comdty"), "back to the instrument always works");
        assert_eq!(switch_target("XYZ:aapl", |_| false), Some("AAPL US Equity"));
        assert_eq!(switch_target("xyz:SKHX", |_| true), None, "unlinked");
        assert_eq!(switch_target("NDX Index", |_| true), None, "no link");
    }

    #[test]
    fn search_finds_perps_by_coin_key_or_name() {
        let live = ["xyz:AAPL", "xyz:GOLD", "xyz:SKHX", "xyz:SP500", "para:10Y", "xyz:AMD", "xyz:AMZN"];
        let s = |q: &str| search(q, live, 10);
        assert_eq!(s("aapl"), ["xyz:AAPL"]);
        assert_eq!(s("xyz:aa"), ["xyz:AAPL"]);
        assert_eq!(s("am")[..2], ["xyz:AMD", "xyz:AMZN"], "coin prefixes, shorter first");
        assert_eq!(s("apple"), ["xyz:AAPL"], "a linked perp's name");
        assert_eq!(s("gold perp"), ["xyz:GOLD"]);
        assert_eq!(s("s&p"), ["xyz:SP500"]);
        assert_eq!(s("skhx"), ["xyz:SKHX"], "unlinked perps by coin");
        assert_eq!(s("10y"), ["para:10Y"]);
        assert_eq!(s("perp").len(), live.len());
        assert!(s("gold cmdty").is_empty(), "a yellow key asks for the listed instrument");
        assert!(s("").is_empty() && s("zzzz").is_empty());
        assert_eq!(search("a", live, 2).len(), 2);
    }

    /// Every row names a real app instrument, by its canonical key, on one side, and a
    /// well-formed xyz perp on the other, each at most once.
    #[test]
    fn the_table_is_well_formed() {
        let mut coins: Vec<&str> = LINKS.iter().map(|l| l.coin).collect();
        let mut keys: Vec<&str> = LINKS.iter().map(|l| l.underlying).collect();
        coins.sort();
        coins.dedup();
        keys.sort();
        keys.dedup();
        assert_eq!((coins.len(), keys.len()), (LINKS.len(), LINKS.len()), "one row per perp and per instrument");
        for l in LINKS {
            assert_eq!(normalize(l.coin).as_deref(), Some(l.coin), "{} is canonical", l.coin);
            assert_eq!(venue(l.coin), Some("xyz"), "{} prefers xyz", l.coin);
            let sec = Security::parse(l.underlying).expect("parses");
            assert_eq!(sec.key(), l.underlying, "{} is a canonical key", l.underlying);
            assert!(catalog::by_security(&sec).is_some(), "{} is in the catalog", l.underlying);
            assert!(l.name.ends_with(" perp") && !l.reference.is_empty());
        }
    }
}
