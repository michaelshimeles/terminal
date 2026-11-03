//! Hyperliquid in the browser: the wallet (EIP-1193, `window.ethereum`) signs `approveAgent`
//! and the revoke, and the agent key lives in memory for the session, or, if the user opts
//! in, in IndexedDB encrypted with a non-extractable WebCrypto AES-GCM key (the page can use
//! that key but never read it).
//!
//! The wallet signs at its current chain id (wallets refuse typed data for another chain);
//! `hyperliquidChain` is always `Testnet`. Terminal checks that the signature recovers the
//! wallet's address before sending it.

use std::cell::RefCell;

use egui::{RichText, Ui};
use tr_hl::actions::ApproveAgentAction;
use tr_hl::nonce::now_ms;
use tr_hl::signing::WireSignature;
use tr_hl::trader;
use tr_hl::{AgentKey, SigningKey};
use wasm_bindgen::prelude::*;

use super::TerminalApp;
use super::hl::store_account;
use crate::theme::{amber, muted, ui_font};

#[wasm_bindgen(inline_js = r#"
const DB = 'terminal-hl', STORE = 'keys';
function open() {
  return new Promise((res, rej) => {
    const r = indexedDB.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore(STORE);
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  });
}
function tx(db, mode, f) {
  return new Promise((res, rej) => {
    const t = db.transaction(STORE, mode);
    const req = f(t.objectStore(STORE));
    t.oncomplete = () => res(req ? req.result : undefined);
    t.onerror = () => rej(t.error);
  });
}
// A non-extractable AES-GCM key, kept as a CryptoKey in IndexedDB: usable, never readable.
async function wrapping(db) {
  let k = await tx(db, 'readonly', s => s.get('wrap'));
  if (!k) {
    k = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
    await tx(db, 'readwrite', s => s.put(k, 'wrap'));
  }
  return k;
}
export async function hl_key_save(account, hex) {
  const db = await open();
  const k = await wrapping(db);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, k, new TextEncoder().encode(hex)));
  await tx(db, 'readwrite', s => s.put({ iv, ct }, 'agent:' + account));
}
export async function hl_key_load(account) {
  const db = await open();
  const rec = await tx(db, 'readonly', s => s.get('agent:' + account));
  const k = await tx(db, 'readonly', s => s.get('wrap'));
  if (!rec || !k) return null;
  return new TextDecoder().decode(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: rec.iv }, k, rec.ct));
}
export async function hl_key_delete(account) {
  const db = await open();
  await tx(db, 'readwrite', s => s.delete('agent:' + account));
}
export function hl_has_wallet() { return !!(typeof window !== 'undefined' && window.ethereum); }
export async function hl_wallet_account() {
  const a = await window.ethereum.request({ method: 'eth_requestAccounts' });
  return a && a[0] ? String(a[0]) : null;
}
export async function hl_wallet_chain() { return String(await window.ethereum.request({ method: 'eth_chainId' })); }
export async function hl_wallet_sign(address, typed) {
  return String(await window.ethereum.request({ method: 'eth_signTypedData_v4', params: [address, typed] }));
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    async fn hl_key_save(account: &str, hex: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn hl_key_load(account: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn hl_key_delete(account: &str) -> Result<JsValue, JsValue>;
    fn hl_has_wallet() -> bool;
    #[wasm_bindgen(catch)]
    async fn hl_wallet_account() -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn hl_wallet_chain() -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn hl_wallet_sign(address: &str, typed: &str) -> Result<JsValue, JsValue>;
}

/// What the page's promises came back with, for the next frame.
enum Event {
    Account(Result<String, String>),
    Signed(Box<Signed>),
    Restored { account: String, hex: Option<zeroize::Zeroizing<String>> },
    Note(String, bool),
}

/// A wallet signature for an approval or a revoke.
struct Signed {
    revoke: bool,
    master: String,
    agent: Option<AgentKey>,
    action: ApproveAgentAction,
    sig: Result<WireSignature, String>,
}

thread_local! {
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

fn post(ctx: &egui::Context, e: Event) {
    EVENTS.with(|v| v.borrow_mut().push(e));
    ctx.request_repaint();
}

fn js_err(e: JsValue) -> String {
    e.as_string()
        .or_else(|| js_sys::Reflect::get(&e, &"message".into()).ok().and_then(|m| m.as_string()))
        .unwrap_or_else(|| format!("{e:?}"))
}

impl TerminalApp {
    /// Settings › Trading › Hyperliquid, web: the wallet buttons.
    pub(super) fn hl_web_wallet_controls(&mut self, ui: &mut Ui, h: &crate::prefs::HlPrefs, busy: bool) {
        if !hl_has_wallet() {
            ui.label(
                RichText::new("No browser wallet found (MetaMask, Rabby…). Show an address above (read-only), or use the desktop app to trade.")
                    .font(ui_font(11.5))
                    .color(muted()),
            );
            return;
        }
        ui.label(
            RichText::new("Your wallet signs approveAgent once (testnet). The agent key stays in this tab unless you choose to remember it below.")
                .font(ui_font(11.5))
                .color(amber()),
        );
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| match &self.hl.wallet {
            None => {
                if ui.add_enabled(!busy, egui::Button::new("Connect Wallet")).clicked() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let r = hl_wallet_account()
                            .await
                            .map_err(js_err)
                            .and_then(|v| v.as_string().ok_or("no account".into()));
                        post(&ctx, Event::Account(r));
                    });
                }
            }
            Some(w) => {
                ui.label(RichText::new(format!("Wallet {}", super::hl_view::masked(w))).font(ui_font(12.0)));
                let wallet = w.clone();
                if h.agent.is_none() {
                    if ui
                        .add_enabled(!busy, egui::Button::new(RichText::new("Approve Agent with Wallet").strong()))
                        .clicked()
                    {
                        self.hl_web_sign(ctx, wallet, false);
                    }
                } else if ui.add_enabled(!busy, egui::Button::new("Revoke on Hyperliquid")).clicked() {
                    self.hl_web_sign(ctx, wallet, true);
                }
            }
        });
    }

    /// Ask the wallet to sign an approval for a fresh agent (or a revoke).
    fn hl_web_sign(&mut self, ctx: egui::Context, wallet: String, revoke: bool) {
        self.hl.busy = Some(if revoke {
            "Waiting for the wallet to sign the revoke…"
        } else {
            "Waiting for the wallet to sign approveAgent…"
        });
        let base = self.prefs.trading.hl.agent.as_ref().map(|a| trader::agent_base_name(&a.name).to_string());
        wasm_bindgen_futures::spawn_local(async move {
            let chain =
                match hl_wallet_chain().await.map_err(js_err).and_then(|c| c.as_string().ok_or("no chain id".into())) {
                    Ok(c) => c,
                    Err(e) => return post(&ctx, Event::Note(format!("The wallet didn't give its chain: {e}"), true)),
                };
            let (agent, mut action) = if revoke {
                (None, trader::revoke_agent(&base.unwrap_or_else(|| "terminal-web".into()), now_ms()))
            } else {
                let agent = AgentKey::generate();
                let action = trader::approve_agent(&agent.address(), "web", trader::DEFAULT_AGENT_DAYS, now_ms());
                (Some(agent), action)
            };
            action.signature_chain_id = chain.to_ascii_lowercase();
            let typed = match tr_hl::signing::approve_agent_typed_data(&action) {
                Ok(t) => t.to_string(),
                Err(e) => return post(&ctx, Event::Note(format!("{e:#}"), true)),
            };
            let sig = hl_wallet_sign(&wallet, &typed)
                .await
                .map_err(js_err)
                .and_then(|s| s.as_string().ok_or("no signature".into()))
                .and_then(|s| WireSignature::from_rsv_hex(&s).map_err(|e| e.to_string()));
            post(
                &ctx,
                Event::Signed(Box::new(Signed { revoke, master: wallet.to_ascii_lowercase(), agent, action, sig })),
            );
        });
    }

    /// Handle what the page's promises returned.
    pub(super) fn hl_web_poll(&mut self) {
        let events = EVENTS.with(|v| std::mem::take(&mut *v.borrow_mut()));
        for e in events {
            match e {
                Event::Account(Ok(a)) => {
                    let a = a.to_ascii_lowercase();
                    self.hl.wallet = Some(a.clone());
                    if self.hl_master().as_deref() != Some(&a) {
                        self.hl_connect(&a);
                    }
                }
                Event::Account(Err(e)) => self.hl.message = Some((format!("The wallet didn't connect: {e}"), true)),
                Event::Signed(signed) => {
                    let Signed { revoke, master, agent, action, sig } = *signed;
                    self.hl.busy = None;
                    let sig = match sig {
                        Ok(s) => s,
                        Err(e) => {
                            self.hl.message = Some((format!("Not signed: {e}"), true));
                            continue;
                        }
                    };
                    // The wallet must have signed exactly this, as this account.
                    let signer =
                        tr_hl::signing::recover_approve_agent(&action, &sig).map(|a| tr_hl::key::address_hex(&a));
                    if signer.as_deref().ok() != Some(master.as_str()) {
                        self.hl.message =
                            Some(("The wallet's signature doesn't match its account; nothing was sent.".into(), true));
                        continue;
                    }
                    let body = trader::user_signed_body(&action, &sig);
                    match (revoke, agent) {
                        (true, _) => self.hl_revoke_send(Some(body)),
                        (false, Some(agent)) => self.hl_submit_approval(master, agent, &action, body),
                        (false, None) => {}
                    }
                }
                Event::Restored { account, hex } => {
                    let Some(hex) = hex else { continue };
                    let expected = self.prefs.trading.hl.agent.as_ref().map(|a| a.address.clone());
                    if let Ok(k) = AgentKey::from_hex(&hex)
                        && Some(tr_hl::key::address_hex(&k.address())) == expected
                        && self.hl_master().is_some_and(|m| store_account(&m) == account)
                    {
                        let _ = self.hl.store.save(&account, &hex);
                        self.hl.key = Some(std::sync::Arc::new(k));
                    }
                }
                Event::Note(m, err) => {
                    self.hl.busy = None;
                    self.hl.message = Some((m, err));
                }
            }
        }
    }

    /// After loading from memory finds nothing: try the encrypted copy, if the user opted in.
    pub(super) fn hl_web_restore_key(&mut self) {
        if self.hl.key.is_some() || !self.prefs.trading.hl.remember_on_browser || self.prefs.trading.hl.agent.is_none()
        {
            return;
        }
        let Some(account) = self.hl_master().map(|m| store_account(&m)) else { return };
        let Some(ctx) = self.hl.ctx.clone() else { return };
        wasm_bindgen_futures::spawn_local(async move {
            let hex = match hl_key_load(&account).await {
                Ok(v) => v.as_string().map(zeroize::Zeroizing::new),
                Err(e) => {
                    tracing::warn!("hyperliquid: reading the remembered key: {}", js_err(e));
                    None
                }
            };
            post(&ctx, Event::Restored { account, hex });
        });
    }

    /// Keep an encrypted copy in this browser (opted in).
    pub(super) fn hl_web_remember_key(&mut self) {
        let (Some(key), Some(master)) = (self.hl.key.clone(), self.hl_master()) else { return };
        if !self.prefs.trading.hl.remember_on_browser {
            return;
        }
        let account = store_account(&master);
        let Some(ctx) = self.hl.ctx.clone() else { return };
        wasm_bindgen_futures::spawn_local(async move {
            let hex = key.to_hex();
            if let Err(e) = hl_key_save(&account, &hex).await {
                post(&ctx, Event::Note(format!("Couldn't remember the key on this browser: {}", js_err(e)), true));
            }
        });
    }

    /// Delete the encrypted copy.
    pub(super) fn hl_web_forget_key(&mut self) {
        let Some(master) = self.hl_master() else { return };
        let account = store_account(&master);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = hl_key_delete(&account).await {
                tracing::warn!("hyperliquid: deleting the remembered key: {}", js_err(e));
            }
        });
    }
}
