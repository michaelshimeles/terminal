//! Hyperliquid **testnet** work for the background worker: the user stream, `/info` reads,
//! the agent key in the OS secret store, and signing and sending actions.
//!
//! Every action is signed here, right after the worker reads the market again (the testnet
//! mid and the account's leverage), so `tr_hl::trader` checks its caps against the market at
//! the moment of signing, not the one the confirmation showed. User data comes straight from
//! Hyperliquid's websocket, never through Terminal's server.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tr_broker::secrets::AgentKeyStore;
use tr_data::rt;
use tr_hl::assets::{Asset, AssetRegistry};
use tr_hl::caps::Refusal;
use tr_hl::client::{Client, ExchangeResponse, SendError};
use tr_hl::model::{AssetData, ExtraAgent};
use tr_hl::nonce::{NonceManager, now_ms};
use tr_hl::stream::{PING, UserState, subscriptions};
use tr_hl::trader::{self, MarketCtx, OrderPlan, Signed};
use tr_hl::{AgentKey, SigningKey};

use crate::prefs::HlAgent;

/// The stream generation that may run; a new `Stream` or `StopStream` ends older ones.
static STREAM: AtomicU64 = AtomicU64::new(0);

/// How long past its `expiresAfter` an order with no clear answer still counts as possibly on
/// Hyperliquid, since the clocks may differ. The app's own check allows the same.
pub const LAND_MARGIN_MS: u64 = 5_000;

const KILLED: &str = "KILL was pressed after this was confirmed, so it was not sent.";

/// The longest KILL waits for an order that was being sent before it sweeps again. The
/// order's requests have deadlines and Hyperliquid drops it after its `expiresAfter`, so this
/// cap only matters if something else goes wrong.
const FOLLOW_UP_MAX: Duration = Duration::from_millis(
    trader::EXPIRES_AFTER_MS + LAND_MARGIN_MS + 2 * tr_hl::client::REQUEST_TIMEOUT.as_millis() as u64,
);

/// When signed actions may run. Orders, modifies and leverage changes take turns, one at a
/// time in the order sent. Cancels never wait for them, and dead man's switch changes take
/// turns of their own. KILL never waits either. It cancels everything open at once, refuses
/// any order confirmed before it that hasn't been signed, even one still reading the market,
/// and sweeps again once an order that was already being sent can no longer land.
pub struct Turns {
    turn: tokio::sync::Mutex<()>,
    /// The newest kill switch seen. `place` refuses an order confirmed before it that isn't
    /// signed yet.
    kills: tokio::sync::watch::Sender<u64>,
    /// An order holding the turn has been signed and is being sent.
    sending: std::sync::atomic::AtomicBool,
    /// An order sent without a clear answer may still land on Hyperliquid until then (ms).
    may_land_until: AtomicU64,
    /// Dead man's switch changes go out one at a time.
    schedule: tokio::sync::Mutex<Schedule>,
}

#[derive(Default)]
struct Schedule {
    /// The app's id for the newest change sent.
    newest: u64,
    /// A change sent without a clear answer may still land until then (ms).
    may_land_until: u64,
}

impl Default for Turns {
    fn default() -> Self {
        Self {
            turn: tokio::sync::Mutex::new(()),
            kills: tokio::sync::watch::Sender::new(0),
            sending: Default::default(),
            may_land_until: AtomicU64::new(0),
            schedule: Default::default(),
        }
    }
}

