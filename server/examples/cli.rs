//! A small client for trying and measuring a running gateway.
//!
//! ```text
//! cargo run --release -p tr-gateway --example cli -- [--url ws://localhost:8787] [--json] COMMAND
//!
//!   markets [FILTER]          list the markets the server streams
//!   watch   [TOPICS] [--secs N]
//!           stream topics, print about one line a second per topic, then latency stats
//!   load    --clients N [TOPICS] [--secs N]
//!           N connections with the same subscriptions; prints totals and latency
//!   stats   print the server's /stats
//!
//! TOPICS: --quotes K,K,..  --book K  --trades K  --bars K:TF (TF: 1m 5m 15m 30m 1h 4h 1d 1w 1M)
//! ```
//!
//! Latency, measured at the client on the same clock as the server when both run on one host:
//! quotes from the server's upstream receive time (`Quote::ts_ms`), books and trades from
//! Hyperliquid's own timestamps.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tr_data::remote::{Client, Event, Subscription, normalize_url};
use tr_proto::{BarDelta, Encoding, MarketKey, Rpc, RpcResult, Timeframe, TopicData, TopicDelta, TopicReq};

#[derive(Default)]
struct Args {
    url: String,
    json: bool,
    command: String,
    filter: Option<String>,
    topics: Vec<TopicReq>,
    secs: u64,
    clients: usize,
}

const USAGE: &str = "\
usage: cli [--url ws://localhost:8787] [--json] COMMAND
  markets [FILTER]                    list the markets the server streams
  watch [TOPICS] [--secs N]           stream topics, then print latency stats
  load --clients N [TOPICS] [--secs N]
                                      N connections with the same subscriptions
  stats                               print the server's /stats
TOPICS: --quotes K,K,..  --book K  --trades K  --bars K:TF (1m 5m 15m 30m 1h 4h 1d 1w 1M)";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn parse_tf(s: &str) -> Result<Timeframe> {
    Ok(match s {
        "1m" => Timeframe::M1,
        "5m" => Timeframe::M5,
        "15m" => Timeframe::M15,
        "30m" => Timeframe::M30,
        "1h" => Timeframe::H1,
        "4h" => Timeframe::H4,
        "1d" => Timeframe::D1,
        "1w" => Timeframe::W1,
        "1M" => Timeframe::MN1,
        _ => bail!("unknown timeframe {s}"),
    })
}

fn parse() -> Result<Args> {
    let mut a = Args { url: "ws://localhost:8787".into(), secs: 30, clients: 1, ..Default::default() };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--url" => a.url = val()?,
            "--json" => a.json = true,
            "--secs" => a.secs = val()?.parse()?,
            "--clients" => a.clients = val()?.parse()?,
            "--quotes" => {
                let keys = val()?.split(',').map(|k| MarketKey::new(k.trim())).collect();
                a.topics.push(TopicReq::Quotes { keys, max_hz: 10 });
            }
            "--book" => a.topics.push(TopicReq::Book { key: MarketKey::new(val()?), depth: 20, group: None }),
            "--trades" => a.topics.push(TopicReq::Trades { key: MarketKey::new(val()?) }),
            "--bars" => {
                let v = val()?;
                let (k, tf) = v.rsplit_once(':').context("--bars wants KEY:TF")?;
                a.topics.push(TopicReq::Bars { key: MarketKey::new(k), tf: parse_tf(tf)?, history: 5 });
            }
            "-h" | "--help" => usage(),
            c if a.command.is_empty() => a.command = c.into(),
            f if a.command == "markets" && a.filter.is_none() => a.filter = Some(f.into()),
            other => bail!("unexpected argument {other}"),
        }
    }
    if a.command.is_empty() {
        usage();
    }
    Ok(a)
}

fn now_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn topic_name(t: &TopicReq) -> String {
    match t {
        TopicReq::Quotes { keys, .. } => {
            format!("quotes[{}]", keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(","))
        }
        TopicReq::Book { key, .. } => format!("book {key}"),
        TopicReq::Trades { key } => format!("trades {key}"),
        TopicReq::Bars { key, tf, .. } => format!("bars {key} {tf}"),
    }
}

/// Latency samples (ms) by measure, plus message counts by topic.
#[derive(Default)]
struct Tally {
    lat: BTreeMap<&'static str, Vec<i64>>,
    msgs: BTreeMap<String, u64>,
    resyncs: u64,
    statuses: Vec<String>,
}

