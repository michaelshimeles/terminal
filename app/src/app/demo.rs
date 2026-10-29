//! The demo (paper-trading) account inside the app: loading and saving the book, feeding it
//! the terminal's quotes so resting orders fill, and switching between the demo and live
//! accounts.
//!
//! The demo account is selected on first launch and stays selected until the user switches
//! to live accounts, which takes a confirmation and is remembered in the preferences. Every
//! demo change is saved at once to `demo-account.json` in the app data folder.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tr_broker::demo::{DemoBook, Event, STARTING_CASH};
use tr_broker::trading::OrderRequest;
use tr_broker::{InstrumentKind, Order, Portfolio};
use web_time::Instant;

use super::TerminalApp;
use crate::prefs::AccountMode;
use crate::worker::{Request, data_dir};

/// How often quotes are requested for demo positions and open demo orders.
const QUOTES_EVERY: Duration = Duration::from_secs(10);

pub struct DemoState {
    pub book: DemoBook,
    /// `book.portfolio()`, rebuilt after every change so panes can borrow it.
    pub pf: Box<Portfolio>,
    path: PathBuf,
    last_quotes: Option<Instant>,
    /// Settings › Trading: the reset confirmation is open, with this starting cash.
    pub confirm_reset: bool,
    pub reset_cash: f64,
    /// The last save failed (disk full, folder unwritable): shown until a save succeeds,
    /// and retried periodically and on exit.
    pub save_error: Option<String>,
    last_save_try: Option<Instant>,
}

pub fn demo_path() -> PathBuf {
    data_dir().join("demo-account.json")
}

/// Terminal market key for a demo symbol (`AAPL` → `AAPL US Equity`).
pub fn market_key(symbol: &str) -> Option<String> {
    tr_broker::model::market_key(InstrumentKind::Stock, symbol, "USD")
}

impl DemoState {
    /// Load the saved book. A damaged file is moved aside (never overwritten) and a fresh
    /// account starts; the returned message says so.
    pub fn load(path: PathBuf) -> (Self, Option<String>) {
        let now = chrono::Utc::now();
        let saved = match crate::storage::read(&path) {
            Ok(bytes) => DemoBook::from_json(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DemoBook::new(now)),
            Err(e) => Err(anyhow::Error::new(e).context("reading the demo account")),
        };
        let (book, note) = match saved {
            Ok(b) => (b, None),
            Err(e) => {
                let aside = path.with_extension(format!("damaged-{}.json", now.format("%Y%m%d%H%M%S")));
                let moved = crate::storage::rename(&path, &aside).is_ok();
                tracing::warn!("demo account: {e:#}");
                let note = if moved {
                    format!(
                        "The demo account file was damaged and was kept as {}; a new demo account started.",
                        aside.display()
                    )
                } else {
                    format!("The demo account file could not be read ({e:#}); a new demo account started.")
                };
                (DemoBook::new(now), Some(note))
            }
        };
        let pf = Box::new(book.portfolio());
        (
            Self {
                book,
                pf,
                path,
                last_quotes: None,
                confirm_reset: false,
                reset_cash: STARTING_CASH,
                save_error: None,
                last_save_try: None,
            },
            note,
        )
    }

    /// The book changed: rebuild the view and save.
    fn changed(&mut self) {
        *self.pf = self.book.portfolio();
        self.save();
    }

    /// Save now. Returns whether the book is on disk.
    pub fn save(&mut self) -> bool {
        self.last_save_try = Some(Instant::now());
        let saved = self
            .book
            .to_json()
            .and_then(|json| crate::storage::write_atomic(&self.path, &json).context("saving the demo account"));
        match saved {
            Ok(()) => {
                self.save_error = None;
                true
            }
            Err(e) => {
                tracing::warn!("saving the demo account: {e:#}");
                self.save_error = Some(format!("{e:#}"));
                false
            }
        }
    }

    /// Retry a failed save every few seconds.
    fn retry_save(&mut self) {
        if self.save_error.is_some() && self.last_save_try.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.save();
        }
    }
}

impl TerminalApp {
    pub(super) fn demo_mode(&self) -> bool {
        self.prefs.trading.account_mode == AccountMode::Demo
    }

    /// The portfolio the panes show: the demo account, or the live (or sample) brokerage.
    pub(super) fn shown_portfolio(&self) -> Option<&Portfolio> {
        if self.demo_mode() { Some(&self.demo.pf) } else { self.broker.portfolio.as_deref() }
    }

    /// Keep quotes coming for demo positions and open orders, and expire Day orders.
    pub(super) fn demo_periodic(&mut self) {
        let had_error = self.demo.save_error.is_some();
        self.demo.retry_save();
        if had_error && self.demo.save_error.is_none() {
            self.trade_toast("Demo account saved", true);
        }
        let now = chrono::Utc::now();
        let expired = self.demo.book.expire(now);
        if !expired.is_empty() {
            self.demo.changed();
            self.demo_events(expired);
        }
        let watched = self.demo.book.watched();
        let wanted = self.demo_mode() || self.demo.book.orders.iter().any(Order::is_open);
        if !wanted || watched.is_empty() || self.demo.last_quotes.is_some_and(|t| t.elapsed() < QUOTES_EVERY) {
            return;
        }
        let keys: Vec<String> = watched.iter().filter_map(|s| market_key(s)).collect();
        if !keys.is_empty() {
            self.worker.send(Request::Quotes(keys));
        }
        self.demo.last_quotes = Some(Instant::now());
    }

