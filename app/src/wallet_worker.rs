//! The built-in wallet's background work: the desktop sign-in (a loopback listener the
//! browser returns to), turning a Turnkey session into a signed-in wallet, the Turnkey
//! session in the OS secret store, `approveAgent` signed by the wallet, and the testnet
//! faucet's drip (unsigned).
//!
//! Turnkey signs only here, and only through `tr_hl::signing::sign_approve_agent_with`, which
//! hands it the digest of a testnet approval and nothing else. Orders never come this way:
//! they are signed by the agent key in `hl_worker`.

use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};

use tr_broker::secrets::AgentKeyStore;
use tr_data::rt;
use tr_hl::faucet::DripOutcome;
use tr_hl::model::SpotUsdc;
use tr_hl::nonce::now_ms;
use tr_hl::signing::sign_approve_agent_with;
use tr_hl::{AgentKey, SigningKey, trader};
use tr_turnkey::{Session, SessionKey};

use crate::hl_worker::{HlResponse, approve, revoke};
use crate::prefs::HlAgent;

/// The sign-in that may still finish; starting another or cancelling bumps it.
#[cfg(not(target_arch = "wasm32"))]
static SIGN_IN: AtomicU64 = AtomicU64::new(0);

/// Stop waiting for the browser (the listener closes within half a second).
#[cfg(not(target_arch = "wasm32"))]
pub fn cancel_sign_in() {
    SIGN_IN.fetch_add(1 << 32, Ordering::SeqCst);
}

pub enum WalletRequest {
    /// Desktop: listen on 127.0.0.1, say which port, and wait for the browser's session.
    #[cfg(not(target_arch = "wasm32"))]
    SignIn { id: u64, store: AgentKeyStore, key: SessionKey, state: String },
    /// Web: a session token for `key` from the sign-in page's message. Check it and read the
    /// wallet from Turnkey.
    #[cfg(target_arch = "wasm32")]
    Finish { id: u64, store: AgentKeyStore, key: SessionKey, token: String },
    /// The session kept in the secret store, if it hasn't expired.
    LoadSession { store: AgentKeyStore, account: String },
    /// Forget the session on this device.
    EndSession { store: AgentKeyStore, account: String },
    /// Approve a fresh agent key, signed by the wallet.
    Approve { store: AgentKeyStore, session: Arc<Session>, master: String, device: String },
    /// Replace the agent on Hyperliquid (signed by the wallet) and delete its key here.
    Revoke { store: AgentKeyStore, session: Arc<Session>, master: String, base: String },
    /// Spot USDC for the Deposit window.
    Balances { user: String },
    /// Ask testnet's faucet for a drip for the wallet (unsigned; testnet only).
    ClaimDrip { user: String },
}

/// A wallet the user just signed in to.
pub struct SignedIn {
    pub session: Session,
    /// Lower-case `0x…`.
    pub address: String,
    pub user: String,
    /// `Err`: the session couldn't be kept in the secret store (it lasts until Terminal quits).
    pub saved: Result<(), String>,
}

pub enum WalletResponse {
    /// The desktop listener is up: open the sign-in page with this port.
    #[cfg(not(target_arch = "wasm32"))]
    Listening {
        id: u64,
        port: u16,
    },
    SignedIn {
        id: u64,
        result: Result<Box<SignedIn>, String>,
    },
    SessionLoaded {
        account: String,
        session: Option<Arc<Session>>,
    },
    Balances {
        user: String,
        result: Result<SpotUsdc, String>,
    },
    /// What the faucet said to [`WalletRequest::ClaimDrip`].
    Drip {
        user: String,
        outcome: DripOutcome,
    },
    /// The wallet didn't sign the approval: nothing was sent, and the new key is dropped.
    ApproveNotSigned(String),
    /// The wallet didn't sign the revoke, so nothing changed: the agent key is still here.
    RevokeNotSigned(String),
}

/// The secret store item for a wallet's Turnkey session.
pub fn session_account(address: &str) -> String {
    format!("turnkey:testnet:{}", address.trim().to_ascii_lowercase())
}

fn now_secs() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Turnkey's API (or the local mock that `TR_TURNKEY_API_URL` names, loopback only).
fn turnkey() -> Result<tr_turnkey::Client, String> {
    tr_turnkey::Client::new().map_err(|e| format!("{e:#}"))
}