/// Clears [`Turns::sending`] however the send ends.
struct Sending<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Turns {
    fn global() -> &'static Turns {
        static TURNS: std::sync::OnceLock<Turns> = std::sync::OnceLock::new();
        TURNS.get_or_init(Turns::default)
    }

    fn latest_kill(&self) -> u64 {
        *self.kills.borrow()
    }

    /// Runs an order, modify or leverage change. It waits for the turn and `read`s the market,
    /// then `go` signs and sends. `kill_epoch` is the kill switch count when an order or modify
    /// was confirmed. It is `None` for a leverage change, which KILL doesn't stop. If the send
    /// got no clear answer, `go` also returns the time until which the order may still land.
    pub async fn place<T, G, GF>(
        &self,
        kill_epoch: Option<u64>,
        read: impl Future<Output = Result<T, String>>,
        go: G,
    ) -> Outcome
    where
        G: FnOnce(T) -> GF,
        GF: Future<Output = (Outcome, Option<u64>)>,
    {
        let killed = |k: &u64| kill_epoch.is_some_and(|e| *k > e);
        let mut kills = self.kills.subscribe();
        // A KILL that comes while this waits for its turn or the market ends it unsigned.
        let _turn = tokio::select! {
            biased;
            _ = kills.wait_for(killed) => return Outcome::Refused(KILLED.into()),
            turn = self.turn.lock() => turn,
        };
        let ctx = tokio::select! {
            biased;
            _ = kills.wait_for(killed) => return Outcome::Refused(KILLED.into()),
            ctx = read => ctx,
        };
        let ctx = match ctx {
            Ok(c) => c,
            Err(e) => return Outcome::Refused(format!("Could not read the market before signing: {e}")),
        };
        // Marked before the last check, so a KILL that comes after it knows to sweep again.
        let _sending = kill_epoch.map(|_| {
            self.sending.store(true, Ordering::SeqCst);
            Sending(&self.sending)
        });
        if killed(&self.latest_kill()) {
            return Outcome::Refused(KILLED.into());
        }
        let (outcome, may_land_until) = go(ctx).await;
        if let Some(t) = may_land_until {
            self.may_land_until.fetch_max(t, Ordering::SeqCst);
        }
        outcome
    }

    /// A cancel. It only takes risk off, so it never waits for an order. Hyperliquid accepts
    /// nonces that arrive out of order.
    pub async fn cancel(&self, go: impl Future<Output = Outcome>) -> Outcome {
        go.await
    }

    /// A dead man's switch change (`scheduleCancel`), where `seq` is the app's id for it.
    /// Changes go out one at a time, in the order the app made them, and never wait for an
    /// order. The worker drops one that reaches it after a newer one went out (`None`).
    /// After a change whose send got no clear answer, the next waits until that one can no
    /// longer land, so an old clear never removes a newer schedule.
    pub async fn schedule<GF>(&self, seq: u64, go: impl FnOnce() -> GF) -> Option<Outcome>
    where
        GF: Future<Output = (Outcome, Option<u64>)>,
    {
        let mut s = self.schedule.lock().await;
        if seq <= s.newest {
            return None;
        }
        s.newest = seq;
        let wait = s.may_land_until.saturating_sub(now_ms());
        if wait > 0 {
            rt::sleep(Duration::from_millis(wait).min(FOLLOW_UP_MAX)).await;
        }
        let (outcome, may_land_until) = go().await;
        if let Some(t) = may_land_until {
            s.may_land_until = s.may_land_until.max(t);
        }
        Some(outcome)
    }

    /// The kill switch. `sweep` reads every open order and cancels it; `report` gets each
    /// sweep's result. The first sweep runs at once. If an order was already being sent, KILL
    /// takes the turn once that order has its answer, or once its `expiresAfter` has passed if
    /// the send got no clear answer, and sweeps again. It keeps the turn through that sweep,
    /// so it never cancels an order confirmed after it.
    pub async fn kill<S, SF>(&self, epoch: u64, sweep: S, report: impl Fn(String, Outcome))
    where
        S: Fn() -> SF,
        SF: Future<Output = (String, Outcome)>,
    {
        self.kills.send_if_modified(|k| {
            let newer = epoch > *k;
            if newer {
                *k = epoch;
            }
            newer
        });
        let in_flight = self.sending.load(Ordering::SeqCst) || self.may_land_until.load(Ordering::SeqCst) > now_ms();
        let (label, outcome) = sweep().await;
        if !in_flight {
            return report(label, outcome);
        }
        report(format!("{label}; an order was still being sent, checking again after it"), outcome);
        let settled = async {
            let turn = self.turn.lock().await;
            loop {
                let (until, now) = (self.may_land_until.load(Ordering::SeqCst), now_ms());
                if until <= now {
                    break turn;
                }
                rt::sleep(Duration::from_millis(until - now)).await;
            }
        };
        let _turn = rt::timeout(FOLLOW_UP_MAX, settled).await.ok();
        let (label, outcome) = sweep().await;
        report(format!("{label}, after the order that was being sent"), outcome);
    }
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
            let turns = Turns::global();
            let label = act.label();
            let done = |label: String, outcome: Outcome| send(HlResponse::Done { id, label, outcome });
            let (c, k, n) = (&client, &*key, &*nonces);
            let outcome = match act {
                HlAct::Order(plan) => {
                    let read = market_now(c, &user, plan.asset());
                    let go = |ctx: MarketCtx| {
                        send_signed(c, k, trader::sign_order(k, n, &plan, &ctx, now_ms()).map(|s| vec![s]))
                    };
                    turns.place(Some(kill_epoch), read, go).await
                }
                HlAct::Modify { oid, plan } => {
                    let read = market_now(c, &user, plan.asset());
                    let go = |ctx: MarketCtx| {
                        send_signed(c, k, trader::sign_modify(k, n, oid, &plan, &ctx, now_ms()).map(|s| vec![s]))
                    };
                    turns.place(Some(kill_epoch), read, go).await
                }
                HlAct::Leverage { asset, cross, leverage } => {
                    let go = |()| {
                        let s = trader::sign_update_leverage(k, n, &asset, cross, leverage, now_ms());
                        send_signed(c, k, s.map(|s| vec![s]))
                    };
                    turns.place(None, async { Ok(()) }, go).await
                }
                HlAct::Cancel { asset, oid } => {
                    turns
                        .cancel(async {
                            send_signed(c, k, trader::sign_cancel(k, n, &asset, oid, now_ms()).map(|s| vec![s])).await.0
                        })
                        .await
                }
                HlAct::ScheduleCancel(at) => {
                    let go = || send_signed(c, k, trader::sign_schedule_cancel(k, n, at, now_ms()).map(|s| vec![s]));
                    match turns.schedule(id, go).await {
                        Some(outcome) => outcome,
                        None => return, // the app has since made a newer change
                    }
                }
                HlAct::CancelAll { registry } => {
                    return turns.kill(kill_epoch, || kill_all(c, k, n, &user, &registry), done).await;
                }
            };
            done(label, outcome);
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

