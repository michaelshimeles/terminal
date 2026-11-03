//! The user's own data over Hyperliquid's websocket, straight from the client (never through
//! Terminal's server): what to subscribe to, and how each message changes [`UserState`].
//!
//! `clearinghouseState` and `openOrders` arrive whole for one dex each time, so they replace
//! that dex's entry. `userFills` and `userFundings` start with a snapshot (`isSnapshot`) and
//! then stream additions, de-duplicated by trade id so a reconnect's snapshot never doubles
//! a fill. Messages for any other user are ignored.
//!
//! The socket itself lives with the caller (the app's worker uses `tr_data::ws`), which
//! sends [`subscriptions`], [`PING`] about every 30 s, and feeds every text frame to
//! [`UserState::apply`].

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use crate::model::*;

/// Keeps the connection open (Hyperliquid closes it after 60 s without traffic).
pub const PING: &str = r#"{"method":"ping"}"#;
/// Fills kept for `ACTV`.
pub const MAX_FILLS: usize = 500;

/// The subscribe messages for `user` on `dexes` (`""` is the core dex), with leverage for
/// `coins` (the ticket's market) and testnet mids per dex.
pub fn subscriptions<'a>(user: &str, dexes: impl IntoIterator<Item = &'a str>, coins: &[&str]) -> Vec<String> {
    let user = user.to_ascii_lowercase();
    let mut subs = Vec::new();
    for dex in dexes {
        subs.push(json!({"type": "clearinghouseState", "user": user, "dex": dex}));
        subs.push(json!({"type": "openOrders", "user": user, "dex": dex}));
        subs.push(json!({"type": "allMids", "dex": dex}));
    }
    for t in ["orderUpdates", "userFills", "userFundings"] {
        subs.push(json!({"type": t, "user": user}));
    }
    for coin in coins {
        subs.push(json!({"type": "activeAssetData", "user": user, "coin": coin}));
    }
    subs.into_iter().map(|s| json!({"method": "subscribe", "subscription": s}).to_string()).collect()
}

/// One subscription for leverage on another market (the ticket moved on).
pub fn asset_data_subscription(user: &str, coin: &str, subscribe: bool) -> String {
    let method = if subscribe { "subscribe" } else { "unsubscribe" };
    json!({"method": method, "subscription": {"type": "activeAssetData", "user": user.to_ascii_lowercase(), "coin": coin}})
        .to_string()
}

/// What a message changed, for toasts and repaints.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Account,
    Orders,
    Fills(Vec<Fill>),
    Fundings,
    AssetData(String),
    Mids,
    OrderUpdates(Vec<OrderUpdate>),
}

/// Everything the panes and the ticket know about one account.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserState {
    pub clearinghouse: BTreeMap<String, Clearinghouse>,
    pub orders: BTreeMap<String, Vec<OpenOrder>>,
    /// Newest first.
    pub fills: Vec<Fill>,
    pub fundings: Vec<Funding>,
    pub asset_data: HashMap<String, AssetData>,
    /// Testnet mids by coin (`BTC`, `xyz:AAPL`). Never mixed with mainnet quotes.
    pub mids: HashMap<String, f64>,
}

impl UserState {
    /// Apply one websocket text frame for `user`. Returns what changed (nothing for acks,
    /// pongs and other users' data).
    pub fn apply(&mut self, user: &str, frame: &str) -> Vec<Event> {
        let Ok(m) = serde_json::from_str::<Value>(frame) else { return Vec::new() };
        let data = &m["data"];
        let mine = |d: &Value| d["user"].as_str().is_some_and(|u| u.eq_ignore_ascii_case(user));
        match m["channel"].as_str() {
            Some("clearinghouseState") if mine(data) => {
                let dex = data["dex"].as_str().unwrap_or_default().to_string();
                self.clearinghouse.insert(dex.clone(), parse_clearinghouse(&dex, &data["clearinghouseState"]));
                vec![Event::Account]
            }
            Some("openOrders") if mine(data) => {
                let dex = data["dex"].as_str().unwrap_or_default().to_string();
                self.orders.insert(dex, parse_open_orders(&data["orders"]));
                vec![Event::Orders]
            }
            Some("userFills") if mine(data) => {
                let fills = parse_fills(&data["fills"]);
                let snapshot = data["isSnapshot"].as_bool().unwrap_or(false);
                let new = self.add_fills(fills, snapshot);
                if snapshot || !new.is_empty() {
                    vec![Event::Fills(if snapshot { Vec::new() } else { new })]
                } else {
                    Vec::new()
                }
            }
            Some("userFundings") if mine(data) => {
                let f = parse_fundings(&data["fundings"]);
                if data["isSnapshot"].as_bool().unwrap_or(false) {
                    self.fundings = f;
                } else {
                    self.fundings.extend(f);
                }
                self.fundings.sort_by(|a, b| b.time.cmp(&a.time).then(a.coin.cmp(&b.coin)));
                self.fundings.dedup_by(|a, b| a.time == b.time && a.coin == b.coin);
                vec![Event::Fundings]
            }
            Some("activeAssetData") if mine(data) => match parse_asset_data(data) {
                Some(a) => {
                    let coin = a.coin.clone();
                    self.asset_data.insert(coin.clone(), a);
                    vec![Event::AssetData(coin)]
                }
                None => Vec::new(),
            },
            Some("allMids") => {
                let mut any = false;
                for (k, v) in data["mids"].as_object().into_iter().flatten() {
                    if let Some(px) = num(v).filter(|p| *p > 0.0) {
                        self.mids.insert(k.clone(), px);
                        any = true;
                    }
                }
                if any { vec![Event::Mids] } else { Vec::new() }
            }
            Some("orderUpdates") => {
                // Order updates carry no user field; the subscription is for this user only.
                let u = parse_order_updates(data);
                if u.is_empty() { Vec::new() } else { vec![Event::OrderUpdates(u)] }
            }
            _ => Vec::new(),
        }
    }