/// Run one request; answers go through `send`.
pub async fn handle(req: WalletRequest, send: impl Fn(HlResponse) + Clone + 'static) {
    let wallet = |r: WalletResponse| HlResponse::Wallet(Box::new(r));
    match req {
        #[cfg(not(target_arch = "wasm32"))]
        WalletRequest::SignIn { id, store, key, state } => {
            let this = SIGN_IN.fetch_add(1, Ordering::SeqCst) + 1;
            let cb = match tr_turnkey::callback::Callback::bind().await {
                Ok(cb) => cb,
                Err(e) => return send(wallet(WalletResponse::SignedIn { id, result: Err(format!("{e:#}")) })),
            };
            send(wallet(WalletResponse::Listening { id, port: cb.port() }));
            let cancelled = async {
                while SIGN_IN.load(Ordering::SeqCst) == this {
                    rt::sleep(std::time::Duration::from_millis(400)).await;
                }
            };
            let answer = tokio::select! {
                a = cb.wait(&state, tr_turnkey::callback::SIGN_IN_TIMEOUT) => a.map_err(|e| format!("{e:#}")),
                _ = cancelled => Err("Sign-in cancelled.".into()),
            };
            let result = match answer {
                Ok(tr_turnkey::callback::Answer::Session(token)) => match turnkey() {
                    Ok(tk) => finish(&tk, store, key, &token).await,
                    Err(e) => Err(e),
                },
                Ok(tr_turnkey::callback::Answer::Failed(e)) => Err(format!("The sign-in page said: {e}")),
                Err(e) => Err(e),
            };
            send(wallet(WalletResponse::SignedIn { id, result }));
        }
        #[cfg(target_arch = "wasm32")]
        WalletRequest::Finish { id, store, key, token } => {
            let result = match turnkey() {
                Ok(tk) => finish(&tk, store, key, &token).await,
                Err(e) => Err(e),
            };
            send(wallet(WalletResponse::SignedIn { id, result }));
        }
        WalletRequest::LoadSession { store, account } => {
            let acct = account.clone();
            let session = rt::blocking(move || {
                let text = store.load(&acct).ok().flatten()?;
                match Session::from_stored(&text, now_secs()) {
                    Ok(Some(s)) => Some(Arc::new(s)),
                    _ => {
                        // Expired or unreadable: it can't sign anything, so don't keep it.
                        let _ = store.delete(&acct);
                        None
                    }
                }
            })
            .await
            .flatten();
            send(wallet(WalletResponse::SessionLoaded { account, session }));
        }
        WalletRequest::EndSession { store, account } => {
            let _ = rt::blocking(move || store.delete(&account)).await;
        }
        WalletRequest::Approve { store, session, master, device } => {
            let agent = AgentKey::generate();
            let action = trader::approve_agent(&agent.address(), &device, trader::DEFAULT_AGENT_DAYS, now_ms());
            let meta = HlAgent {
                address: action.agent_address.clone(),
                name: action.agent_name.clone(),
                valid_until: trader::agent_valid_until(&action.agent_name).unwrap_or(0),
                network: tr_hl::Network::Testnet,
            };
            let key = Arc::new(agent);
            let signed = match turnkey() {
                Ok(tk) => wallet_sign(&tk, &session, &master, &action).await,
                Err(e) => Err(e),
            };
            let r = match signed {
                Ok(body) => match tr_hl::client::Client::new() {
                    Ok(client) => approve(&client, store, master, key, body, meta).await,
                    Err(e) => {
                        HlResponse::Approved { master, agent: meta, key, result: Err(format!("{e:#}")), saved: Ok(()) }
                    }
                },
                // The agent key is dropped here; nothing was sent.
                Err(e) => wallet(WalletResponse::ApproveNotSigned(e)),
            };
            send(r);
        }
        WalletRequest::Revoke { store, session, master, base } => {
            let action = trader::revoke_agent(&base, now_ms());
            let account = crate::app::hl_store_account(&master);
            let signed = match turnkey() {
                Ok(tk) => wallet_sign(&tk, &session, &master, &action).await,
                Err(e) => Err(e),
            };
            let r = match (signed, tr_hl::client::Client::new()) {
                (Ok(body), Ok(client)) => revoke(&client, store, account, Some(body)).await,
                (Err(e), _) => wallet(WalletResponse::RevokeNotSigned(e)),
                (_, Err(e)) => wallet(WalletResponse::RevokeNotSigned(format!("{e:#}"))),
            };
            send(r);
        }
        WalletRequest::Balances { user } => {
            let result = match (tr_hl::client::Client::new(), tr_hl::key::parse_address(&user)) {
                (Ok(c), Ok(a)) => c.spot_usdc(&a).await.map_err(|e| format!("{e:#}")),
                (Err(e), _) | (_, Err(e)) => Err(format!("{e:#}")),
            };
            send(wallet(WalletResponse::Balances { user, result }));
        }
        WalletRequest::ClaimDrip { user } => {
            let outcome = match (tr_hl::client::Client::new(), tr_hl::key::parse_address(&user)) {
                (Ok(c), Ok(a)) => c.claim_drip(&a).await,
                (Err(e), _) => DripOutcome::Network(format!("{e:#}")),
                (_, Err(e)) => DripOutcome::Refused(format!("{e:#}")),
            };
            send(wallet(WalletResponse::Drip { user, outcome }));
        }
    }
}

