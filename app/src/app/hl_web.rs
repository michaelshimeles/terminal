//! Hyperliquid in the browser: the built-in wallet's sign-in popup, and the agent key, which
//! lives in memory for the session or, if the user opts in, in IndexedDB encrypted with a
//! non-extractable WebCrypto AES-GCM key (the page can use that key but never read it).
//!
//! Sign-in opens this site's `/wallet/` page in a popup with the session key's public half
//! and a `state`. The page answers with `postMessage`; only messages from this page's own
//! origin, carrying the same `state`, are taken (see `super::wallet`). Browser wallets
//! (EIP-1193) are not used.

use std::cell::RefCell;

use tr_hl::{AgentKey, SigningKey};
use wasm_bindgen::prelude::*;

use super::TerminalApp;
use super::hl::store_account;

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
// The sign-in page's answer, from this origin only. Kept until Rust takes it.
let walletInbox = null;
if (typeof window !== 'undefined') {
  window.addEventListener('message', (e) => {
    if (e.origin !== window.location.origin) return;
    const d = e.data;
    if (!d || d.type !== 'terminal-wallet' || typeof d.state !== 'string') return;
    walletInbox = JSON.stringify({ state: d.state, session: typeof d.session === 'string' ? d.session : null,
      error: typeof d.error === 'string' ? d.error : null });
  });
}
export function wallet_popup(url) {
  const w = window.open(url, 'terminal-wallet', 'popup,width=480,height=760');
  return !!w;
}
export function wallet_take() { const v = walletInbox; walletInbox = null; return v; }
export function page_origin() { return window.location.origin; }
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    async fn hl_key_save(account: &str, hex: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn hl_key_load(account: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn hl_key_delete(account: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = wallet_popup)]
    fn js_wallet_popup(url: &str) -> bool;
    fn wallet_take() -> Option<String>;
    fn page_origin() -> String;
}

/// Open the sign-in page in a popup; `false` if the browser blocked it.
pub(super) fn wallet_popup(url: &str) -> bool {
    js_wallet_popup(url)
}

/// This page's origin (`https://terminal.rasmic.xyz`).
pub(super) fn origin() -> String {
    page_origin()
}

#[derive(serde::Deserialize)]
struct PageAnswer {
    state: String,
    session: Option<String>,
    error: Option<String>,
}

/// What the page's promises came back with, for the next frame.
enum Event {
    Restored { account: String, hex: Option<zeroize::Zeroizing<String>> },
    Note(String, bool),
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
    /// Handle what the page's promises returned.
    pub(super) fn hl_web_poll(&mut self) {
        if let Some(a) = wallet_take().and_then(|t| serde_json::from_str::<PageAnswer>(&t).ok()) {
            self.wallet_page_answered(&a.state, a.session, a.error);
        }
        let events = EVENTS.with(|v| std::mem::take(&mut *v.borrow_mut()));
        for e in events {
            match e {
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
