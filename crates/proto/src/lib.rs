//! The protocol between Terminal clients (desktop, web, CLI) and the Terminal data server
//! (`tr-gateway`), as designed in `docs/architecture/server.md` §4.
//!
//! Each message is one websocket frame. postcard over binary frames is the default encoding;
//! JSON over text frames is a debug encoding a client asks for in [`ClientMsg::Hello`]. The
//! server reads binary frames as postcard and text frames as JSON, and answers in the encoding
//! the client's `Hello` named.
//!
//! # Evolving the protocol
//!
//! postcard is not self-describing: an enum variant travels as its index and a struct as its
//! fields in order. So:
//! - enum variants are only ever appended, never reordered or removed;
//! - a struct never gains or loses a field (a new field means a new message variant);
//! - any other change bumps [`PROTOCOL_VERSION`], and the server keeps serving N and N-1.
//!
//! The `wire_tags_are_stable` test pins every variant's index. The design's later variants
//! (`DocOp`, `DocAck`/`DocConflict`, and the `Ctx`, Congress, Portfolio, Docs and Alerts
//! topics) are not here yet; each is appended after the variants below when a server serves it.
//! [`Bar`] and [`Timeframe`] come from `tr-core`, so their field and variant order is part of
//! the wire format too.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
pub use tr_core::{Bar, Timeframe};

/// Bumped on any change the evolution rules above don't allow.
pub const PROTOCOL_VERSION: u16 = 1;

/// Client-chosen subscription id, unique per connection.
pub type SubId = u32;
/// Client-chosen request id, unique per connection.
pub type ReqId = u32;

/// A market as a client names it: a venue-native key (`BTC`, `xyz:TSLA`, `@107`, `PURR/USDC`)
/// or a market key the server maps to one (`BTCUSD Curncy`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarketKey(pub String);

impl MarketKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MarketKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    Postcard,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// For example `terminal-desktop` or `tr-cli`.
    pub name: String,
    pub version: String,
}

/// Reserved for session resume (§4.3). Servers ignore it for now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResumeToken(pub String);

