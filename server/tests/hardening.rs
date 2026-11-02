//! The gateway's defences against clients: the operator token on `/stats` and `/metrics`, the
//! cap on connections from one address, and closing clients that keep flooding it.
//!
//! None of this needs market data, so the upstream is a port nothing listens on.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tr_gateway::Config;
use tr_proto::{ClientInfo, ClientMsg, Encoding, PROTOCOL_VERSION, ServerMsg};

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const WAIT: Duration = Duration::from_secs(5);

async fn gateway(tweak: impl FnOnce(&mut Config)) -> tr_gateway::Gateway {
    let mut cfg = Config { bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), ..Default::default() };
    // Nothing listens on the discard port: the upstream stays down, which these tests don't mind.
    cfg.hl.pool.ws_url = "ws://127.0.0.1:9/ws".into();
    cfg.hl.info_url = "http://127.0.0.1:9/info".into();
    tweak(&mut cfg);
    tr_gateway::start(cfg).await.unwrap()
}

async fn get(gw: &tr_gateway::Gateway, path: &str, auth: Option<&str>) -> u16 {
    let mut req = reqwest::Client::new().get(format!("http://{}{path}", gw.addr));
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    req.send().await.expect("the gateway answers").status().as_u16()
}

/// Open a websocket, optionally claiming a client address in `x-forwarded-for`. `Err` carries
/// the HTTP status of a refused upgrade.
async fn connect(gw: &tr_gateway::Gateway, forwarded: Option<&str>) -> Result<Ws, u16> {
    let mut req = format!("ws://{}/ws", gw.addr).into_client_request().unwrap();
    if let Some(f) = forwarded {
        req.headers_mut().insert("x-forwarded-for", f.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
        Err(e) => panic!("connect: {e}"),
    }
}

fn text(m: &ClientMsg) -> Message {
    Message::text(serde_json::to_string(m).unwrap())
}

async fn hello(ws: &mut Ws) {
    let info = ClientInfo { name: "hardening-test".into(), version: "0".into() };
    let m = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        encoding: Encoding::Json,
        client: info,
        token: None,
        resume: None,
    };
    ws.send(text(&m)).await.unwrap();
    let welcome = tokio::time::timeout(WAIT, ws.next()).await.expect("a welcome").unwrap().unwrap();
    let w: ServerMsg = serde_json::from_str(welcome.to_text().unwrap()).unwrap();
    assert!(matches!(w, ServerMsg::Welcome { .. }), "{w:?}");
}

/// Wait until the gateway has `n` clients, so a closed socket's slot is free again.
async fn clients(gw: &tr_gateway::Gateway, n: i64) {
    let t0 = tokio::time::Instant::now();
    while gw.hub().metrics.clients.load(std::sync::atomic::Ordering::Relaxed) != n {
        assert!(t0.elapsed() < WAIT, "never got to {n} clients");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn stats_and_metrics_stay_public_without_a_token() {
    let gw = gateway(|_| {}).await;
    for path in ["/healthz", "/stats", "/metrics"] {
        assert_eq!(get(&gw, path, None).await, 200, "{path}");
    }
    gw.shutdown().await;
}

#[tokio::test]
async fn a_token_hides_stats_and_metrics_from_everyone_without_it() {
    let gw = gateway(|c| c.metrics_token = Some(tr_gateway::access::Secret::new("t0ken"))).await;
    for path in ["/stats", "/metrics"] {
        assert_eq!(get(&gw, path, None).await, 404, "{path} without a token");
        assert_eq!(get(&gw, path, Some("Bearer wrong")).await, 404, "{path} with a wrong token");
        assert_eq!(get(&gw, path, Some("t0ken")).await, 404, "{path} without the Bearer scheme");
        assert_eq!(get(&gw, path, Some("Bearer t0ken")).await, 200, "{path} with the token");
        assert_eq!(get(&gw, path, Some("bearer t0ken")).await, 200, "{path}: the scheme ignores case");
    }
    assert_eq!(get(&gw, "/healthz", None).await, 200, "health checks need no token");
    let mut ws = connect(&gw, None).await.expect("clients need no token");
    hello(&mut ws).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn one_address_gets_a_capped_number_of_connections() {
    let gw = gateway(|c| c.max_clients_per_ip = 2).await;
    let mut a = connect(&gw, None).await.unwrap();
    let mut b = connect(&gw, None).await.unwrap();
    hello(&mut a).await;
    hello(&mut b).await;
    assert_eq!(connect(&gw, None).await.err(), Some(429), "a third from one address is refused");
    // Without a trusted header configured, a claimed address changes nothing.
    assert_eq!(connect(&gw, Some("198.51.100.7")).await.err(), Some(429));
    a.close(None).await.unwrap();
    drop(a);
    clients(&gw, 1).await;
    let mut c = connect(&gw, None).await.expect("a closed connection frees its slot");
    hello(&mut c).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn behind_an_edge_the_cap_counts_the_forwarded_address() {
    let gw = gateway(|c| {
        c.max_clients_per_ip = 1;
        c.client_ip_header = Some("x-forwarded-for".parse().unwrap());
    })
    .await;
    let _a = connect(&gw, Some("198.51.100.7")).await.unwrap();
    // The edge appends the address it saw; only that last entry counts.
    assert_eq!(connect(&gw, Some("203.0.113.1, 198.51.100.7")).await.err(), Some(429));
    let _b = connect(&gw, Some("198.51.100.7, 198.51.100.8")).await.expect("another address");
    let _c = connect(&gw, Some("198.51.100.9")).await.expect("another address");
    gw.shutdown().await;
}

/// A client over the message budget gets `RateLimited` for each extra message. One that keeps
/// going after a budget's worth of those is closed, rather than answered forever.
#[tokio::test]
async fn clients_that_keep_flooding_are_closed() {
    let gw = gateway(|c| c.session.max_messages_per_10s = 5).await;
    let mut ws = connect(&gw, None).await.unwrap();
    hello(&mut ws).await;
    // Five within the budget, five answered with RateLimited, and the one that closes it. No
    // more: a server closing on unread data resets the connection, losing what it sent.
    for t in 0..11 {
        ws.send(text(&ClientMsg::Ping { t })).await.unwrap();
    }
    let mut limited = 0;
    let closed = tokio::time::timeout(WAIT, async {
        while let Some(m) = ws.next().await {
            match m {
                Ok(Message::Text(t)) if t.contains("RateLimited") => limited += 1,
                Ok(Message::Close(f)) => return f.map(|f| u16::from(f.code)),
                Ok(_) => {}
                Err(e) => panic!("{e}"),
            }
        }
        None
    })
    .await;
    assert!(closed.is_ok(), "still open after 11 messages against a budget of 5 ({limited} RateLimited)");
    assert_eq!(limited, 6, "five warnings, then the reason for closing");
    assert_eq!(closed.unwrap(), Some(1008), "closed as a policy violation");
    gw.shutdown().await;
}
