//! Hyperliquid **testnet** work for the background worker: the user stream, `/info` reads,
//! the agent key in the OS secret store, and signing and sending actions.
//!
//! Every action is signed here, right after the worker reads the market again (the testnet
//! mid and the account's leverage), so `tr_hl::trader` checks its caps against the market at
//! the moment of signing, not the one the confirmation showed. User data comes straight from
//! Hyperliquid's websocket, never through Terminal's server.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tr_broker::secrets::AgentKeyStore;
use tr_data::rt;
use tr_hl::assets::{Asset, AssetRegistry};
use tr_hl::client::{Client, ExchangeResponse, SendError};
use tr_hl::model::{AssetData, ExtraAgent};
use tr_hl::nonce::{NonceManager, now_ms};
use tr_hl::stream::{PING, UserState, subscriptions};
use tr_hl::trader::{self, MarketCtx, OrderPlan};
use tr_hl::{AgentKey, SigningKey};

use crate::prefs::HlAgent;

/// The stream generation that may run; a new `Stream` or `StopStream` ends older ones.
static STREAM: AtomicU64 = AtomicU64::new(0);
/// The newest kill switch seen. An order confirmed before it, not yet signed, is refused.
static LATEST_KILL: AtomicU64 = AtomicU64::new(0);

/// Signed actions run one at a time, in the order they were sent, so a kill switch waits for
/// an order already being signed and then reads the open orders after it.
fn act_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub enum HlRequest {
    /// Asset ids and rules, read fresh from `perpDexs` and `meta`.
    Registry,
    /// Start the user stream for `user` (replacing any other).
    Stream {
        generation: u64,
        user: String,
    },
    StopStream,
    /// The testnet mid and the account's leverage on one market (the ticket).
    Market {
        user: String,
        coin: String,
        dex: String,
    },
    /// `userRole` of the account and agent, and the account's `extraAgents`.
    Roles {
        master: String,
        agent: Option<String>,
    },
    /// Load the agent key for `account` (also says whether the store works at all).
    LoadKey {
        store: AgentKeyStore,
        account: String,
    },
    /// Check the key store, post a signed `approveAgent` for `master`, and keep the agent key
    /// only if Hyperliquid accepts it.
    Approve {
        store: AgentKeyStore,
        master: String,
        key: Arc<AgentKey>,
        body: Value,
        agent: HlAgent,
    },
    /// Forget the local key, then post the signed revoke (if the main wallet signed one).
    Revoke {
        store: AgentKeyStore,
        account: String,
        body: Option<Value>,
    },
    /// Sign and send. `kill_epoch` is the kill switch count when the user confirmed.
    Act {
        id: u64,
        key: Arc<AgentKey>,
        nonces: Arc<NonceManager>,
        user: String,
        act: HlAct,
        kill_epoch: u64,
    },
    /// Whether an order whose send was uncertain reached Hyperliquid.
    OrderStatus {
        user: String,
        cloid: String,
    },
}

pub enum HlAct {
    Order(Box<OrderPlan>),
    Modify {
        oid: u64,
        plan: Box<OrderPlan>,
    },
    Cancel {
        asset: Asset,
        oid: u64,
    },
    /// The kill switch: read every open order on every dex now, then cancel them.
    CancelAll {
        registry: Arc<AssetRegistry>,
    },
    Leverage {
        asset: Asset,
        cross: bool,
        leverage: u32,
    },
    ScheduleCancel(Option<u64>),
}