/// Client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// The first message on every connection. `token` is reserved for sign-in (§8) and is
    /// ignored for now; `resume` likewise.
    Hello {
        version: u16,
        encoding: Encoding,
        client: ClientInfo,
        token: Option<String>,
        resume: Option<ResumeToken>,
    },
    Subscribe {
        id: SubId,
        topic: TopicReq,
    },
    Unsubscribe {
        id: SubId,
    },
    /// The client saw a gap in `seq`; the server answers with a fresh `Snapshot`.
    Resync {
        id: SubId,
    },
    Request {
        id: ReqId,
        req: Rpc,
    },
    Ping {
        t: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TopicReq {
    /// Prices for many markets in one subscription, at most `max_hz` deltas a second.
    Quotes { keys: Vec<MarketKey>, max_hz: u8 },
    /// Snapshot: the last `history` bars, the forming bar last. Deltas: [`BarDelta`].
    Bars { key: MarketKey, tf: Timeframe, history: u32 },
    /// The top `depth` levels a side, bucketed to multiples of `group` when set.
    Book { key: MarketKey, depth: u8, group: Option<f64> },
    /// Snapshot: recent trades. Deltas: new trades, oldest first.
    Trades { key: MarketKey },
}

/// One-shot requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Rpc {
    /// Up to `limit` bars before `end` (Unix seconds; `None` = now), oldest first. The same
    /// contract as `MarketData::history` in `tr-data`.
    History { key: MarketKey, tf: Timeframe, end: Option<i64>, limit: u32 },
    /// Every market the server can stream right now.
    Markets,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RpcResult {
    History(Vec<Bar>),
    Markets(Vec<MarketInfo>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketInfo {
    pub key: MarketKey,
    /// Where it trades: a Hyperliquid perp dex (`""` is the core dex, `xyz`, ...) or `spot`.
    pub venue: String,
    pub kind: InstrumentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InstrumentKind {
    Perp,
    Spot,
}

/// Opaque per-connection id, for logs and support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub u64);

/// Reserved for sign-in (§8). Always `None` for now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub name: String,
}

/// What the server accepts on this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_subscriptions: u32,
    pub max_quote_keys: u32,
    pub max_book_depth: u8,
    pub max_history: u32,
}

/// Server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        version: u16,
        session: SessionId,
        user: Option<UserInfo>,
        server_time_ms: i64,
        limits: Limits,
    },
    /// Full state. Resets the subscription's `seq`.
    Snapshot {
        id: SubId,
        seq: u64,
        data: TopicData,
    },
    /// `seq` is the previous message's `seq` + 1.
    Delta {
        id: SubId,
        seq: u64,
        data: TopicDelta,
    },
    /// The server dropped deltas for this subscription; a `Snapshot` follows.
    Resync {
        id: SubId,
        reason: ResyncReason,
    },
    Reply {
        id: ReqId,
        result: Result<RpcResult, ApiError>,
    },
    Status {
        id: SubId,
        status: TopicStatus,
    },
    Pong {
        t: u64,
        server_time_ms: i64,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TopicData {
    Quotes(Vec<Quote>),
    Bars(Vec<Bar>),
    Book(Book),
    Trades(Vec<Trade>),
}

/// For state topics (quotes, book) a delta is the new value, so a lost one is harmless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TopicDelta {
    /// The quotes that changed since the last message.
    Quotes(Vec<Quote>),
    Bar(BarDelta),
    Book(Book),
    Trades(Vec<Trade>),
}

/// Bars stream as the forming bar's latest state, and a `Closed` bar with its final values
/// before the first `Forming` of the next bar. The server may drop a `Forming` that a later
/// message for the same `ts` supersedes; it never drops a `Closed`. Clients apply `Closed` by
/// replacing the bar with that `ts` (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BarDelta {
    Forming(Bar),
    Closed(Bar),
}