/// The wallet signs `action` (a testnet `approveAgent`) through Turnkey: the `/exchange` body.
async fn wallet_sign(
    tk: &tr_turnkey::Client,
    session: &Session,
    master: &str,
    action: &tr_hl::actions::ApproveAgentAction,
) -> Result<serde_json::Value, String> {
    if session.expired(now_secs()) {
        return Err("the wallet session expired; sign in again".into());
    }
    let main = tr_hl::key::parse_address(master).map_err(|e| format!("{e:#}"))?;
    let sig = sign_approve_agent_with(action, &main, |d| async move { tk.sign_digest(session, &main, &d).await })
        .await
        .map_err(|e| tr_hl::mask_addresses(&format!("{e:#}")))?;
    Ok(trader::user_signed_body(action, &sig))
}

/// Accept the session for `key`, read the wallet from Turnkey, and keep the session in the
/// secret store for the rest of its 15 minutes.
async fn finish(
    tk: &tr_turnkey::Client,
    store: AgentKeyStore,
    key: SessionKey,
    token: &str,
) -> Result<Box<SignedIn>, String> {
    let session = Session::accept(key, token, now_secs()).map_err(|e| format!("{e:#}"))?;
    let me = tk.whoami(&session).await.map_err(|e| format!("Turnkey: {e:#}"))?;
    let account = tk.ethereum_account(&session).await.map_err(|e| format!("Turnkey: {e:#}"))?;
    let address = tr_hl::key::parse_address(&account.address)
        .map(|a| tr_hl::key::address_hex(&a))
        .map_err(|e| format!("Turnkey's wallet address: {e:#}"))?;
    let stored = session.to_stored();
    let item = session_account(&address);
    let saved = rt::blocking(move || store.save(&item, &stored).map_err(|e| format!("{e:#}")))
        .await
        .unwrap_or_else(|| Err("the key store task failed".into()));
    Ok(Box::new(SignedIn { session, address, user: me.username, saved }))
}

#[cfg(test)]
mod tests {
    use tr_hl::signing::recover_approve_agent;
    use tr_turnkey::mock::{MockTurnkey, USERNAME};

    use super::*;

    async fn signed_in(mock: &MockTurnkey, store: &AgentKeyStore) -> Box<SignedIn> {
        let key = SessionKey::generate();
        let token = mock.mint_session(&key.public_hex(), 600);
        let tk = tr_turnkey::Client::with_base(&mock.base).unwrap();
        finish(&tk, store.clone(), key, &token).await.unwrap()
    }

    #[tokio::test]
    async fn signing_in_finds_the_wallet_and_keeps_the_session_in_the_store() {
        let mock = MockTurnkey::start(0, None).await.unwrap();
        let store = AgentKeyStore::memory();
        let s = signed_in(&mock, &store).await;
        assert_eq!(s.address, mock.address());
        assert_eq!(s.user, USERNAME);
        assert!(s.saved.is_ok());
        let kept = store.load(&session_account(&s.address)).unwrap().expect("the session is in the store");
        let back = Session::from_stored(&kept, now_secs()).unwrap().unwrap();
        assert_eq!(back.key.public_hex(), s.session.key.public_hex());
        // A token for another key is refused before Turnkey is asked anything.
        let before = mock.requests().len();
        let tk = tr_turnkey::Client::with_base(&mock.base).unwrap();
        let theirs = mock.mint_session(&SessionKey::generate().public_hex(), 600);
        assert!(finish(&tk, store, SessionKey::generate(), &theirs).await.is_err());
        assert_eq!(mock.requests().len(), before);
    }

    /// The `/exchange` body the wallet's signature goes into is a testnet approval, and the
    /// signature recovers the wallet.
    #[tokio::test]
    async fn the_wallet_signs_a_testnet_approval_that_recovers_it() {
        let mock = MockTurnkey::start(0, None).await.unwrap();
        let store = AgentKeyStore::memory();
        let s = signed_in(&mock, &store).await;
        let tk = tr_turnkey::Client::with_base(&mock.base).unwrap();
        let agent = AgentKey::generate();
        let action = trader::approve_agent(&agent.address(), "ci", trader::DEFAULT_AGENT_DAYS, now_ms());
        let body = wallet_sign(&tk, &s.session, &s.address, &action).await.unwrap();
        assert_eq!(body["action"]["type"], "approveAgent");
        assert_eq!(body["action"]["hyperliquidChain"], "Testnet");
        assert_eq!(body["action"]["signatureChainId"], "0x66eee");
        let sig: tr_hl::signing::WireSignature = serde_json::from_value(body["signature"].clone()).unwrap();
        assert_eq!(tr_hl::key::address_hex(&recover_approve_agent(&action, &sig).unwrap()), s.address);
        // 180 days, the longest Hyperliquid allows, so approvals (paid signatures) are rare.
        let until = trader::agent_valid_until(&action.agent_name).unwrap();
        assert!(until >= now_ms() + 179 * 86_400_000, "{}", action.agent_name);
    }