impl HlAct {
    pub fn label(&self) -> String {
        match self {
            HlAct::Order(p) => format!(
                "{} {} {}",
                if p.intent().is_buy { "BUY" } else { "SELL" },
                tr_hl::rounding::to_wire(p.size()).unwrap_or_default(),
                p.asset().coin
            ),
            HlAct::Modify { oid, plan } => format!("modify #{oid} {}", plan.asset().coin),
            HlAct::Cancel { asset, oid } => format!("cancel {} #{oid}", asset.coin),
            HlAct::CancelAll { .. } => "cancel all".into(),
            HlAct::Leverage { asset, cross, leverage } => {
                format!("{} leverage {leverage}× {}", asset.coin, if *cross { "cross" } else { "isolated" })
            }
            HlAct::ScheduleCancel(Some(_)) => "dead man's switch".into(),
            HlAct::ScheduleCancel(None) => "dead man's switch off".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HlRoles {
    pub master: String,
    pub agent: Option<String>,
    pub agents: Vec<ExtraAgent>,
}

/// What an action came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Hyperliquid answered (it may still have rejected the order: see the statuses).
    Answered(ExchangeResponse),
    /// Refused before anything was signed (a cap, or the market).
    Refused(String),
    Failed(SendError),
}

pub enum HlResponse {
    Registry(Result<Arc<AssetRegistry>, String>),
    State {
        generation: u64,
        state: Box<UserState>,
    },
    StreamStatus {
        generation: u64,
        connected: bool,
        status: String,
    },
    Market {
        coin: String,
        mid: Option<f64>,
        data: Result<Option<AssetData>, String>,
    },
    Roles(Result<HlRoles, String>),
    /// `Err`: the store can't be used at all (trading stays off).
    KeyLoaded {
        account: String,
        result: Result<Option<Arc<AgentKey>>, String>,
    },
    /// `master` is the account the approval was for.
    Approved {
        master: String,
        agent: HlAgent,
        key: Arc<AgentKey>,
        result: Result<String, String>,
        saved: Result<(), String>,
    },
    Revoked(Result<String, String>),
    Done {
        id: u64,
        label: String,
        outcome: Outcome,
    },
    /// `Ok(None)`: Hyperliquid has never seen this client order id.
    OrderStatus {
        cloid: String,
        result: Result<Option<String>, String>,
    },
}

/// Run one request; answers go through `send`.
pub async fn handle(req: HlRequest, send: impl Fn(HlResponse) + Clone + 'static) {
    let client = match Client::new() {
        Ok(c) => c,
        Err(e) => return tracing::warn!("hyperliquid client: {e:#}"),
    };
    match req {
        HlRequest::Registry => {
            send(HlResponse::Registry(client.registry().await.map(Arc::new).map_err(|e| format!("{e:#}"))))
        }
        HlRequest::Stream { generation, user } => {
            STREAM.store(generation, Ordering::SeqCst);
            stream(generation, user, send).await;
        }
        HlRequest::StopStream => {
            STREAM.fetch_add(1 << 32, Ordering::SeqCst);
        }
        HlRequest::Market { user, coin, dex } => {
            let mid = client.mids(&dex).await.ok().and_then(|m| m.get(&coin).copied());
            let data = match tr_hl::key::parse_address(&user) {
                Ok(u) => client.asset_data(&u, &coin).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            send(HlResponse::Market { coin, mid, data });
        }
        HlRequest::Roles { master, agent } => {
            let roles = async {
                let m = tr_hl::key::parse_address(&master)?;
                let mut out = HlRoles {
                    master: client.user_role(&m).await?,
                    agent: None,
                    agents: client.extra_agents(&m).await?,
                };
                if let Some(a) = agent {
                    out.agent = Some(client.user_role(&tr_hl::key::parse_address(&a)?).await?);
                }
                anyhow::Ok(out)
            };
            send(HlResponse::Roles(roles.await.map_err(|e| format!("{e:#}"))));
        }
        HlRequest::LoadKey { store, account } => {
            let result = rt::blocking(move || {
                store.check()?;
                match store.load(&account) {
                    Ok(Some(hex)) => AgentKey::from_hex(&hex).map(|k| Some(Arc::new(k))).map_err(|e| e.to_string()),
                    Ok(None) => Ok(None),
                    Err(e) => Err(format!("{e:#}")),
                }
                .map(|k| (account, k))
            })
            .await
            .unwrap_or_else(|| Err("the key store task failed".into()));
            match result {
                Ok((account, key)) => send(HlResponse::KeyLoaded { account, result: Ok(key) }),
                Err(e) => send(HlResponse::KeyLoaded { account: String::new(), result: Err(e) }),
            }
        }
        HlRequest::Approve { store, master, key, body, agent } => {
            // The keyring may be slow or locked: never ask it from the UI thread.
            let checked = {
                let store = store.clone();
                rt::blocking(move || store.check()).await.unwrap_or_else(|| Err("the key store task failed".into()))
            };
            if let Err(e) = checked {
                let result = Err(format!("this device can't keep an agent key: {e}"));
                return send(HlResponse::Approved { master, agent, key, result, saved: Ok(()) });
            }
            let r = client.send_user_signed(&body).await;
            let result = match &r {
                Ok(resp) if resp.ok && resp.first_error().is_none() => Ok(resp.summary()),
                Ok(resp) => Err(resp.first_error().unwrap_or("rejected").to_string()),
                Err(e) => Err(e.to_string()),
            };
            // Keep the key only for an agent Hyperliquid accepted.
            let saved = if result.is_ok() {
                let (k, account) = (key.clone(), crate::app::hl_store_account(&master));
                rt::blocking(move || store.save(&account, &k.to_hex()).map_err(|e| format!("{e:#}")))
                    .await
                    .unwrap_or_else(|| Err("the key store task failed".into()))
            } else {
                Ok(())
            };
            send(HlResponse::Approved { master, agent, key, result, saved });
        }
        HlRequest::Revoke { store, account, body } => {
            let deleted = rt::blocking(move || store.delete(&account).map_err(|e| format!("{e:#}")))
                .await
                .unwrap_or_else(|| Err("the key store task failed".into()));
            let result = match (deleted, body) {
                (Err(e), _) => Err(format!("could not delete the local key: {e}")),
                (Ok(()), None) => Ok("Agent key deleted from this device.".into()),
                (Ok(()), Some(b)) => match client.send_user_signed(&b).await {
                    Ok(resp) if resp.ok && resp.first_error().is_none() => {
                        Ok(format!("Agent key deleted and the agent replaced on Hyperliquid ({}).", resp.summary()))
                    }
                    Ok(resp) => Err(format!(
                        "Agent key deleted from this device; Hyperliquid refused the revoke: {}",
                        resp.first_error().unwrap_or("rejected")
                    )),
                    Err(e) => Err(format!("Agent key deleted from this device; the revoke wasn't confirmed: {e}")),
                },
            };
            send(HlResponse::Revoked(result));
        }
        HlRequest::Act { id, key, nonces, user, act, kill_epoch } => {
            let mut label = act.label();
            if matches!(act, HlAct::CancelAll { .. }) {
                LATEST_KILL.fetch_max(kill_epoch, Ordering::SeqCst);
            }
            let _turn = act_lock().lock().await;
            let placing = matches!(act, HlAct::Order(_) | HlAct::Modify { .. });
            let outcome = if placing && LATEST_KILL.load(Ordering::SeqCst) > kill_epoch {
                Outcome::Refused("KILL was pressed after this was confirmed, so it was not sent.".into())
            } else {
                act_now(&client, &key, &nonces, &user, act, &mut label).await
            };
            send(HlResponse::Done { id, label, outcome });
        }
        HlRequest::OrderStatus { user, cloid } => {
            let result = match tr_hl::key::parse_address(&user) {
                Ok(u) => client.order_status(&u, &cloid).await.map_err(|e| format!("{e:#}")),
                Err(e) => Err(e.to_string()),
            };
            send(HlResponse::OrderStatus { cloid, result });
        }
    }
}

/// The market for `coin` now: testnet mid and the account's leverage.
async fn market_now(client: &Client, user: &str, asset: &Asset) -> Result<MarketCtx, String> {
    let u = tr_hl::key::parse_address(user).map_err(|e| e.to_string())?;
    let (mids, data) = futures::join!(client.mids(&asset.dex), client.asset_data(&u, &asset.coin));
    let mid = mids.map_err(|e| format!("{e:#}"))?.get(&asset.coin).copied();
    let leverage = data.map_err(|e| format!("{e:#}"))?.and_then(|d| d.leverage);
    Ok(MarketCtx { mid, leverage })
}

/// Every open order on the loaded dexes, read now (the kill switch never trusts a stale list).
async fn open_orders_now(client: &Client, user: &str, reg: &AssetRegistry) -> Result<Vec<(String, u64)>, String> {
    let u = tr_hl::key::parse_address(user).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for dex in reg.dexes() {
        let orders = client.open_orders(&u, dex).await.map_err(|e| format!("{e:#}"))?;
        out.extend(orders.into_iter().map(|o| (o.coin, o.oid)));
    }
    Ok(out)
}

async fn act_now(
    client: &Client,
    key: &AgentKey,
    nonces: &NonceManager,
    user: &str,
    act: HlAct,
    label: &mut String,
) -> Outcome {
    let signed = match act {
        HlAct::Order(plan) => match market_now(client, user, plan.asset()).await {
            Ok(ctx) => trader::sign_order(key, nonces, &plan, &ctx, now_ms()).map(|s| vec![s]),
            Err(e) => return Outcome::Refused(format!("Could not read the market before signing: {e}")),
        },
        HlAct::Modify { oid, plan } => match market_now(client, user, plan.asset()).await {
            Ok(ctx) => trader::sign_modify(key, nonces, oid, &plan, &ctx, now_ms()).map(|s| vec![s]),
            Err(e) => return Outcome::Refused(format!("Could not read the market before signing: {e}")),
        },
        HlAct::Cancel { asset, oid } => trader::sign_cancel(key, nonces, &asset, oid, now_ms()).map(|s| vec![s]),
        HlAct::CancelAll { registry } => return kill_all(client, key, nonces, user, &registry, label).await,
        HlAct::Leverage { asset, cross, leverage } => {
            trader::sign_update_leverage(key, nonces, &asset, cross, leverage, now_ms()).map(|s| vec![s])
        }
        HlAct::ScheduleCancel(at) => trader::sign_schedule_cancel(key, nonces, at, now_ms()).map(|s| vec![s]),
    };
    let signed = match signed {
        Ok(s) => s,
        Err(refusal) => return Outcome::Refused(refusal.to_string()),
    };
    tracing::debug!(
        "hyperliquid testnet: {} action(s) signed by agent {}",
        signed.len(),
        tr_hl::mask_address(&key.address())
    );
    let mut last = None;
    for s in &signed {
        match client.send(s).await {
            Ok(r) if r.ok && r.first_error().is_none() => last = Some(Outcome::Answered(r)),
            Ok(r) => return Outcome::Answered(r),
            Err(e) => return Outcome::Failed(e),
        }
    }
    last.unwrap_or_else(|| Outcome::Refused("Nothing to send.".into()))
}

/// The kill switch: read every open order now, cancel them, and read again, up to three
/// rounds (an order placed elsewhere meanwhile is caught too). Reports what is left.
async fn kill_all(
    client: &Client,
    key: &AgentKey,
    nonces: &NonceManager,
    user: &str,
    registry: &AssetRegistry,
    label: &mut String,
) -> Outcome {
    let mut cancelled = 0;
    let mut skipped: Vec<String> = Vec::new();
    let mut last = None;
    for _ in 0..3 {
        let orders = match open_orders_now(client, user, registry).await {
            Ok(o) => o,
            Err(e) => return Outcome::Refused(format!("Could not read the open orders: {e}")),
        };
        let open: Vec<(String, u64)> = orders.into_iter().filter(|(c, _)| !skipped.contains(c)).collect();
        if open.is_empty() {
            break;
        }
        let (signed, unknown) = match trader::sign_cancel_all(key, nonces, registry, &open, now_ms()) {
            Ok(s) => s,
            Err(e) => return Outcome::Refused(e.to_string()),
        };
        cancelled += open.iter().filter(|(c, _)| !unknown.contains(c)).count();
        skipped.extend(unknown);
        if signed.is_empty() {
            break;
        }
        for s in &signed {
            match client.send(s).await {
                Ok(r) if r.ok && r.first_error().is_none() => last = Some(Outcome::Answered(r)),
                Ok(r) => return Outcome::Answered(r),
                Err(e) => return Outcome::Failed(e),
            }
        }
    }
    let left = open_orders_now(client, user, registry).await.map(|o| o.len());
    *label = match (cancelled, &left) {
        (0, Ok(0)) => "cancel all (no open orders)".into(),
        (n, Ok(0)) => format!("cancel all ({n} cancelled, none left)"),
        (n, Ok(k)) => format!("cancel all ({n} cancelled, {k} STILL OPEN: check ORD)"),
        (n, Err(_)) => format!("cancel all ({n} cancelled; couldn't read what's left)"),
    };
    if !skipped.is_empty() {
        *label += &format!("; not cancelled, unknown market: {}", skipped.join(", "));
    }
    last.unwrap_or_else(|| {
        Outcome::Answered(ExchangeResponse { ok: true, kind: Some("cancel".into()), statuses: Vec::new(), error: None })
    })
}

/// The user stream: subscribe, ping, and hand the UI a fresh [`UserState`] at most four
/// times a second. Reconnects with back-off; stops when a newer stream (or none) is wanted.
async fn stream(generation: u64, user: String, send: impl Fn(HlResponse) + Clone + 'static) {
    let alive = || STREAM.load(Ordering::SeqCst) == generation;
    let status = |connected: bool, s: String| send(HlResponse::StreamStatus { generation, connected, status: s });
    let mut state = UserState::default();
    let mut backoff = Duration::from_secs(1);
    while alive() {
        status(false, "Connecting to Hyperliquid testnet…".into());
        match tr_data::ws::Socket::connect(tr_hl::network::WS_URL).await {
            Ok(mut sock) => {
                let mut ok = true;
                for sub in subscriptions(&user, ["", "xyz"], &[]) {
                    ok &= sock.send(sub).await.is_ok();
                }
                if ok {
                    status(true, "Live from Hyperliquid testnet".into());
                    backoff = Duration::from_secs(1);
                }
                let mut ping = rt::interval(Duration::from_secs(30));
                let mut flush = rt::interval(Duration::from_millis(250));
                let mut dirty = false;
                while ok {
                    tokio::select! {
                        m = sock.next() => match m {
                            Some(Ok(text)) => dirty |= !state.apply(&user, &text).is_empty(),
                            Some(Err(e)) => { tracing::info!("hyperliquid stream: {e:#}"); break }
                            None => break,
                        },
                        _ = ping.tick() => ok = sock.send(PING.into()).await.is_ok(),
                        _ = flush.tick() => {
                            if !alive() {
                                sock.close().await;
                                return;
                            }
                            if std::mem::take(&mut dirty) {
                                send(HlResponse::State { generation, state: Box::new(state.clone()) });
                            }
                        }
                    }
                }
                if dirty {
                    send(HlResponse::State { generation, state: Box::new(state.clone()) });
                }
                status(false, "Hyperliquid testnet stream dropped; reconnecting…".into());
            }
            Err(e) => status(false, format!("Hyperliquid testnet unreachable ({e:#}); retrying…")),
        }
        rt::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
