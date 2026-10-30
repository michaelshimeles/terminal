//! The gateway end to end, against a mock of Hyperliquid's public API (websocket and `info` on
//! one local port) and through the desktop's client (`tr_data::remote`).

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tr_data::remote::{Client, Event, Subscription};
use tr_gateway::Config;
use tr_proto::{
    BarDelta, Encoding, InstrumentKind, MarketKey, ResyncReason, Rpc, RpcResult, Timeframe, TopicData, TopicDelta,
    TopicReq, TopicStatus, UnavailableReason,
};

const WAIT: Duration = Duration::from_secs(10);

// ---- the mock upstream ----

/// A mock connection: where to send it frames (`None` closes it), and what it subscribed to.
type MockConn = (mpsc::UnboundedSender<Option<String>>, Arc<Mutex<HashSet<String>>>);

#[derive(Default)]
struct Mock {
    /// Every subscribe and unsubscribe received, as `subscribe l2Book:BTC`.
    log: Mutex<Vec<String>>,
    conns: Mutex<Vec<MockConn>>,
}

fn label(sub: &Value) -> String {
    let s = |k: &str| sub[k].as_str().unwrap_or_default().to_string();
    match s("type").as_str() {
        "allMids" if s("dex").is_empty() => "allMids".into(),
        "allMids" => format!("allMids:{}", s("dex")),
        "candle" => format!("candle:{}:{}", s("coin"), s("interval")),
        t => format!("{t}:{}", s("coin")),
    }
}

impl Mock {
    fn subs(&self) -> HashSet<String> {
        self.conns.lock().unwrap().iter().flat_map(|(_, s)| s.lock().unwrap().clone()).collect()
    }

    fn count(&self, entry: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|e| *e == entry).count()
    }

    /// Send `data` on `channel` to every connection subscribed to `sub`.
    fn push(&self, sub: &str, channel: &str, data: Value) {
        let text = json!({ "channel": channel, "data": data }).to_string();
        for (tx, subs) in self.conns.lock().unwrap().iter() {
            if subs.lock().unwrap().contains(sub) {
                let _ = tx.send(Some(text.clone()));
            }
        }
    }

    /// Close every upstream connection, as Hyperliquid may at any time.
    fn drop_all(&self) {
        for (tx, _) in self.conns.lock().unwrap().drain(..) {
            let _ = tx.send(None);
        }
    }

    async fn until(&self, what: &str, f: impl Fn(&Mock) -> bool) {
        let t0 = tokio::time::Instant::now();
        while !f(self) {
            assert!(t0.elapsed() < WAIT, "timed out waiting for {what}; upstream has {:?}", self.subs());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn mock_ws(State(m): State<Arc<Mock>>, up: WebSocketUpgrade) -> axum::response::Response {
    up.on_upgrade(move |ws| mock_conn(ws, m))
}

async fn mock_conn(ws: WebSocket, m: Arc<Mock>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<String>>();
    let subs = Arc::new(Mutex::new(HashSet::new()));
    m.conns.lock().unwrap().push((tx.clone(), subs.clone()));
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(Some(text)) => if sink.send(Message::Text(text.into())).await.is_err() { return },
                _ => {
                    let _ = sink.close().await;
                    return;
                }
            },
            msg = stream.next() => {
                let Some(Ok(Message::Text(t))) = msg else {
                    if matches!(msg, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { return }
                    continue;
                };
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                match v["method"].as_str() {
                    Some("ping") => { let _ = tx.send(Some(json!({"channel": "pong"}).to_string())); }
                    Some(method @ ("subscribe" | "unsubscribe")) => {
                        let l = label(&v["subscription"]);
                        m.log.lock().unwrap().push(format!("{method} {l}"));
                        if method == "subscribe" { subs.lock().unwrap().insert(l); } else { subs.lock().unwrap().remove(&l); }
                        let ack = json!({"channel": "subscriptionResponse", "data": {"method": method, "subscription": v["subscription"]}});
                        let _ = tx.send(Some(ack.to_string()));
                    }
                    _ => {}
                }
            }
        }
    }
}

fn interval_s(i: &str) -> i64 {
    match i {
        "1m" => 60,
        "5m" => 300,
        "1h" => 3600,
        "1d" => 86_400,
        _ => 60,
    }
}

