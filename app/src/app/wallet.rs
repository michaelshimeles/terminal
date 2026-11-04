//! The built-in wallet (Turnkey), testnet only: sign-in through the browser, the short Turnkey
//! session, approving and revoking the trading (agent) key with the wallet, Deposit and backup.
//!
//! The wallet's key never leaves Turnkey. Turnkey signs only `approveAgent` (see
//! `crate::wallet_worker`); orders are signed by the agent key on this device, as before.
//! See `docs/architecture/trading-hyperliquid.md` §4.5.

use std::sync::Arc;
use std::time::Duration;

use tr_hl::model::SpotUsdc;
use tr_turnkey::session::random_state;
use tr_turnkey::{Session, SessionKey};
use web_time::Instant;

use super::TerminalApp;
use crate::prefs::HlWallet;
use crate::wallet_worker::{SignedIn, WalletRequest, WalletResponse, session_account};
use crate::worker::Request;

/// A session with less than this left isn't used to sign: the user signs in again first.
const SESSION_MARGIN_SECS: u64 = 60;
/// How often the Deposit window re-reads spot USDC while it is open.
const BALANCES_EVERY: Duration = Duration::from_secs(15);

/// What to do once a sign-in finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Approve,
    Revoke,
}

/// A sign-in waiting for the browser.
pub struct SignIn {
    id: u64,
    state: String,
    /// The device key's public half (the page asks Turnkey for a session bound to it).
    #[cfg(not(target_arch = "wasm32"))]
    public_key: String,
    /// The page to open (again), once known.
    pub url: Option<String>,
    pub started: Instant,
    then: Option<After>,
    /// Web: the key waits here for the page's message. Desktop: the worker holds it.
    #[cfg(target_arch = "wasm32")]
    key: Option<SessionKey>,
}

#[derive(Default)]
pub struct WalletState {
    /// The live Turnkey session (at most 15 minutes), if any.
    pub session: Option<Arc<Session>>,
    /// The secret store item already read, so it isn't read again every frame.
    session_tried: Option<String>,
    pub signing_in: Option<SignIn>,
    next_id: u64,
    pub deposit: bool,
    pub backup_prompt: bool,
    pub backup_ack: bool,
    /// Spot USDC for the Deposit window: `(address, result)`.
    pub spot: Option<(String, Result<SpotUsdc, String>)>,
    spot_at: Option<Instant>,
}

impl WalletState {
    /// Minutes left on the session, if it can still sign.
    pub fn session_minutes(&self) -> Option<u64> {
        let s = self.session.as_ref()?;
        let left = s.remaining(now_secs());
        (left > SESSION_MARGIN_SECS).then(|| left.div_ceil(60))
    }
}

pub(crate) fn now_secs() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The sign-in page in the browser build: this site's own `/wallet/`.
#[cfg(target_arch = "wasm32")]
fn web_page() -> String {
    format!("{}/wallet/", super::hl_web::origin())
}

impl TerminalApp {
    fn wallet_send(&self, r: WalletRequest) {
        self.worker.send(Request::Wallet(r));
    }

    /// The signed-in wallet, if any.
    pub(super) fn wallet(&self) -> Option<&HlWallet> {
        self.prefs.trading.hl.wallet.as_ref()
    }

