//! Hyperliquid's public websocket messages: what we send and how we read what comes back.
//! Shapes follow the docs (WS-SUB in docs/architecture/server.md) and recorded live messages
//! in `tests/fixtures/hl`. Prices and sizes arrive as decimal strings (candles too, although
//! the docs type them as numbers), so every number accepts either.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use serde_json::json;
use serde_json::value::RawValue;
use tr_core::{Bar, Timeframe};
use tr_proto::{Level, Side, Trade};

use crate::provider::UpstreamEvent;

/// One upstream subscription, as Hyperliquid identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SubKey {
    /// `dex` is `""` for the core perp dex (whose mids also carry spot).
    AllMids {
        dex: String,
    },
    L2Book {
        coin: String,
    },
    Trades {
        coin: String,
    },
    Candle {
        coin: String,
        interval: &'static str,
    },
}

impl SubKey {
    pub fn subscription(&self) -> serde_json::Value {
        match self {
            SubKey::AllMids { dex } if dex.is_empty() => json!({ "type": "allMids" }),
            SubKey::AllMids { dex } => json!({ "type": "allMids", "dex": dex }),
            SubKey::L2Book { coin } => json!({ "type": "l2Book", "coin": coin }),
            SubKey::Trades { coin } => json!({ "type": "trades", "coin": coin }),
            SubKey::Candle { coin, interval } => json!({ "type": "candle", "coin": coin, "interval": interval }),
        }
    }

    /// The `subscribe` or `unsubscribe` message for this subscription.
    pub fn message(&self, method: &str) -> String {
        json!({ "method": method, "subscription": self.subscription() }).to_string()
    }

    pub fn kind(&self) -> &'static str {
        match self {
            SubKey::AllMids { .. } => "allMids",
            SubKey::L2Book { .. } => "l2Book",
            SubKey::Trades { .. } => "trades",
            SubKey::Candle { .. } => "candle",
        }
    }
}

impl fmt::Display for SubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubKey::AllMids { dex } if dex.is_empty() => f.write_str("allMids"),
            SubKey::AllMids { dex } => write!(f, "allMids:{dex}"),
            SubKey::L2Book { coin } => write!(f, "l2Book:{coin}"),
            SubKey::Trades { coin } => write!(f, "trades:{coin}"),
            SubKey::Candle { coin, interval } => write!(f, "candle:{coin}:{interval}"),
        }
    }
}

pub const PING: &str = r#"{"method":"ping"}"#;

/// Hyperliquid's name for a timeframe.
pub fn interval(tf: Timeframe) -> &'static str {
    match tf {
        Timeframe::M1 => "1m",
        Timeframe::M5 => "5m",
        Timeframe::M15 => "15m",
        Timeframe::M30 => "30m",
        Timeframe::H1 => "1h",
        Timeframe::H4 => "4h",
        Timeframe::D1 => "1d",
        Timeframe::W1 => "1w",
        Timeframe::MN1 => "1M",
    }
}

fn static_interval(s: &str) -> Option<&'static str> {
    Timeframe::ALL.into_iter().map(interval).find(|i| *i == s)
}

/// What one upstream text frame means to us.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Data(SubKey, UpstreamEvent),
    Subscribed(SubKey),
    Unsubscribed(SubKey),
    Pong,
    Error(String),
    /// A channel we don't use, or an empty batch.
    Ignored,
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    channel: Cow<'a, str>,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct AllMids {
    mids: HashMap<String, String>,
    #[serde(default)]
    dex: Option<String>,
}

#[derive(Deserialize)]
struct WsLevel {
    #[serde(deserialize_with = "num")]
    px: f64,
    #[serde(deserialize_with = "num")]
    sz: f64,
    n: u32,
}

#[derive(Deserialize)]
struct WsBook {
    coin: String,
    time: i64,
    levels: (Vec<WsLevel>, Vec<WsLevel>),
}

#[derive(Deserialize)]
struct WsTrade {
    coin: String,
    side: String,
    #[serde(deserialize_with = "num")]
    px: f64,
    #[serde(deserialize_with = "num")]
    sz: f64,
    time: i64,
    tid: u64,
}

#[derive(Deserialize)]
struct WsCandle {
    t: i64,
    s: String,
    i: String,
    #[serde(deserialize_with = "num")]
    o: f64,
    #[serde(deserialize_with = "num")]
    h: f64,
    #[serde(deserialize_with = "num")]
    l: f64,
    #[serde(deserialize_with = "num")]
    c: f64,
    #[serde(deserialize_with = "num")]
    v: f64,
}