impl Tally {
    fn report(&self) {
        for (name, n) in &self.msgs {
            println!("  {name}: {n} messages");
        }
        for (what, v) in &self.lat {
            let mut v = v.clone();
            v.sort_unstable();
            let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
            if !v.is_empty() {
                println!(
                    "  latency {what}: n={} p50={}ms p90={}ms p99={}ms max={}ms",
                    v.len(),
                    q(0.5),
                    q(0.9),
                    q(0.99),
                    v[v.len() - 1]
                );
            }
        }
        println!("  resyncs: {}  statuses: {:?}", self.resyncs, self.statuses);
    }
}

const QUOTE_LAT: &str = "quotes (server upstream receive -> client)";
const BOOK_LAT: &str = "book (Hyperliquid time -> client)";
const TRADE_LAT: &str = "trades (Hyperliquid time -> client)";

/// Read one subscription until `until`, tallying and (when `print`) describing what arrives.
async fn drain(mut sub: Subscription, name: String, tally: Arc<Mutex<Tally>>, until: Instant, print: bool) {
    let mut last_print = Instant::now() - Duration::from_secs(5);
    // Hyperliquid opens a trades subscription with recent history; only later trades are live.
    let since = now_ms(SystemTime::now());
    loop {
        let Ok(Some(u)) = tokio::time::timeout_at(until.into(), sub.next()).await else { return };
        let at = now_ms(u.received);
        let mut line = None;
        {
            let mut t = tally.lock().unwrap();
            *t.msgs.entry(name.clone()).or_default() += 1;
            match &u.event {
                Event::Snapshot(TopicData::Quotes(q)) | Event::Delta(TopicDelta::Quotes(q)) => {
                    // A snapshot's `ts_ms` is the price's age; a delta's is when it changed.
                    for x in q.iter().filter(|_| matches!(u.event, Event::Delta(_))) {
                        t.lat.entry(QUOTE_LAT).or_default().push(at - x.ts_ms);
                    }
                    let f = |x: &tr_proto::Quote| {
                        format!("{}={} (from {}, prev day {:?})", x.key, x.px, x.source, x.prev_day)
                    };
                    line = Some(q.iter().map(f).collect::<Vec<_>>().join("  "));
                }
                Event::Snapshot(TopicData::Book(b)) | Event::Delta(TopicDelta::Book(b)) => {
                    t.lat.entry(BOOK_LAT).or_default().push(at - b.time_ms);
                    if let (Some(bid), Some(ask)) = (b.bids.first(), b.asks.first()) {
                        line = Some(format!(
                            "{}x{} levels, best {} x {} / {} x {}",
                            b.bids.len(),
                            b.asks.len(),
                            bid.px,
                            bid.sz,
                            ask.px,
                            ask.sz
                        ));
                    }
                }
                Event::Snapshot(TopicData::Trades(tr)) | Event::Delta(TopicDelta::Trades(tr)) => {
                    for x in tr.iter().filter(|x| x.time_ms >= since) {
                        t.lat.entry(TRADE_LAT).or_default().push(at - x.time_ms);
                    }
                    if let Some(x) = tr.last() {
                        line = Some(format!("{} trades, last {:?} {} @ {}", tr.len(), x.side, x.sz, x.px));
                    }
                }
                Event::Snapshot(TopicData::Bars(b)) => {
                    line = b.last().map(|x| format!("{} bars, last ts {} close {}", b.len(), x.ts, x.close));
                }
                Event::Delta(TopicDelta::Bar(d)) => {
                    let (kind, b) = match d {
                        BarDelta::Forming(b) => ("forming", b),
                        BarDelta::Closed(b) => ("CLOSED", b),
                    };
                    line = Some(format!(
                        "{kind} ts {} o {} h {} l {} c {} v {:.4}",
                        b.ts, b.open, b.high, b.low, b.close, b.volume
                    ));
                }
                Event::Resync(r) => {
                    t.resyncs += 1;
                    line = Some(format!("resync ({r:?})"));
                }
                Event::Status(s) => {
                    t.statuses.push(format!("{name}: {s:?}"));
                    line = Some(format!("status {s:?}"));
                }
                Event::Reconnecting => line = Some("reconnecting".into()),
                Event::Resubscribing => line = Some("resubscribing".into()),
            }
        }
        let kind = match &u.event {
            Event::Snapshot(_) => "snapshot",
            Event::Delta(_) => "delta",
            _ => "event",
        };
        let important =
            !matches!(u.event, Event::Delta(TopicDelta::Book(_) | TopicDelta::Quotes(_) | TopicDelta::Trades(_)))
                || last_print.elapsed() >= Duration::from_secs(1);
        if print
            && important
            && let Some(l) = line
        {
            println!("{name} {kind}: {l}");
            last_print = Instant::now();
        }
    }
}