async fn mock_info(axum::Json(req): axum::Json<Value>) -> axum::Json<Value> {
    axum::Json(match req["type"].as_str().unwrap() {
        "perpDexs" => json!([null, {"name": "xyz"}]),
        "metaAndAssetCtxs" if req["dex"] == "xyz" => {
            json!([{"universe": [{"name": "xyz:TSLA"}]}, [{"prevDayPx": "400.0"}]])
        }
        "metaAndAssetCtxs" => json!([
            {"universe": [{"name": "BTC"}, {"name": "ETH"}, {"name": "OLD", "isDelisted": true}]},
            [{"prevDayPx": "100.0"}, {"prevDayPx": "10.0"}, {"prevDayPx": "1.0"}]
        ]),
        "candleSnapshot" => {
            let r = &req["req"];
            let iv = interval_s(r["interval"].as_str().unwrap()) * 1000;
            let (start, end) = (r["startTime"].as_i64().unwrap(), r["endTime"].as_i64().unwrap());
            let first = (start + iv - 1) / iv;
            let bars: Vec<Value> = (first..=end / iv)
                .map(|k| {
                    let px = format!("{}.0", 100 + k % 10);
                    json!({"t": k * iv, "T": k * iv + iv - 1, "s": r["coin"], "i": r["interval"], "o": px, "h": px, "l": px, "c": px, "v": "1.0", "n": 1})
                })
                .collect();
            Value::Array(bars[bars.len().saturating_sub(5000)..].to_vec())
        }
        other => panic!("unexpected info request {other}"),
    })
}

async fn start_mock() -> (Arc<Mock>, SocketAddr) {
    let m = Arc::new(Mock::default());
    let app = Router::new().route("/ws", get(mock_ws)).route("/info", post(mock_info)).with_state(m.clone());
    let l = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (m, addr)
}

// ---- helpers ----

struct Setup {
    mock: Arc<Mock>,
    gw: tr_gateway::Gateway,
}