#[derive(Deserialize)]
struct SubAck {
    method: String,
    subscription: SubObj,
}

#[derive(Deserialize)]
struct SubObj {
    #[serde(rename = "type")]
    ty: String,
    coin: Option<String>,
    dex: Option<String>,
    interval: Option<String>,
}

impl SubObj {
    fn key(self) -> Option<SubKey> {
        Some(match self.ty.as_str() {
            "allMids" => SubKey::AllMids { dex: self.dex.unwrap_or_default() },
            "l2Book" => SubKey::L2Book { coin: self.coin? },
            "trades" => SubKey::Trades { coin: self.coin? },
            "candle" => SubKey::Candle { coin: self.coin?, interval: static_interval(self.interval.as_deref()?)? },
            _ => return None,
        })
    }
}

/// A price or size that may be a JSON string or number.
fn num<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    struct V;
    impl Visitor<'_> for V {
        type Value = f64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a number or a decimal string")
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<f64, E> {
            v.parse().map_err(|_| E::custom(format!("bad number {v:?}")))
        }
    }
    d.deserialize_any(V)
}

fn levels(v: Vec<WsLevel>) -> Vec<Level> {
    v.into_iter().map(|l| Level { px: l.px, sz: l.sz, n: l.n }).collect()
}

/// Parse one text frame from the upstream websocket.
pub fn parse(text: &str) -> Result<Incoming, String> {
    let env: Envelope = serde_json::from_str(text).map_err(|e| format!("envelope: {e}"))?;
    let data = || env.data.map(RawValue::get).ok_or_else(|| format!("{} without data", env.channel));
    let err = |e: serde_json::Error| format!("{}: {e}", env.channel);
    Ok(match env.channel.as_ref() {
        "allMids" => {
            let m: AllMids = serde_json::from_str(data()?).map_err(err)?;
            // `#…` keys are outcome (HIP-4) markets, which are out of scope (§5.7).
            let mids = m
                .mids
                .into_iter()
                .filter(|(k, _)| !k.starts_with('#'))
                .filter_map(|(k, v)| Some((k, v.parse::<f64>().ok()?)))
                .collect();
            Incoming::Data(SubKey::AllMids { dex: m.dex.unwrap_or_default() }, UpstreamEvent::Mids(mids))
        }
        "l2Book" => {
            let b: WsBook = serde_json::from_str(data()?).map_err(err)?;
            let ev = UpstreamEvent::Book { bids: levels(b.levels.0), asks: levels(b.levels.1), time_ms: b.time };
            Incoming::Data(SubKey::L2Book { coin: b.coin }, ev)
        }
        "trades" => {
            let ts: Vec<WsTrade> = serde_json::from_str(data()?).map_err(err)?;
            let Some(coin) = ts.first().map(|t| t.coin.clone()) else { return Ok(Incoming::Ignored) };
            let trades = ts
                .into_iter()
                .map(|t| Trade {
                    px: t.px,
                    sz: t.sz,
                    side: if t.side == "B" { Side::Buy } else { Side::Sell },
                    time_ms: t.time,
                    tid: t.tid,
                })
                .collect();
            Incoming::Data(SubKey::Trades { coin }, UpstreamEvent::Trades(trades))
        }
        "candle" => {
            let c: WsCandle = serde_json::from_str(data()?).map_err(err)?;
            let Some(interval) = static_interval(&c.i) else { return Ok(Incoming::Ignored) };
            let bar = Bar::new(c.t.div_euclid(1000), c.o, c.h, c.l, c.c, c.v);
            Incoming::Data(SubKey::Candle { coin: c.s, interval }, UpstreamEvent::Bar(bar))
        }
        "subscriptionResponse" => {
            let ack: SubAck = serde_json::from_str(data()?).map_err(err)?;
            match (ack.subscription.key(), ack.method.as_str()) {
                (Some(k), "subscribe") => Incoming::Subscribed(k),
                (Some(k), "unsubscribe") => Incoming::Unsubscribed(k),
                _ => Incoming::Ignored,
            }
        }
        "pong" => Incoming::Pong,
        "error" => Incoming::Error(match env.data {
            Some(d) => serde_json::from_str::<String>(d.get()).unwrap_or_else(|_| d.get().to_string()),
            None => String::new(),
        }),
        _ => Incoming::Ignored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/hl/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn subscription_messages() {
        let k = SubKey::Candle { coin: "BTC".into(), interval: interval(Timeframe::M1) };
        assert_eq!(
            k.message("subscribe"),
            r#"{"method":"subscribe","subscription":{"coin":"BTC","interval":"1m","type":"candle"}}"#
        );
        assert_eq!(SubKey::AllMids { dex: String::new() }.subscription(), json!({"type": "allMids"}));
        assert_eq!(SubKey::AllMids { dex: "xyz".into() }.to_string(), "allMids:xyz");
        assert_eq!(k.to_string(), "candle:BTC:1m");
        assert_eq!(Timeframe::ALL.map(interval), ["1m", "5m", "15m", "30m", "1h", "4h", "1d", "1w", "1M"]);
    }

    #[test]
    fn recorded_messages() {
        let Incoming::Data(SubKey::AllMids { dex }, UpstreamEvent::Mids(m)) =
            parse(&fixture("all_mids_xyz.json")).unwrap()
        else {
            panic!("allMids")
        };
        assert_eq!(dex, "xyz");
        assert!(m.iter().any(|(k, px)| k == "xyz:TSLA" && *px > 0.0));
        let Incoming::Data(SubKey::AllMids { dex }, UpstreamEvent::Mids(m)) =
            parse(&fixture("all_mids_core.json")).unwrap()
        else {
            panic!("core allMids")
        };
        assert_eq!(dex, "");
        assert!(m.iter().any(|(k, _)| k == "BTC") && m.iter().any(|(k, _)| k.starts_with('@')));
        assert!(!m.iter().any(|(k, _)| k.starts_with('#')), "outcome markets are dropped");

        let Incoming::Data(SubKey::L2Book { coin }, UpstreamEvent::Book { bids, asks, time_ms }) =
            parse(&fixture("l2book_btc.json")).unwrap()
        else {
            panic!("l2Book")
        };
        assert_eq!((coin.as_str(), bids.len(), asks.len()), ("BTC", 20, 20));
        assert!(bids[0].px < asks[0].px && bids.windows(2).all(|w| w[0].px > w[1].px) && time_ms > 1_700_000_000_000);

        let Incoming::Data(SubKey::Trades { coin }, UpstreamEvent::Trades(t)) =
            parse(&fixture("trades_btc.json")).unwrap()
        else {
            panic!("trades")
        };
        assert_eq!(coin, "BTC");
        assert!(!t.is_empty() && t.iter().all(|x| x.px > 0.0 && x.sz > 0.0 && x.tid > 0));

        let Incoming::Data(SubKey::Candle { coin, interval }, UpstreamEvent::Bar(b)) =
            parse(&fixture("candle_btc_1m.json")).unwrap()
        else {
            panic!("candle")
        };
        assert_eq!((coin.as_str(), interval), ("BTC", "1m"));
        assert_eq!(b.ts % 60, 0);
        assert!(b.low <= b.open && b.open <= b.high && b.volume > 0.0);
    }

    #[test]
    fn control_messages() {
        let ack = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC","nSigFigs":null,"mantissa":null,"fast":false}}}"#;
        assert_eq!(parse(ack).unwrap(), Incoming::Subscribed(SubKey::L2Book { coin: "BTC".into() }));
        let ack = r#"{"channel":"subscriptionResponse","data":{"method":"unsubscribe","subscription":{"type":"candle","interval":"1h","coin":"xyz:TSLA"}}}"#;
        assert_eq!(
            parse(ack).unwrap(),
            Incoming::Unsubscribed(SubKey::Candle { coin: "xyz:TSLA".into(), interval: "1h" })
        );
        let ack =
            r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"allMids"}}}"#;
        assert_eq!(parse(ack).unwrap(), Incoming::Subscribed(SubKey::AllMids { dex: String::new() }));
        assert_eq!(parse(r#"{"channel":"pong"}"#).unwrap(), Incoming::Pong);
        assert_eq!(
            parse(r#"{"channel":"error","data":"Invalid subscription"}"#).unwrap(),
            Incoming::Error("Invalid subscription".into())
        );
        assert_eq!(parse(r#"{"channel":"trades","data":[]}"#).unwrap(), Incoming::Ignored);
        assert_eq!(parse(r#"{"channel":"webData3","data":{}}"#).unwrap(), Incoming::Ignored);
        assert!(parse("not json").is_err());
        // Numbers may come as JSON numbers, as the docs type candles.
        let c = r#"{"channel":"candle","data":{"t":60000,"T":119999,"s":"ETH","i":"5m","o":1,"c":2.5,"h":3,"l":0.5,"v":10,"n":4}}"#;
        assert_eq!(
            parse(c).unwrap(),
            Incoming::Data(
                SubKey::Candle { coin: "ETH".into(), interval: "5m" },
                UpstreamEvent::Bar(Bar::new(60, 1.0, 3.0, 0.5, 2.5, 10.0))
            )
        );
    }
}