fn http_base(ws: &str) -> String {
    let u = normalize_url(ws);
    let u = u.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
    u.strip_suffix("/ws").unwrap_or(&u).to_string()
}

async fn print_stats(url: &str) -> Result<()> {
    let body = reqwest::get(format!("{}/stats", http_base(url))).await?.text().await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let u = &v["upstream"];
    println!(
        "server: upstream {}/{} connections up, {} of {} subscriptions {} by kind {}; clients {}, client subscriptions {}; topics {}",
        u["connections_up"],
        u["max_connections"],
        u["subscriptions"],
        u["max_subscriptions"],
        u["per_connection"],
        u["by_kind"],
        v["clients"],
        v["client_subscriptions"],
        v["topics"],
    );
    let keys: Vec<&str> = u["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k.as_str())
        .filter(|k| !k.starts_with("allMids"))
        .collect();
    println!("server: on-demand upstream subscriptions: {keys:?}");
    println!(
        "server: sent upstream {} messages in the last minute (budget {}), REST weight {} (budget {})",
        u["messages_sent_last_min"], u["messages_per_min"], u["rest_weight_last_min"], u["rest_weight_per_min"]
    );
    for (topic, q) in v["latency_us"].as_object().into_iter().flatten() {
        if q["window"].as_u64().unwrap_or(0) > 0 {
            println!(
                "server: added latency {topic}: n={} p50={}us p90={}us p99={}us max={}us",
                q["window"], q["p50_us"], q["p90_us"], q["p99_us"], q["max_us"]
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = parse()?;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let enc = if a.json { Encoding::Json } else { Encoding::Postcard };
    match a.command.as_str() {
        "stats" => print_stats(&a.url).await,
        "markets" => {
            let c = Client::connect(&a.url, enc, "tr-cli");
            let Ok(RpcResult::Markets(m)) = c.request(Rpc::Markets).await.map_err(anyhow::Error::from) else {
                bail!("no market list")
            };
            let f = a.filter.unwrap_or_default().to_lowercase();
            let shown: Vec<_> = m.iter().filter(|x| x.key.as_str().to_lowercase().contains(&f)).collect();
            for x in &shown {
                println!("{:<8} {:?}  {}", if x.venue.is_empty() { "core" } else { &x.venue }, x.kind, x.key);
            }
            println!("{} of {} markets", shown.len(), m.len());
            Ok(())
        }
        "watch" | "load" => {
            if a.topics.is_empty() {
                bail!("name at least one topic (--quotes, --book, --trades, --bars)");
            }
            let n = if a.command == "watch" { 1 } else { a.clients.max(1) };
            let tally = Arc::new(Mutex::new(Tally::default()));
            let mut clients = Vec::new();
            let t0 = Instant::now();
            for _ in 0..n {
                let c = Client::connect(&a.url, enc, "tr-cli");
                if !c.wait_up(Duration::from_secs(10)).await {
                    bail!("could not reach the server: {:?}", c.state());
                }
                clients.push(c);
            }
            println!(
                "{n} client connection(s) up in {:?}; subscribing to {}",
                t0.elapsed(),
                a.topics.iter().map(topic_name).collect::<Vec<_>>().join(", ")
            );
            let until = Instant::now() + Duration::from_secs(a.secs);
            let mut tasks = Vec::new();
            for (i, c) in clients.iter().enumerate() {
                for t in &a.topics {
                    let name = if n == 1 { topic_name(t) } else { format!("{} (all clients)", topic_name(t)) };
                    tasks.push(tokio::spawn(drain(
                        c.subscribe(t.clone()),
                        name,
                        tally.clone(),
                        until,
                        i == 0 && n == 1,
                    )));
                }
            }
            if a.command == "load" {
                tokio::time::sleep(Duration::from_secs(a.secs.min(5))).await;
                let _ = print_stats(&a.url).await;
            }
            for t in tasks {
                let _ = t.await;
            }
            println!("after {}s across {n} client(s):", a.secs);
            tally.lock().unwrap().report();
            Ok(())
        }
        other => bail!("unknown command {other}"),
    }
}