    /// Open the sign-in page. `then` runs once the session is back.
    pub(super) fn wallet_sign_in(&mut self, then: Option<After>) {
        self.hl.wallet.next_id += 1;
        let id = self.hl.wallet.next_id;
        let key = SessionKey::generate();
        let state = random_state();
        let public_key = key.public_hex();
        self.hl.message = None;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.hl.wallet.signing_in =
                Some(SignIn { id, state: state.clone(), public_key, url: None, started: Instant::now(), then });
            self.wallet_send(WalletRequest::SignIn { id, store: self.hl.store.clone(), key, state });
        }
        #[cfg(target_arch = "wasm32")]
        {
            let url = tr_turnkey::web_sign_in_url(&web_page(), &public_key, &state);
            let opened = super::hl_web::wallet_popup(&url);
            self.hl.wallet.signing_in =
                Some(SignIn { id, state, url: Some(url), started: Instant::now(), then, key: Some(key) });
            if !opened {
                self.hl.message = Some((
                    "The browser blocked the sign-in window: allow pop-ups, then press Open Again.".into(),
                    true,
                ));
            }
        }
    }

    /// Stop waiting for the browser.
    pub(super) fn wallet_cancel_sign_in(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        crate::wallet_worker::cancel_sign_in();
        self.hl.wallet.signing_in = None;
    }

    /// Open the sign-in page again (the browser tab was closed).
    pub(super) fn wallet_reopen(&mut self) {
        let Some(url) = self.hl.wallet.signing_in.as_ref().and_then(|s| s.url.clone()) else { return };
        #[cfg(not(target_arch = "wasm32"))]
        if let Err(e) = super::broker::open_external(&url) {
            self.hl.message = Some((format!("Couldn't open the browser: {e}"), true));
        }
        #[cfg(target_arch = "wasm32")]
        if !super::hl_web::wallet_popup(&url) {
            self.hl.message = Some(("The browser blocked the sign-in window: allow pop-ups.".into(), true));
        }
    }

    /// Web: the page's message for the sign-in with `state`.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn wallet_page_answered(&mut self, state: &str, session: Option<String>, error: Option<String>) {
        let Some(sign_in) = self.hl.wallet.signing_in.as_mut() else { return };
        if !tr_turnkey::session::same_secret(&sign_in.state, state) {
            return;
        }
        match (session, sign_in.key.take()) {
            (Some(token), Some(key)) => {
                let id = sign_in.id;
                self.wallet_send(WalletRequest::Finish { id, store: self.hl.store.clone(), key, token });
            }
            (_, _) => {
                self.hl.wallet.signing_in = None;
                self.hl.message =
                    Some((format!("Not signed in: {}", error.unwrap_or_else(|| "no answer".into())), true));
            }
        }
    }

    /// Approve a trading key with the wallet (signing in first if there's no session).
    pub(super) fn wallet_approve(&mut self) {
        let Some(w) = self.wallet().cloned() else { return };
        let Some(session) = self.hl.wallet.session.clone().filter(|_| self.hl.wallet.session_minutes().is_some())
        else {
            return self.wallet_sign_in(Some(After::Approve));
        };
        self.hl.busy = Some("Approving the trading key: the wallet signs approveAgent on Turnkey…");
        self.hl.message = None;
        self.hl.pending_approval = Some(w.address.clone());
        self.wallet_send(WalletRequest::Approve {
            store: self.hl.store.clone(),
            session,
            master: w.address,
            device: super::hl::device(),
        });
    }

    /// Replace the agent on Hyperliquid with one nobody holds (the wallet signs), and delete its
    /// key here.
    pub(super) fn wallet_revoke(&mut self) {
        let Some(w) = self.wallet().cloned() else { return };
        let Some(session) = self.hl.wallet.session.clone().filter(|_| self.hl.wallet.session_minutes().is_some())
        else {
            return self.wallet_sign_in(Some(After::Revoke));
        };
        let base = self
            .prefs
            .trading
            .hl
            .agent
            .as_ref()
            .map(|a| tr_hl::trader::agent_base_name(&a.name).to_string())
            .unwrap_or_else(|| format!("terminal-{}", super::hl::device()));
        self.hl_forget_agent();
        self.hl.busy = Some("Revoking: the wallet signs the replacement on Turnkey…");
        self.wallet_send(WalletRequest::Revoke { store: self.hl.store.clone(), session, master: w.address, base });
    }

    /// Forget the Turnkey session on this device (trading keeps working with the agent key).
    pub(super) fn wallet_end_session(&mut self) {
        let Some(w) = self.wallet() else { return };
        let account = session_account(&w.address);
        self.hl.wallet.session = None;
        self.wallet_send(WalletRequest::EndSession { store: self.hl.store.clone(), account });
    }

    /// Back up the wallet: the sign-in page's export mode, where Turnkey's own iframe shows
    /// the recovery phrase. Terminal never sees it.
    pub(super) fn wallet_backup(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        let url = tr_turnkey::export_url(&tr_turnkey::sign_in_page());
        #[cfg(target_arch = "wasm32")]
        let url = tr_turnkey::export_url(&web_page());
        if let Err(e) = super::broker::open_external(&url) {
            self.hl.message = Some((format!("Couldn't open the browser: {e}"), true));
        }
    }

    pub(super) fn wallet_open_deposit(&mut self) {
        self.hl.wallet.deposit = true;
        self.hl.wallet.spot_at = None;
    }

    /// Read the session kept in the store (once), drop an expired one, refresh Deposit.
    pub(super) fn wallet_periodic(&mut self) {
        let Some(w) = self.wallet().cloned() else { return };
        let account = session_account(&w.address);
        if self.hl.wallet.session.is_none() && self.hl.wallet.session_tried.as_deref() != Some(&account) {
            self.hl.wallet.session_tried = Some(account.clone());
            self.wallet_send(WalletRequest::LoadSession { store: self.hl.store.clone(), account: account.clone() });
        }
        if self.hl.wallet.session.as_ref().is_some_and(|s| s.expired(now_secs())) {
            self.hl.wallet.session = None;
            self.wallet_send(WalletRequest::EndSession { store: self.hl.store.clone(), account });
        }
        if self.hl.wallet.deposit && self.hl.wallet.spot_at.is_none_or(|t| t.elapsed() > BALANCES_EVERY) {
            self.hl.wallet.spot_at = Some(Instant::now());
            self.wallet_send(WalletRequest::Balances { user: w.address });
        }
    }

    pub(super) fn on_wallet(&mut self, r: WalletResponse) {
        match r {
            #[cfg(not(target_arch = "wasm32"))]
            WalletResponse::Listening { id, port } => {
                let Some(s) = self.hl.wallet.signing_in.as_mut().filter(|s| s.id == id) else { return };
                let url = tr_turnkey::desktop_sign_in_url(&tr_turnkey::sign_in_page(), &s.public_key, &s.state, port);
                s.url = Some(url);
                self.wallet_reopen();
            }
            WalletResponse::SignedIn { id, result } => {
                let Some(s) = self.hl.wallet.signing_in.take() else { return };
                if s.id != id {
                    self.hl.wallet.signing_in = Some(s);
                    return;
                }
                match result {
                    Ok(signed) => self.wallet_signed_in(*signed, s.then),
                    Err(e) => self.hl.message = Some((tr_hl::mask_addresses(&e), true)),
                }
            }
            WalletResponse::SessionLoaded { account, session } => {
                if self.wallet().is_some_and(|w| session_account(&w.address) == account)
                    && self.hl.wallet.session.is_none()
                {
                    self.hl.wallet.session = session;
                }
            }
            WalletResponse::Balances { user, result } => {
                if self.wallet().is_some_and(|w| w.address == user) {
                    self.hl.wallet.spot = Some((user, result));
                }
            }
        }
    }

    fn wallet_signed_in(&mut self, s: SignedIn, then: Option<After>) {
        let SignedIn { session, address, user, saved } = s;
        let organization_id = session.organization_id().to_string();
        if self.hl_master().as_deref() != Some(&address) {
            // Another account: its agent (if any) is not this one's.
            self.prefs.trading.hl.agent = None;
            self.prefs.trading.hl.enabled = false;
            self.hl.key = None;
            self.hl.reset_key_lookup();
        }
        self.prefs.trading.hl.master = Some(address.clone());
        self.prefs.trading.hl.wallet = Some(HlWallet { address: address.clone(), organization_id, user: user.clone() });
        self.save_hl_prefs();
        self.hl.wallet.session_tried = Some(session_account(&address));
        self.hl.wallet.session = Some(Arc::new(session));
        let masked = super::hl_view::masked(&address);
        let mut note = format!("Signed in: wallet {masked} (Turnkey user {user}).");
        if let Err(e) = saved {
            note.push_str(&format!(" The session couldn't be kept in {} ({e}).", self.hl.store.describe()));
        }
        self.hl.message = Some((note, false));
        self.hl_start();
        match then {
            Some(After::Approve) => self.wallet_approve(),
            Some(After::Revoke) => self.wallet_revoke(),
            None => {}
        }
    }

    /// Stop showing the wallet here and forget its session (its agent key stays unless revoked).
    pub(super) fn wallet_remove(&mut self) {
        self.wallet_end_session();
        self.prefs.trading.hl.wallet = None;
        self.hl.wallet = Default::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_close_to_expiry_does_not_sign() {
        let key = SessionKey::generate();
        let claims = |exp| tr_turnkey::session::Claims {
            organization_id: "org".into(),
            user_id: "u".into(),
            public_key: key.public_hex(),
            exp,
            session_type: String::new(),
        };
        let mut w = WalletState::default();
        assert_eq!(w.session_minutes(), None);
        let token = tr_turnkey::session::unsigned_token(&claims(now_secs() + 600));
        w.session =
            Some(Arc::new(Session::accept(SessionKey::from_hex(&key.to_hex()).unwrap(), &token, now_secs()).unwrap()));
        assert_eq!(w.session_minutes(), Some(10));
        let token = tr_turnkey::session::unsigned_token(&claims(now_secs() + 30));
        w.session =
            Some(Arc::new(Session::accept(SessionKey::from_hex(&key.to_hex()).unwrap(), &token, now_secs()).unwrap()));
        assert_eq!(w.session_minutes(), None, "under a minute left: sign in again first");
    }
}