impl BarDelta {
    pub fn bar(&self) -> &Bar {
        match self {
            BarDelta::Forming(b) | BarDelta::Closed(b) => b,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    /// The key as the client sent it.
    pub key: MarketKey,
    /// The venue market that backs it (`BTC` for `BTCUSD Curncy`).
    pub source: MarketKey,
    pub px: f64,
    pub basis: PriceBasis,
    /// The price 24 hours earlier, when the venue publishes one.
    pub prev_day: Option<f64>,
    /// When the server received this price (Unix milliseconds).
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PriceBasis {
    Mid,
    Mark,
    Oracle,
    Last,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Book {
    /// Best first: bids descending, asks ascending.
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    /// The venue's time for this book (Unix milliseconds).
    pub time_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Level {
    pub px: f64,
    pub sz: f64,
    /// Orders at this level (0 when grouped levels were merged and the venue gave none).
    pub n: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub px: f64,
    pub sz: f64,
    /// The aggressor's side.
    pub side: Side,
    /// The venue's trade time (Unix milliseconds).
    pub time_ms: i64,
    /// Venue trade id; unique together with `time_ms`.
    pub tid: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResyncReason {
    /// The client fell too far behind an event topic.
    Overflow,
    /// The server's own upstream reconnected, so its state may have jumped.
    Upstream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopicStatus {
    Live,
    /// No fresh data since `since_ms` (Unix milliseconds); the last values may be old.
    Stale {
        since_ms: i64,
    },
    /// The server can't serve this subscription. It stays open but gets no data.
    Unavailable {
        reason: UnavailableReason,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnavailableReason {
    UnknownMarket,
    /// The server is at its upstream subscription budget.
    Capacity,
    Unsupported,
    Upstream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    BadMessage,
    UnsupportedVersion,
    HelloRequired,
    DuplicateId,
    UnknownId,
    TooManySubscriptions,
    SlowConsumer,
    UnknownMarket,
    Upstream,
    Internal,
    /// The client sent messages faster than `Limits` allows; the server ignored this one.
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

/// One encoded websocket frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Binary(Vec<u8>),
    Text(String),
}

impl Frame {
    pub fn len(&self) -> usize {
        match self {
            Frame::Binary(b) => b.len(),
            Frame::Text(t) => t.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub String);

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protocol codec: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

/// Encode a message as one frame: postcard in a binary frame, or JSON in a text frame.
pub fn encode<T: Serialize>(msg: &T, encoding: Encoding) -> Result<Frame, CodecError> {
    match encoding {
        Encoding::Postcard => postcard::to_stdvec(msg).map(Frame::Binary).map_err(|e| CodecError(e.to_string())),
        Encoding::Json => serde_json::to_string(msg).map(Frame::Text).map_err(|e| CodecError(e.to_string())),
    }
}

/// Decode a binary (postcard) frame.
pub fn decode_binary<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    postcard::from_bytes(bytes).map_err(|e| CodecError(e.to_string()))
}

/// Decode a text (JSON) frame.
pub fn decode_text<T: DeserializeOwned>(text: &str) -> Result<T, CodecError> {
    serde_json::from_str(text).map_err(|e| CodecError(e.to_string()))
}

/// Decode either kind of frame.
pub fn decode<T: DeserializeOwned>(frame: &Frame) -> Result<T, CodecError> {
    match frame {
        Frame::Binary(b) => decode_binary(b),
        Frame::Text(t) => decode_text(t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> MarketKey {
        MarketKey::new(s)
    }

    fn bar(ts: i64) -> Bar {
        Bar::new(ts, 76_612.0, 76_640.5, 76_443.0, 76_521.0, 74.46557)
    }

    fn book(levels: usize) -> Book {
        let side = |sign: f64| {
            (0..levels)
                .map(|i| Level { px: 76_530.0 + sign * i as f64, sz: 1.5 + i as f64 * 0.1, n: 3 + i as u32 })
                .collect()
        };
        Book { bids: side(-1.0), asks: side(1.0), time_ms: 1_793_325_942_459 }
    }

    fn quote(k: &str, src: &str) -> Quote {
        Quote {
            key: key(k),
            source: key(src),
            px: 76_521.5,
            basis: PriceBasis::Mid,
            prev_day: Some(75_000.0),
            ts_ms: 1_793_325_942_459,
        }
    }

    fn trade(i: u64) -> Trade {
        Trade { px: 76_528.0, sz: 0.41745, side: Side::Buy, time_ms: 1_793_325_941_395, tid: 1_005_251_635_753_713 + i }
    }

    fn client_msgs() -> Vec<ClientMsg> {
        vec![
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                encoding: Encoding::Postcard,
                client: ClientInfo { name: "tr-cli".into(), version: "0.1.1".into() },
                token: None,
                resume: None,
            },
            ClientMsg::Subscribe {
                id: 1,
                topic: TopicReq::Quotes { keys: vec![key("BTC"), key("xyz:TSLA")], max_hz: 4 },
            },
            ClientMsg::Unsubscribe { id: 1 },
            ClientMsg::Resync { id: 2 },
            ClientMsg::Request {
                id: 7,
                req: Rpc::History { key: key("BTCUSD Curncy"), tf: Timeframe::M1, end: None, limit: 500 },
            },
            ClientMsg::Ping { t: 42 },
        ]
    }

    fn server_msgs() -> Vec<ServerMsg> {
        let limits = Limits { max_subscriptions: 64, max_quote_keys: 512, max_book_depth: 20, max_history: 5000 };
        vec![
            ServerMsg::Welcome {
                version: PROTOCOL_VERSION,
                session: SessionId(9),
                user: None,
                server_time_ms: 1_793_325_942_000,
                limits,
            },
            ServerMsg::Snapshot { id: 1, seq: 0, data: TopicData::Quotes(vec![quote("BTCUSD Curncy", "BTC")]) },
            ServerMsg::Delta { id: 1, seq: 1, data: TopicDelta::Bar(BarDelta::Closed(bar(60))) },
            ServerMsg::Resync { id: 1, reason: ResyncReason::Overflow },
            ServerMsg::Reply { id: 7, result: Ok(RpcResult::History(vec![bar(0), bar(60)])) },
            ServerMsg::Status {
                id: 3,
                status: TopicStatus::Unavailable { reason: UnavailableReason::Capacity, detail: "budget".into() },
            },
            ServerMsg::Pong { t: 42, server_time_ms: 1_793_325_942_000 },
            ServerMsg::Error { code: ErrorCode::BadMessage, message: "nope".into() },
        ]
    }

    #[test]
    fn round_trips_in_both_encodings() {
        for enc in [Encoding::Postcard, Encoding::Json] {
            for m in client_msgs() {
                let f = encode(&m, enc).unwrap();
                assert_eq!(decode::<ClientMsg>(&f).unwrap(), m, "{enc:?}");
            }
            for m in server_msgs() {
                let f = encode(&m, enc).unwrap();
                assert_eq!(decode::<ServerMsg>(&f).unwrap(), m, "{enc:?}");
            }
            let extra = [
                ServerMsg::Snapshot { id: 4, seq: 10, data: TopicData::Book(book(20)) },
                ServerMsg::Snapshot { id: 5, seq: 0, data: TopicData::Trades(vec![trade(0), trade(1)]) },
                ServerMsg::Snapshot { id: 6, seq: 0, data: TopicData::Bars(vec![bar(0)]) },
                ServerMsg::Delta { id: 4, seq: 11, data: TopicDelta::Book(book(5)) },
                ServerMsg::Delta { id: 5, seq: 1, data: TopicDelta::Trades(vec![trade(2)]) },
                ServerMsg::Delta { id: 1, seq: 2, data: TopicDelta::Quotes(vec![quote("BTC", "BTC")]) },
                ServerMsg::Reply {
                    id: 8,
                    result: Ok(RpcResult::Markets(vec![MarketInfo {
                        key: key("xyz:TSLA"),
                        venue: "xyz".into(),
                        kind: InstrumentKind::Perp,
                    }])),
                },
                ServerMsg::Reply {
                    id: 9,
                    result: Err(ApiError { code: ErrorCode::UnknownMarket, message: "x".into() }),
                },
                ServerMsg::Status { id: 1, status: TopicStatus::Stale { since_ms: 5 } },
            ];
            for m in extra {
                assert_eq!(decode::<ServerMsg>(&encode(&m, enc).unwrap()).unwrap(), m);
            }
        }
    }

    #[test]
    fn encodings_pick_the_frame_kind() {
        let m = ClientMsg::Ping { t: 1 };
        assert!(matches!(encode(&m, Encoding::Postcard).unwrap(), Frame::Binary(_)));
        assert_eq!(encode(&m, Encoding::Json).unwrap(), Frame::Text(r#"{"Ping":{"t":1}}"#.into()));
        assert!(decode_binary::<ClientMsg>(&[0xff, 0xff]).is_err());
        assert!(decode_text::<ClientMsg>("{}").is_err());
    }

    /// postcard writes an enum variant as its index. Appending variants keeps these; reordering
    /// or removing one changes them and breaks every deployed client, so this test pins them.
    #[test]
    fn wire_tags_are_stable() {
        let tag = |f: Frame| match f {
            Frame::Binary(b) => b[0],
            Frame::Text(_) => unreachable!(),
        };
        let client: Vec<u8> = client_msgs().iter().map(|m| tag(encode(m, Encoding::Postcard).unwrap())).collect();
        assert_eq!(client, [0, 1, 2, 3, 4, 5]);
        let server: Vec<u8> = server_msgs().iter().map(|m| tag(encode(m, Encoding::Postcard).unwrap())).collect();
        assert_eq!(server, [0, 1, 2, 3, 4, 5, 6, 7]);
        let t = |v: Vec<u8>| v[0];
        let topics = [
            TopicReq::Quotes { keys: vec![], max_hz: 1 },
            TopicReq::Bars { key: key("BTC"), tf: Timeframe::M1, history: 1 },
            TopicReq::Book { key: key("BTC"), depth: 1, group: None },
            TopicReq::Trades { key: key("BTC") },
        ];
        assert_eq!(topics.map(|x| t(postcard::to_stdvec(&x).unwrap())), [0, 1, 2, 3]);
        let data =
            [TopicData::Quotes(vec![]), TopicData::Bars(vec![]), TopicData::Book(book(0)), TopicData::Trades(vec![])];
        assert_eq!(data.map(|x| t(postcard::to_stdvec(&x).unwrap())), [0, 1, 2, 3]);
        let deltas = [
            TopicDelta::Quotes(vec![]),
            TopicDelta::Bar(BarDelta::Forming(bar(0))),
            TopicDelta::Book(book(0)),
            TopicDelta::Trades(vec![]),
        ];
        assert_eq!(deltas.map(|x| t(postcard::to_stdvec(&x).unwrap())), [0, 1, 2, 3]);
        assert_eq!(
            [BarDelta::Forming(bar(0)), BarDelta::Closed(bar(0))].map(|x| t(postcard::to_stdvec(&x).unwrap())),
            [0, 1]
        );
        assert_eq!(
            [Rpc::History { key: key("B"), tf: Timeframe::D1, end: None, limit: 1 }, Rpc::Markets]
                .map(|x| t(postcard::to_stdvec(&x).unwrap())),
            [0, 1]
        );
        let status = [
            TopicStatus::Live,
            TopicStatus::Stale { since_ms: 0 },
            TopicStatus::Unavailable { reason: UnavailableReason::Upstream, detail: String::new() },
        ];
        assert_eq!(status.map(|x| t(postcard::to_stdvec(&x).unwrap())), [0, 1, 2]);
        // tr-core types are part of the wire format: Timeframe's variant order and Bar's fields.
        let tfs: Vec<u8> = Timeframe::ALL.iter().map(|tf| t(postcard::to_stdvec(tf).unwrap())).collect();
        assert_eq!(tfs, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let b = postcard::to_stdvec(&Bar::new(1, 2.0, 3.0, 4.0, 5.0, 6.0)).unwrap();
        assert_eq!(b.len(), 1 + 5 * 8, "ts as a varint, then five f64 in field order");
        assert_eq!(&b[1..9], &2.0f64.to_le_bytes());
    }

    /// Frame sizes for the messages that dominate traffic (§11 estimates ~30 bytes a quote).
    #[test]
    fn frame_sizes() {
        let size = |m: &ServerMsg, e| encode(m, e).unwrap().len();
        let one_quote = ServerMsg::Delta { id: 1, seq: 1234, data: TopicDelta::Quotes(vec![quote("BTC", "BTC")]) };
        let book20 = ServerMsg::Delta { id: 2, seq: 1234, data: TopicDelta::Book(book(20)) };
        let one_trade = ServerMsg::Delta { id: 3, seq: 1234, data: TopicDelta::Trades(vec![trade(0)]) };
        let forming = ServerMsg::Delta { id: 4, seq: 1234, data: TopicDelta::Bar(BarDelta::Forming(bar(60))) };
        let rows = [("quote", &one_quote), ("book 20x2", &book20), ("trade", &one_trade), ("bar", &forming)];
        for (name, m) in rows {
            let (p, j) = (size(m, Encoding::Postcard), size(m, Encoding::Json));
            println!("{name:>9}: postcard {p:>4} B, json {j:>5} B ({:.1}x)", j as f64 / p as f64);
            assert!(p < j, "{name}: postcard should be smaller");
        }
        assert!(size(&one_quote, Encoding::Postcard) <= 40);
        assert!(size(&one_trade, Encoding::Postcard) <= 40);
        assert!(size(&forming, Encoding::Postcard) <= 50);
        assert!(size(&book20, Encoding::Postcard) <= 40 * 18 + 16);
    }
}