    /// Merge fills (a snapshot replaces), newest first, de-duplicated by trade id. Returns
    /// the ones that are new.
    fn add_fills(&mut self, fills: Vec<Fill>, snapshot: bool) -> Vec<Fill> {
        if snapshot {
            self.fills.clear();
        }
        let mut new = Vec::new();
        for f in fills {
            if !self.fills.iter().any(|x| x.tid == f.tid && x.coin == f.coin && x.time == f.time) {
                new.push(f.clone());
                self.fills.push(f);
            }
        }
        self.fills.sort_by(|a, b| b.time.cmp(&a.time).then(b.tid.cmp(&a.tid)));
        self.fills.truncate(MAX_FILLS);
        new
    }

    /// Every open order on every dex, as `(coin, oid)`: what the kill switch cancels.
    pub fn open_order_ids(&self) -> Vec<(String, u64)> {
        self.orders.values().flatten().map(|o| (o.coin.clone(), o.oid)).collect()
    }

    /// Account value over every dex loaded (standard accounts hold one balance per dex).
    pub fn account_value(&self) -> f64 {
        self.clearinghouse.values().map(|c| c.margin.account_value).sum()
    }

    pub fn withdrawable(&self) -> f64 {
        self.clearinghouse.values().map(|c| c.withdrawable).sum()
    }

    pub fn positions(&self) -> impl Iterator<Item = &PerpPosition> {
        self.clearinghouse.values().flat_map(|c| c.positions.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "0x00000000000000000000000000000000000f107e";

    fn recorded() -> Vec<String> {
        let path = format!("{}/tests/fixtures/testnet_ws_user_stream.jsonl", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
    }

    #[test]
    fn subscriptions_cover_both_dexes() {
        let subs = subscriptions("0xABC", ["", "xyz"], &["xyz:AAPL"]);
        assert_eq!(subs.len(), 3 * 2 + 3 + 1);
        assert!(subs.iter().any(|s| s.contains(r#""type":"clearinghouseState""#) && s.contains(r#""dex":"xyz""#)));
        assert!(subs.iter().all(|s| !s.contains("0xABC")), "addresses go lower-case");
        assert!(asset_data_subscription("0xabc", "BTC", false).starts_with(r#"{"method":"unsubscribe""#));
    }

    #[test]
    fn a_recorded_testnet_stream_builds_the_account() {
        let mut s = UserState::default();
        let mut events = Vec::new();
        for m in recorded() {
            events.extend(s.apply(USER, &m));
        }
        assert_eq!(s.clearinghouse.len(), 2, "core and xyz");
        assert_eq!(s.positions().count(), 3);
        assert_eq!(s.open_order_ids(), [("xyz:AAPL".to_string(), 63919996139)]);
        assert_eq!(s.fills.len(), 4);
        assert!(s.fills.windows(2).all(|w| w[0].time >= w[1].time), "newest first");
        assert_eq!(s.asset_data["xyz:AAPL"].leverage.map(|l| l.value), Some(20));
        assert!(s.mids.values().all(|p| *p > 0.0) && !s.mids.is_empty());
        assert!(s.account_value() > 0.0);
        assert!(events.contains(&Event::Account) && events.contains(&Event::Orders));
        // The same data for another user changes nothing.
        let mut other = UserState::default();
        for m in recorded().iter().filter(|m| !m.contains("allMids")) {
            other.apply("0x0000000000000000000000000000000000000001", m);
        }
        assert_eq!(other, UserState::default());
    }

    #[test]
    fn a_reconnect_snapshot_never_doubles_fills() {
        let mut s = UserState::default();
        let fill =
            |tid: u64, time: u64| json!({"coin": "BTC", "px": "1", "sz": "1", "side": "B", "time": time, "tid": tid});
        let msg = |snap: bool, fills: Vec<Value>| {
            json!({"channel": "userFills", "data": {"isSnapshot": snap, "user": USER, "fills": fills}}).to_string()
        };
        s.apply(USER, &msg(true, vec![fill(1, 10), fill(2, 20)]));
        let ev = s.apply(USER, &msg(false, vec![fill(3, 30), fill(2, 20)]));
        assert_eq!(ev.len(), 1);
        let Event::Fills(new) = &ev[0] else { panic!("{ev:?}") };
        assert_eq!(new.iter().map(|f| f.tid).collect::<Vec<_>>(), [3]);
        s.apply(USER, &msg(true, vec![fill(1, 10), fill(2, 20), fill(3, 30)]));
        assert_eq!(s.fills.iter().map(|f| f.tid).collect::<Vec<_>>(), [3, 2, 1]);
        assert!(s.apply(USER, "not json").is_empty() && s.apply(USER, r#"{"channel":"pong"}"#).is_empty());
    }
}