    /// New quotes arrived: mark demo positions and fill resting demo orders they cross.
    pub(super) fn demo_on_quotes(&mut self, keys: &[String]) {
        let now = chrono::Utc::now();
        // Expire overdue Day orders before any quote can fill them.
        let mut events = self.demo.book.expire(now);
        let mut touched = !events.is_empty();
        for sym in self.demo.book.watched() {
            let Some(k) = market_key(&sym).filter(|k| keys.contains(k)) else { continue };
            let Some(q) = self.quotes.get(&k).map(|q| q.last).filter(|x| x.is_finite() && *x > 0.0) else { continue };
            events.extend(self.demo.book.on_price(&sym, q, now));
            touched = true;
        }
        if touched {
            self.demo.changed();
        }
        self.demo_events(events);
    }

    fn demo_events(&mut self, events: Vec<Event>) {
        for e in events {
            let ok = matches!(e, Event::Filled(_));
            self.trade_toast(format!("Demo: {}{}", e.message(), self.unsaved_note()), ok);
        }
    }

    /// ` (not saved yet: …)` while the demo book cannot be written.
    fn unsaved_note(&self) -> String {
        self.demo.save_error.as_ref().map_or(String::new(), |e| format!(" — not saved yet ({e}); retrying"))
    }

    /// Place a demo order now (one click: no preview, no confirmation).
    pub(super) fn demo_submit(&mut self, req: &OrderRequest, quote: Option<f64>) -> Result<Order, String> {
        let result = self.demo.book.submit(req, quote, chrono::Utc::now());
        match &result {
            Ok(o) => {
                self.demo.changed();
                let msg = match o.status.as_str() {
                    "EXECUTED" => tr_broker::demo::fill_message(o),
                    "CANCELED" => format!("{} cancelled (not fillable now)", tr_broker::demo::order_line(o)),
                    _ => format!("{} working", tr_broker::demo::order_line(o)),
                };
                let saved = self.demo.save_error.is_none();
                self.trade_toast(format!("Demo: {msg}{}", self.unsaved_note()), saved && o.status != "CANCELED");
            }
            Err(e) => self.trade_toast(format!("Demo order not placed: {e}"), false),
        }
        result
    }

    pub(super) fn demo_cancel(&mut self, id: &str) {
        match self.demo.book.cancel(id, chrono::Utc::now()) {
            Ok(o) => {
                self.demo.changed();
                let note = self.unsaved_note();
                self.trade_toast(format!("Demo: cancelled {}{note}", tr_broker::demo::order_line(&o)), note.is_empty());
            }
            Err(e) => self.trade_toast(format!("Could not cancel: {e}"), false),
        }
    }

    /// Sell a whole demo position at market.
    pub(super) fn demo_close(&mut self, symbol: &str) {
        let Some(req) = self.demo.book.close_request(symbol) else {
            return self.trade_toast(format!("No {symbol} shares to close (check open sell orders)"), false);
        };
        let quote = market_key(symbol).and_then(|k| self.quotes.get(&k)).map(|q| q.last);
        let _ = self.demo_submit(&req, quote);
    }

    pub(super) fn reset_demo(&mut self, cash: f64) {
        self.demo.book.reset(cash, chrono::Utc::now());
        self.demo.changed();
        self.demo.confirm_reset = false;
        if let Some(t) = &mut self.trading.ticket {
            t.error = None;
            t.done = None;
        }
        self.trade_toast(
            format!("Demo account reset to {}", super::portfolio_view::money(self.demo.book.cash, "USD")),
            true,
        );
    }

    /// Switch accounts. Going live asks first (see `live_dialog`); going back to the demo
    /// account is immediate. The choice is saved.
    pub(super) fn set_account_mode(&mut self, mode: AccountMode, confirmed: bool) {
        if mode == self.prefs.trading.account_mode {
            return;
        }
        if mode == AccountMode::Live && !confirmed {
            self.trading.live_prompt = true;
            return;
        }
        self.prefs.trading.account_mode = mode;
        self.trading.live_prompt = false;
        if let Err(e) = self.prefs.save(&super::prefs_path()) {
            self.status = format!("Could not save preferences: {e}");
        }
        // The ticket's account belongs to the other mode now.
        if let Some(t) = &mut self.trading.ticket {
            t.account = None;
            t.preview = None;
            t.confirming = false;
            t.review = false;
            t.error = None;
            t.done = None;
        }
        match mode {
            AccountMode::Demo => {
                self.trade_toast("Switched to the DEMO account: orders are simulated with virtual cash", true);
                if let Some(t) = &mut self.trading.ticket {
                    t.account = Some(tr_broker::demo::ACCOUNT_ID.into());
                }
            }
            AccountMode::Live => {
                self.trade_toast("Switched to LIVE accounts: orders go to your real brokerage", true);
                if self.broker.portfolio.as_ref().is_none_or(|p| p.demo) && !self.broker.loading {
                    self.refresh_portfolio();
                }
                self.ensure_capabilities(false);
            }
        }
    }
}