    #[tokio::test]
    async fn an_expired_session_asks_turnkey_nothing() {
        let mock = MockTurnkey::start(0, None).await.unwrap();
        let key = SessionKey::generate();
        let token = mock.mint_session(&key.public_hex(), 2);
        let session = Session::accept(key, &token, now_secs()).unwrap();
        let tk = tr_turnkey::Client::with_base(&mock.base).unwrap();
        let action = trader::approve_agent(&AgentKey::generate().address(), "ci", 180, now_ms());
        let late =
            Session::accept(SessionKey::from_hex(&session.key.to_hex()).unwrap(), &session.token, now_secs()).unwrap();
        // Two seconds later it has expired.
        tokio::time::sleep(std::time::Duration::from_millis(2_100)).await;
        let err = wallet_sign(&tk, &late, &mock.address(), &action).await.unwrap_err();
        assert!(err.contains("expired"), "{err}");
        assert!(mock.requests().is_empty());
    }

    /// An approval the wallet didn't sign is reported as such, not as Hyperliquid's refusal.
    #[tokio::test]
    async fn an_approval_the_wallet_did_not_sign_says_so() {
        let mock = MockTurnkey::start(0, None).await.unwrap();
        let key = SessionKey::generate();
        let token = mock.mint_session(&key.public_hex(), 1);
        let session = Arc::new(Session::accept(key, &token, now_secs()).unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        let got = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = got.clone();
        let req = WalletRequest::Approve {
            store: AgentKeyStore::memory(),
            session,
            master: mock.address(),
            device: "ci".into(),
        };
        handle(req, move |r| {
            *sink.lock().unwrap() = Some(match r {
                HlResponse::Wallet(w) => match *w {
                    WalletResponse::ApproveNotSigned(e) => e,
                    _ => "other".into(),
                },
                _ => "sent to Hyperliquid".into(),
            });
        })
        .await;
        assert!(got.lock().unwrap().as_deref().is_some_and(|e| e.contains("expired")), "{:?}", got.lock().unwrap());
        assert!(mock.requests().is_empty());
    }

    /// A revoke the wallet didn't sign changes nothing: the agent key stays in the store and
    /// the app is told so (it keeps the agent until a revoke really happens).
    #[tokio::test]
    async fn a_revoke_the_wallet_did_not_sign_keeps_the_agent() {
        let mock = MockTurnkey::start(0, None).await.unwrap();
        let store = AgentKeyStore::memory();
        let master = mock.address();
        let agent_item = crate::app::hl_store_account(&master);
        store.save(&agent_item, &AgentKey::generate().to_hex()).unwrap();
        let key = SessionKey::generate();
        let token = mock.mint_session(&key.public_hex(), 1);
        let session = Arc::new(Session::accept(key, &token, now_secs()).unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        let got = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = got.clone();
        let req = WalletRequest::Revoke { store: store.clone(), session, master, base: "terminal-ci".into() };
        handle(req, move |r| {
            *sink.lock().unwrap() = Some(match r {
                HlResponse::Wallet(w) if matches!(*w, WalletResponse::RevokeNotSigned(_)) => "not signed",
                HlResponse::Revoked(_) => "revoked",
                _ => "other",
            });
        })
        .await;
        assert_eq!(*got.lock().unwrap(), Some("not signed"));
        assert!(store.load(&agent_item).unwrap().is_some(), "the agent key is still here");
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn an_expired_session_in_the_store_is_deleted_on_load() {
        let mock = MockTurnkey::start(0, None).await.unwrap();
        let store = AgentKeyStore::memory();
        let key = SessionKey::generate();
        let token = mock.mint_session(&key.public_hex(), 1);
        let session = Session::accept(key, &token, now_secs()).unwrap();
        let account = session_account(&mock.address());
        store.save(&account, &session.to_stored()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        let got = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = got.clone();
        handle(WalletRequest::LoadSession { store: store.clone(), account: account.clone() }, move |r| {
            if let HlResponse::Wallet(w) = r
                && let WalletResponse::SessionLoaded { session, .. } = *w
            {
                *sink.lock().unwrap() = Some(session.is_some());
            }
        })
        .await;
        assert_eq!(*got.lock().unwrap(), Some(false));
        assert!(store.load(&account).unwrap().is_none(), "the expired session was deleted");
    }
}
