//! HIP-3 markets (`xyz:AAPL`) where the gateway reads Hyperliquid's coin names, from frames
//! recorded off the public websocket on 2026-11-02 (hashes and wallet addresses zeroed).

use tr_gateway::hl::wire::{Incoming, SubKey, interval, parse};
use tr_gateway::provider::UpstreamEvent;
use tr_proto::{Side, Timeframe};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/hl/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// Book, trade and candle frames for a HIP-3 coin carry its dex prefix, so they reach the
/// subscription that asked for `xyz:AAPL`, exactly like a core coin's.
#[test]
fn hip3_frames_route_by_their_prefixed_coin() {
    let Ok(Incoming::Data(SubKey::L2Book { coin }, UpstreamEvent::Book { bids, asks, time_ms })) =
        parse(&fixture("l2book_xyz_aapl.json"))
    else {
        panic!("l2Book")
    };
    assert_eq!(coin, "xyz:AAPL");
    assert_eq!((bids.len(), asks.len()), (20, 20));
    assert!(bids.windows(2).all(|w| w[0].px > w[1].px) && asks.windows(2).all(|w| w[0].px < w[1].px));
    assert!(bids[0].px < asks[0].px && (300.0..400.0).contains(&bids[0].px), "{}", bids[0].px);
    assert!(bids.iter().chain(&asks).all(|l| l.sz > 0.0 && l.n > 0) && time_ms > 1_793_000_000_000);

    let Ok(Incoming::Data(SubKey::Trades { coin }, UpstreamEvent::Trades(trades))) =
        parse(&fixture("trades_xyz_aapl.json"))
    else {
        panic!("trades")
    };
    assert_eq!((coin.as_str(), trades.len()), ("xyz:AAPL", 5));
    assert!(trades.iter().all(|t| t.px > 300.0 && t.sz > 0.0 && t.tid > 0));
    assert_eq!(trades[0].side, Side::Buy);

    let Ok(Incoming::Data(SubKey::Candle { coin, interval: iv }, UpstreamEvent::Bar(bar))) =
        parse(&fixture("candle_xyz_aapl_1m.json"))
    else {
        panic!("candle")
    };
    assert_eq!((coin.as_str(), iv), ("xyz:AAPL", "1m"));
    assert_eq!((bar.ts % 60, bar.open, bar.close, bar.volume), (0, 338.49, 338.4, 95.424));
    assert!(bar.low <= bar.close && bar.close <= bar.high);
}

/// What the gateway sends for a HIP-3 coin names it with the prefix, and Hyperliquid's
/// acknowledgement (recorded) maps back to the same subscription.
#[test]
fn hip3_subscriptions_keep_the_prefix() {
    let coin = || "xyz:AAPL".to_string();
    for key in [
        SubKey::L2Book { coin: coin() },
        SubKey::Trades { coin: coin() },
        SubKey::Candle { coin: coin(), interval: interval(Timeframe::M1) },
    ] {
        assert_eq!(key.subscription()["coin"], "xyz:AAPL", "{key}");
    }
    let ack = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"l2Book","coin":"xyz:AAPL","nSigFigs":null,"mantissa":null,"fast":false}}}"#;
    assert_eq!(parse(ack).unwrap(), Incoming::Subscribed(SubKey::L2Book { coin: coin() }));
    let ack = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"candle","coin":"xyz:AAPL","interval":"1m"}}}"#;
    assert_eq!(parse(ack).unwrap(), Incoming::Subscribed(SubKey::Candle { coin: coin(), interval: "1m" }));
}