impl Setup {
    async fn new(tweak: impl FnOnce(&mut Config)) -> Setup {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (mock, addr) = start_mock().await;
        let mut cfg = Config { bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), ..Default::default() };
        cfg.hl.info_url = format!("http://{addr}/info");
        cfg.hl.pool.ws_url = format!("ws://{addr}/ws");
        cfg.hub.linger = Duration::from_millis(300);
        tweak(&mut cfg);
        let gw = tr_gateway::start(cfg).await.unwrap();
        mock.until("both venues' mids", |m| m.subs().is_superset(&set(&["allMids", "allMids:xyz"]))).await;
        mock.push("allMids", "allMids", json!({"mids": {"BTC": "100.5", "ETH": "10.25", "@107": "30.0", "#5": "0.5"}}));
        mock.push("allMids:xyz", "allMids", json!({"dex": "xyz", "mids": {"xyz:TSLA": "401.5"}}));
        let s = Setup { mock, gw };
        let t0 = tokio::time::Instant::now();
        while s.gw.hub().mids.len() < 4 {
            assert!(t0.elapsed() < WAIT, "mids never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        s
    }

    fn client(&self, enc: Encoding) -> Client {
        Client::connect(&format!("ws://{}", self.gw.addr), enc, "gateway-test")
    }

    fn upstream_keys(&self) -> Vec<String> {
        self.gw.hub().provider().stats().keys
    }
}

fn set(v: &[&str]) -> HashSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

async fn next(sub: &mut Subscription) -> Event {
    tokio::time::timeout(WAIT, sub.next()).await.expect("an update in time").expect("client alive").event
}

/// The next event that isn't a status or reconnect notice.
async fn next_data(sub: &mut Subscription) -> Event {
    loop {
        match next(sub).await {
            Event::Status(_) | Event::Reconnecting => {}
            e => return e,
        }
    }
}

fn book_msg(coin: &str, bid: &str) -> Value {
    json!({"coin": coin, "time": 1_793_325_942_459i64, "levels": [
        [{"px": bid, "sz": "1.5", "n": 3}, {"px": "99.0", "sz": "2", "n": 1}],
        [{"px": "101.0", "sz": "1", "n": 2}]
    ]})
}

fn trade(tid: u64) -> Value {
    json!({"coin": "BTC", "side": "B", "px": "100.5", "sz": "0.1", "time": tid as i64, "hash": "0x0", "tid": tid, "users": ["0x0", "0x0"]})
}

// ---- tests ----

#[tokio::test]
async fn clients_share_one_upstream_subscription_until_the_last_leaves() {
    let s = Setup::new(|_| {}).await;
    let (a, b) = (s.client(Encoding::Postcard), s.client(Encoding::Postcard));
    let topic = || TopicReq::Book { key: MarketKey::new("BTC"), depth: 10, group: None };
    let mut sa = a.subscribe(topic());
    let mut sb = b.subscribe(topic());
    s.mock.until("l2Book:BTC upstream", |m| m.subs().contains("l2Book:BTC")).await;
    s.mock.push("l2Book:BTC", "l2Book", book_msg("BTC", "100.0"));
    for sub in [&mut sa, &mut sb] {
        let Event::Snapshot(TopicData::Book(book)) = next_data(sub).await else { panic!("book snapshot") };
        assert_eq!((book.bids[0].px, book.bids[0].sz, book.bids[0].n, book.asks.len()), (100.0, 1.5, 3, 1));
    }
    // A third client on the same market adds nothing upstream.
    let c = s.client(Encoding::Postcard);
    let mut sc = c.subscribe(topic());
    let Event::Snapshot(TopicData::Book(_)) = next_data(&mut sc).await else { panic!("hot snapshot") };
    assert_eq!(s.mock.count("subscribe l2Book:BTC"), 1, "one upstream subscription for three clients");
    assert_eq!(s.upstream_keys(), ["allMids", "allMids:xyz", "l2Book:BTC"]);
    // Deltas reach every client, in sequence.
    s.mock.push("l2Book:BTC", "l2Book", book_msg("BTC", "100.25"));
    for sub in [&mut sa, &mut sb, &mut sc] {
        let Event::Delta(TopicDelta::Book(book)) = next_data(sub).await else { panic!("book delta") };
        assert_eq!(book.bids[0].px, 100.25);
    }
    drop((sa, sb));
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(s.mock.subs().contains("l2Book:BTC"), "one client still watches");
    drop(sc);
    s.mock.until("the upstream unsubscribe after the linger", |m| m.count("unsubscribe l2Book:BTC") == 1).await;
    assert_eq!(s.upstream_keys(), ["allMids", "allMids:xyz"]);
    assert_eq!(s.gw.hub().topic_counts().book, 0);
}

#[tokio::test]
async fn subscriptions_past_the_upstream_budget_are_refused() {
    // Two permanent mids subscriptions plus one more.
    let s = Setup::new(|c| c.hl.pool.max_subscriptions = 3).await;
    let client = s.client(Encoding::Postcard);
    let book = client.subscribe(TopicReq::Book { key: MarketKey::new("BTC"), depth: 5, group: None });
    s.mock.until("l2Book:BTC upstream", |m| m.subs().contains("l2Book:BTC")).await;
    let mut trades = client.subscribe(TopicReq::Trades { key: MarketKey::new("ETH") });
    match next(&mut trades).await {
        Event::Status(TopicStatus::Unavailable { reason: UnavailableReason::Capacity, detail }) => {
            assert!(detail.contains("3 of 3"), "{detail}")
        }
        e => panic!("expected a capacity refusal, got {e:?}"),
    }
    assert_eq!(s.mock.count("subscribe trades:ETH"), 0, "nothing past the budget reaches upstream");
    // Freeing the slot lets the next subscription through.
    drop(book);
    s.mock.until("the book's unsubscribe", |m| m.count("unsubscribe l2Book:BTC") == 1).await;
    let _trades = client.subscribe(TopicReq::Trades { key: MarketKey::new("ETH") });
    s.mock.until("trades:ETH upstream", |m| m.subs().contains("trades:ETH")).await;
}

#[tokio::test]
async fn unknown_markets_never_reach_upstream_and_keys_resolve() {
    let s = Setup::new(|_| {}).await;
    let client = s.client(Encoding::Postcard);
    let mut nope = client.subscribe(TopicReq::Book { key: MarketKey::new("NOTACOIN"), depth: 5, group: None });
    assert!(matches!(
        next(&mut nope).await,
        Event::Status(TopicStatus::Unavailable { reason: UnavailableReason::UnknownMarket, .. })
    ));
    let keys = ["BTCUSD Curncy", "ETH-PERP", "xyz:TSLA", "@107", "#5", "AAPL US Equity"].map(MarketKey::new).to_vec();
    let mut q = client.subscribe(TopicReq::Quotes { keys, max_hz: 4 });
    let Event::Snapshot(TopicData::Quotes(quotes)) = next(&mut q).await else { panic!("quotes") };
    let got: HashMap<&str, (&str, f64, Option<f64>)> =
        quotes.iter().map(|q| (q.key.as_str(), (q.source.as_str(), q.px, q.prev_day))).collect();
    assert_eq!(got["BTCUSD Curncy"], ("BTC", 100.5, Some(100.0)));
    assert_eq!(got["ETH-PERP"], ("ETH", 10.25, Some(10.0)));
    assert_eq!(got["xyz:TSLA"], ("xyz:TSLA", 401.5, Some(400.0)));
    assert_eq!(got["@107"], ("@107", 30.0, None));
    assert!(!got.contains_key("#5"), "outcome markets are out of scope");
    assert!(!got.contains_key("AAPL US Equity"), "equities don't map to HIP-3 perps yet");
    // A changed mid arrives as a delta with just that quote.
    s.mock.push("allMids", "allMids", json!({"mids": {"BTC": "100.75", "ETH": "10.25", "@107": "30.0"}}));
    let Event::Delta(TopicDelta::Quotes(d)) = next(&mut q).await else { panic!("quote delta") };
    assert_eq!(d.iter().map(|q| (q.key.as_str(), q.px)).collect::<Vec<_>>(), [("BTCUSD Curncy", 100.75)]);
    assert!(s.mock.log.lock().unwrap().iter().all(|e| !e.contains("NOTACOIN")));
    // The market list: perps by venue, spot, no outcomes.
    let Ok(RpcResult::Markets(m)) = client.request(Rpc::Markets).await else { panic!("markets") };
    let find = |k: &str| m.iter().find(|x| x.key.as_str() == k).map(|x| (x.venue.as_str(), x.kind));
    assert_eq!(find("BTC"), Some(("", InstrumentKind::Perp)));
    assert_eq!(find("xyz:TSLA"), Some(("xyz", InstrumentKind::Perp)));
    assert_eq!(find("@107"), Some(("spot", InstrumentKind::Spot)));
    assert_eq!(find("#5"), None);
}

#[tokio::test]
async fn upstream_reconnects_resubscribe_and_clients_resync() {
    let s = Setup::new(|_| {}).await;
    let client = s.client(Encoding::Postcard);
    let mut sub = client.subscribe(TopicReq::Trades { key: MarketKey::new("BTC") });
    s.mock.until("trades:BTC upstream", |m| m.subs().contains("trades:BTC")).await;
    let Event::Snapshot(TopicData::Trades(t)) = next(&mut sub).await else { panic!("trades snapshot") };
    assert!(t.is_empty());
    s.mock.push("trades:BTC", "trades", json!([trade(1), trade(2)]));
    let Event::Delta(TopicDelta::Trades(t)) = next(&mut sub).await else { panic!("trades delta") };
    assert_eq!(t.iter().map(|t| t.tid).collect::<Vec<_>>(), [1, 2]);

    s.mock.drop_all();
    assert!(matches!(next(&mut sub).await, Event::Status(TopicStatus::Stale { .. })));
    s.mock
        .until("resubscribed after reconnecting", |m| {
            m.count("subscribe trades:BTC") == 2 && m.count("subscribe allMids") == 2
        })
        .await;
    // Hyperliquid starts a (re)subscription with recent trades, overlapping what we have.
    s.mock.push("trades:BTC", "trades", json!([trade(2), trade(3)]));
    let mut seen = Vec::new();
    loop {
        match next(&mut sub).await {
            Event::Snapshot(TopicData::Trades(t)) => {
                assert_eq!(t.iter().map(|t| t.tid).collect::<Vec<_>>(), [1, 2, 3], "deduplicated");
                break;
            }
            Event::Status(TopicStatus::Live) => seen.push("live"),
            Event::Resync(ResyncReason::Upstream) => seen.push("resync"),
            e => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!(seen, ["live", "resync"]);
    assert_eq!(s.gw.hub().provider().stats().connections_up, 2);
    let stats = tr_gateway::http::stats(&s.gw.state);
    assert_eq!(stats.upstream_reconnects, 2, "both connections came back");
}

#[tokio::test]
async fn bars_stream_closed_before_the_next_bar_and_history_merges() {
    let s = Setup::new(|_| {}).await;
    let client = s.client(Encoding::Postcard);
    let Ok(RpcResult::History(h)) = client
        .request(Rpc::History { key: MarketKey::new("BTCUSD Curncy"), tf: Timeframe::H1, end: None, limit: 10 })
        .await
    else {
        panic!("history")
    };
    assert_eq!(h.len(), 10);
    assert!(h.windows(2).all(|w| w[1].ts - w[0].ts == 3600));
    let mut sub = client.subscribe(TopicReq::Bars { key: MarketKey::new("BTC"), tf: Timeframe::M1, history: 3 });
    s.mock.until("candle:BTC:1m upstream", |m| m.subs().contains("candle:BTC:1m")).await;
    let Event::Snapshot(TopicData::Bars(snap)) = next(&mut sub).await else { panic!("bars snapshot") };
    assert_eq!(snap.len(), 3);
    let t0 = snap.last().unwrap().ts + 60;
    let candle = |t: i64, c: &str| json!({"t": t * 1000, "T": t * 1000 + 59_999, "s": "BTC", "i": "1m", "o": "100", "h": "200", "l": "50", "c": c, "v": "3", "n": 9});
    for (t, c) in [(t0, "101"), (t0, "102"), (t0 + 60, "103")] {
        s.mock.push("candle:BTC:1m", "candle", candle(t, c));
    }
    let mut got = Vec::new();
    while got.last().is_none_or(|l| l != "F60:103") {
        if let Event::Delta(TopicDelta::Bar(d)) = next_data(&mut sub).await {
            got.push(match d {
                BarDelta::Forming(b) => format!("F{}:{}", b.ts - t0, b.close),
                BarDelta::Closed(b) => format!("C{}:{}", b.ts - t0, b.close),
            });
        }
    }
    // A reader may skip a forming bar a later message supersedes, but the closed bar's final
    // values always arrive, before the next bar.
    let closed = got.iter().position(|g| g == "C0:102").expect("the closed bar");
    assert_eq!(&got[closed..], ["C0:102", "F60:103"], "{got:?}");
    assert!(got[..closed].iter().all(|g| g.starts_with("F0:")), "{got:?}");
    // History now includes what the live stream has seen.
    let Ok(RpcResult::History(h)) =
        client.request(Rpc::History { key: MarketKey::new("BTC"), tf: Timeframe::M1, end: None, limit: 5 }).await
    else {
        panic!("history")
    };
    assert_eq!(h.last().map(|b| (b.ts, b.close)), Some((t0 + 60, 103.0)));
}

#[tokio::test]
async fn json_is_a_debug_encoding() {
    let s = Setup::new(|_| {}).await;
    let client = s.client(Encoding::Json);
    let mut q = client.subscribe(TopicReq::Quotes { keys: vec![MarketKey::new("xyz:TSLA")], max_hz: 1 });
    let Event::Snapshot(TopicData::Quotes(quotes)) = next(&mut q).await else { panic!("quotes over JSON") };
    assert_eq!(quotes[0].px, 401.5);
    let stats = tr_gateway::http::stats(&s.gw.state);
    assert_eq!((stats.clients, stats.upstream.subscriptions), (1, 2));
}

#[tokio::test]
async fn clients_that_send_too_fast_are_throttled() {
    let s = Setup::new(|c| c.session.max_messages_per_10s = 3).await;
    let client = s.client(Encoding::Postcard);
    assert!(client.wait_up(WAIT).await);
    // The client's first ping and these five subscribes share a budget of three messages.
    let mut subs: Vec<_> =
        (0..5).map(|_| client.subscribe(TopicReq::Quotes { keys: vec![MarketKey::new("BTC")], max_hz: 1 })).collect();
    let mut answered = 0;
    for sub in &mut subs {
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(500), sub.next()).await {
            answered += 1;
        }
    }
    assert!((2..=3).contains(&answered), "{answered} of 5 subscriptions were served");
}