/// Send signed actions in order, stopping at the first that isn't a clean success. When a
/// send gets no clear answer, it also returns the time until which that action may still land,
/// its `expiresAfter` plus [`LAND_MARGIN_MS`].
async fn send_signed(client: &Client, key: &AgentKey, signed: Result<Vec<Signed>, Refusal>) -> (Outcome, Option<u64>) {
    let signed = match signed {
        Ok(s) => s,
        Err(refusal) => return (Outcome::Refused(refusal.to_string()), None),
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
            Ok(r) => return (Outcome::Answered(r), None),
            Err(e) => {
                let until = matches!(e, SendError::Uncertain(_))
                    .then(|| s.expires_after.unwrap_or(s.nonce + trader::EXPIRES_AFTER_MS) + LAND_MARGIN_MS);
                return (Outcome::Failed(e), until);
            }
        }
    }
    (last.unwrap_or_else(|| Outcome::Refused("Nothing to send.".into())), None)
}

/// One kill switch sweep. It reads every open order now, cancels them, and reads again, up to
/// three rounds, so it also catches an order placed elsewhere meanwhile. Reports what is left.
async fn kill_all(
    client: &Client,
    key: &AgentKey,
    nonces: &NonceManager,
    user: &str,
    registry: &AssetRegistry,
) -> (String, Outcome) {
    let mut cancelled = 0;
    let mut skipped: Vec<String> = Vec::new();
    let mut last = None;
    let failed = |o: Outcome| ("cancel all".to_string(), o);
    for _ in 0..3 {
        let orders = match open_orders_now(client, user, registry).await {
            Ok(o) => o,
            Err(e) => return failed(Outcome::Refused(format!("Could not read the open orders: {e}"))),
        };
        let open: Vec<(String, u64)> = orders.into_iter().filter(|(c, _)| !skipped.contains(c)).collect();
        if open.is_empty() {
            break;
        }
        let (signed, unknown) = match trader::sign_cancel_all(key, nonces, registry, &open, now_ms()) {
            Ok(s) => s,
            Err(e) => return failed(Outcome::Refused(e.to_string())),
        };
        cancelled += open.iter().filter(|(c, _)| !unknown.contains(c)).count();
        skipped.extend(unknown);
        if signed.is_empty() {
            break;
        }
        for s in &signed {
            match client.send(s).await {
                Ok(r) if r.ok && r.first_error().is_none() => last = Some(Outcome::Answered(r)),
                Ok(r) => return failed(Outcome::Answered(r)),
                Err(e) => return failed(Outcome::Failed(e)),
            }
        }
    }
    let left = open_orders_now(client, user, registry).await.map(|o| o.len());
    let mut label = match (cancelled, &left) {
        (0, Ok(0)) => "cancel all (no open orders)".into(),
        (n, Ok(0)) => format!("cancel all ({n} cancelled, none left)"),
        (n, Ok(k)) => format!("cancel all ({n} cancelled, {k} STILL OPEN: check ORD)"),
        (n, Err(_)) => format!("cancel all ({n} cancelled; couldn't read what's left)"),
    };
    if !skipped.is_empty() {
        label += &format!("; not cancelled, unknown market: {}", skipped.join(", "));
    }
    let outcome = last.unwrap_or_else(|| {
        Outcome::Answered(ExchangeResponse { ok: true, kind: Some("cancel".into()), statuses: Vec::new(), error: None })
    });
    (label, outcome)
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use super::*;

    const QUICK: Duration = Duration::from_secs(2);

    fn ok() -> Outcome {
        Outcome::Answered(ExchangeResponse { ok: true, kind: Some("order".into()), statuses: Vec::new(), error: None })
    }

    /// A stand-in for Hyperliquid's open orders, and what each KILL sweep reported.
    #[derive(Clone, Default)]
    struct Book {
        open: Arc<Mutex<Vec<u64>>>,
        reports: Arc<Mutex<Vec<String>>>,
    }

    impl Book {
        fn sweep(&self) -> impl Future<Output = (String, Outcome)> + use<> {
            let open = self.open.clone();
            async move {
                let n = std::mem::take(&mut *open.lock().unwrap()).len();
                (format!("cancel all ({n} cancelled, none left)"), ok())
            }
        }
        fn report(&self) -> impl Fn(String, Outcome) + use<> {
            let reports = self.reports.clone();
            move |label, _| reports.lock().unwrap().push(label)
        }
        fn reports(&self) -> Vec<String> {
            self.reports.lock().unwrap().clone()
        }
        fn open(&self) -> Vec<u64> {
            self.open.lock().unwrap().clone()
        }
    }

    async fn eventually(what: &str, f: impl Fn() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed() < QUICK, "{what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Greptile #37 (f636b7c): an order whose market read never answered held the one turn,
    /// so KILL couldn't cancel anything until it did. In the browser that was never.
    #[tokio::test]
    async fn kill_goes_ahead_while_an_order_is_stuck_reading_the_market() {
        let turns = Arc::new(Turns::default());
        let book = Book::default();
        book.open.lock().unwrap().push(1);
        let signed = Arc::new(AtomicBool::new(false));
        let order = tokio::spawn({
            let (turns, signed) = (turns.clone(), signed.clone());
            async move {
                let read = std::future::pending::<Result<(), String>>();
                let go = |()| async move {
                    signed.store(true, Ordering::SeqCst);
                    (ok(), None)
                };
                turns.place(Some(0), read, go).await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let book2 = book.clone();
        tokio::time::timeout(QUICK, turns.kill(1, move || book2.sweep(), book.report()))
            .await
            .expect("KILL waited behind an order stuck reading the market");
        assert_eq!(book.reports(), ["cancel all (1 cancelled, none left)"], "nothing was in flight: one sweep");
        let o = tokio::time::timeout(QUICK, order).await.expect("the stuck order gave up").unwrap();
        assert_eq!(o, Outcome::Refused(KILLED.into()));
        assert!(!signed.load(Ordering::SeqCst), "nothing was signed after KILL");
    }

    /// The order was already sent and Hyperliquid had it, but the answer was slow. KILL cancels
    /// everything else at once, then sweeps again once the order is answered.
    #[tokio::test]
    async fn kill_sweeps_again_once_an_order_being_sent_is_answered() {
        let turns = Arc::new(Turns::default());
        let book = Book::default();
        book.open.lock().unwrap().push(1);
        let (answer, answered) = tokio::sync::oneshot::channel::<()>();
        let order = tokio::spawn({
            let (turns, open) = (turns.clone(), book.open.clone());
            async move {
                let go = |()| async move {
                    open.lock().unwrap().push(7); // it reached Hyperliquid…
                    let _ = answered.await; // …and the answer is on its way
                    (ok(), None)
                };
                turns.place(Some(0), async { Ok(()) }, go).await
            }
        });
        eventually("the order was sent", || book.open().contains(&7)).await;
        let kill = tokio::spawn({
            let (turns, book) = (turns.clone(), book.clone());
            async move { turns.kill(1, || book.sweep(), book.report()).await }
        });
        eventually("KILL waited behind an order being sent", || !book.reports().is_empty()).await;
        let first = book.reports()[0].clone();
        assert!(first.starts_with("cancel all (2 cancelled") && first.contains("checking again"), "{first}");
        // The second sweep also catches an order placed elsewhere in the meantime.
        book.open.lock().unwrap().push(8);
        answer.send(()).unwrap();
        tokio::time::timeout(QUICK, kill).await.expect("the second sweep ran").unwrap();
        assert_eq!(order.await.unwrap(), ok());
        let r = book.reports();
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[1].starts_with("cancel all (1 cancelled, none left)"), "{r:?}");
        assert!(book.open().is_empty());
    }

    /// The send got no clear answer, so the order may still land until its `expiresAfter`
    /// passes. KILL sweeps once more after that, and an order confirmed after KILL waits until
    /// that sweep is done, so KILL never cancels it.
    #[tokio::test]
    async fn an_order_with_no_clear_answer_is_swept_again_after_its_window() {
        let turns = Arc::new(Turns::default());
        let book = Book::default();
        let window = 300;
        let until = now_ms() + window;
        let sent = turns
            .place(Some(0), async { Ok(()) }, |()| async move {
                (Outcome::Failed(SendError::Uncertain("timed out".into())), Some(until))
            })
            .await;
        assert!(matches!(sent, Outcome::Failed(SendError::Uncertain(_))));
        let t0 = std::time::Instant::now();
        let kill = tokio::spawn({
            let (turns, book) = (turns.clone(), book.clone());
            async move { turns.kill(1, || book.sweep(), book.report()).await }
        });
        eventually("the first sweep ran at once", || !book.reports().is_empty()).await;
        assert!(t0.elapsed() < Duration::from_millis(window / 2), "{:?}", t0.elapsed());
        book.open.lock().unwrap().push(9); // the uncertain order lands late
        let after = tokio::spawn({
            let (turns, open) = (turns.clone(), book.open.clone());
            async move {
                let go = |()| async move {
                    open.lock().unwrap().push(10);
                    (ok(), None)
                };
                turns.place(Some(1), async { Ok(()) }, go).await
            }
        });
        tokio::time::timeout(QUICK, kill).await.expect("KILL finished").unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(window - 20), "swept again only after the window");
        let r = book.reports();
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[1].starts_with("cancel all (1 cancelled, none left)"), "the late order was cancelled: {r:?}");
        assert_eq!(after.await.unwrap(), ok());
        assert_eq!(book.open(), [10], "the order confirmed after KILL was placed, not swept");
    }

    /// Cancels and the dead man's switch only take risk off, so they never wait for an order.
    #[tokio::test]
    async fn cancels_never_wait_for_an_order() {
        let turns = Arc::new(Turns::default());
        let _order = tokio::spawn({
            let turns = turns.clone();
            async move {
                let read = std::future::pending::<Result<(), String>>();
                turns.place(Some(0), read, |()| async { (ok(), None) }).await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let o = tokio::time::timeout(QUICK, turns.cancel(async { ok() })).await;
        assert_eq!(o.expect("a cancel waited behind an order"), ok());
    }

    /// Greptile #37 (f1ff1b3): dead man's switch changes ran side by side. Turning the switch
    /// off and on again while the clear was slow let the new schedule go out first, and the
    /// old clear could then land after it and remove it.
    #[tokio::test]
    async fn dead_mans_switch_changes_go_out_in_the_order_they_were_made() {
        let turns = Arc::new(Turns::default());
        // An order stuck reading the market holds the order turn. Schedule changes don't wait.
        let _order = tokio::spawn({
            let turns = turns.clone();
            async move {
                let read = std::future::pending::<Result<(), String>>();
                turns.place(Some(0), read, |()| async { (ok(), None) }).await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (answer, answered) = tokio::sync::oneshot::channel::<()>();
        let clear = tokio::spawn({
            let (turns, sent) = (turns.clone(), sent.clone());
            async move {
                let go = || async move {
                    sent.lock().unwrap().push("clear");
                    let _ = answered.await; // the answer is slow
                    (ok(), None)
                };
                turns.schedule(1, go).await
            }
        });
        eventually("the clear went out without waiting for the order", || !sent.lock().unwrap().is_empty()).await;
        let set = tokio::spawn({
            let (turns, sent) = (turns.clone(), sent.clone());
            async move {
                let go = || async move {
                    sent.lock().unwrap().push("set");
                    (ok(), None)
                };
                turns.schedule(2, go).await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*sent.lock().unwrap(), ["clear"], "the new schedule went out before the clear was answered");
        answer.send(()).unwrap();
        assert_eq!(tokio::time::timeout(QUICK, clear).await.unwrap().unwrap(), Some(ok()));
        assert_eq!(tokio::time::timeout(QUICK, set).await.unwrap().unwrap(), Some(ok()));
        assert_eq!(*sent.lock().unwrap(), ["clear", "set"]);
    }

    /// A change with no clear answer may still land until its `expiresAfter` passes, so the
    /// next one waits until then.
    #[tokio::test]
    async fn after_a_change_with_no_clear_answer_the_next_waits_until_it_cannot_land() {
        let turns = Turns::default();
        let window = 300;
        let until = now_ms() + window;
        let clear = turns
            .schedule(1, || async move { (Outcome::Failed(SendError::Uncertain("timed out".into())), Some(until)) })
            .await;
        assert!(matches!(clear, Some(Outcome::Failed(SendError::Uncertain(_)))));
        let t0 = std::time::Instant::now();
        assert_eq!(turns.schedule(2, || async { (ok(), None) }).await, Some(ok()));
        assert!(t0.elapsed() >= Duration::from_millis(window - 20), "sent while the clear could still land");
    }

    /// The worker runs requests side by side, so an older change can reach it after a newer
    /// one went out. The worker drops it and never sends it.
    #[tokio::test]
    async fn a_dead_mans_switch_change_the_app_has_replaced_is_dropped() {
        let turns = Turns::default();
        assert_eq!(turns.schedule(5, || async { (ok(), None) }).await, Some(ok()));
        let sent = AtomicBool::new(false);
        let go = || async {
            sent.store(true, Ordering::SeqCst);
            (ok(), None)
        };
        assert_eq!(turns.schedule(4, go).await, None);
        assert!(!sent.load(Ordering::SeqCst));
    }

    /// An order confirmed before KILL is never signed; one confirmed after it goes ahead.
    #[tokio::test]
    async fn an_order_confirmed_before_kill_is_never_signed() {
        let turns = Turns::default();
        let book = Book::default();
        turns.kill(1, || book.sweep(), book.report()).await;
        let signed = AtomicBool::new(false);
        let go = |()| async {
            signed.store(true, Ordering::SeqCst);
            (ok(), None)
        };
        assert_eq!(turns.place(Some(0), async { Ok(()) }, go).await, Outcome::Refused(KILLED.into()));
        assert!(!signed.load(Ordering::SeqCst));
        assert_eq!(turns.place(Some(1), async { Ok(()) }, |()| async { (ok(), None) }).await, ok());
    }
}
