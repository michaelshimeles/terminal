//! Brokerage accounts through SnapTrade: credential lifecycle (Keychain / private file,
//! environment overrides), user registration, the read-only Connection Portal, portfolio
//! refresh scheduling and the `PORT`/`ACCT`/`POS`/`ORD`/`ACTV` commands. Order entry is in
//! `trading`.

use std::time::Duration;

use tr_broker::{Credentials, Portfolio, Position, SecretStore};
use web_time::Instant;

use super::{PaneKind, TerminalApp};
use crate::command::PortView;
use crate::worker::{Request, data_dir};

/// Portfolio reload interval while a portfolio pane is visible (SnapTrade caches holdings
/// for most plans, so polling faster gains nothing).
const REFRESH: Duration = Duration::from_secs(120);
/// Faster polling for a few minutes after opening the Connection Portal, so newly connected
/// accounts appear without a manual refresh.
const CONNECT_POLL: Duration = Duration::from_secs(20);
const CONNECT_WINDOW: Duration = Duration::from_secs(300);

/// State of one portfolio pane.
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioPane {
    pub view: PortView,
    /// Restrict to one account id.
    pub account: Option<String>,
    /// Restrict positions/orders/activity to one market key (`AAPL POS`).
    pub symbol: Option<String>,
    pub sort: PosSort,
    /// Orders: only open ones.
    pub open_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PosSort {
    #[default]
    Value,
    Pnl,
    PnlPct,
    Symbol,
}

impl PortfolioPane {
    pub fn new(view: PortView) -> Self {
        Self { view, account: None, symbol: None, sort: PosSort::Value, open_only: false }
    }

    pub fn title(&self) -> String {
        match &self.symbol {
            Some(s) => format!("{} {}", tr_data::security(s).ticker, self.view.label()),
            None if self.view == PortView::Summary => "Portfolio".into(),
            None => self.view.label().into(),
        }
    }
}

/// Where the integration stands, for the setup screen and status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerMode {
    /// Offline sample data (`TR_PROVIDER=sample` or `TR_BROKER=sample`) without credentials.
    Simulated,
    /// Keys missing or incomplete: show the setup screen.
    Setup,
    /// Ready to read accounts.
    Live,
}

pub struct BrokerState {
    pub store: SecretStore,
    /// Effective credentials (stored values overridden by `SNAPTRADE_*`).
    pub creds: Credentials,
    /// Some fields come from the environment.
    pub from_env: bool,
    /// What is saved in the store (without environment overrides).
    pub saved: Credentials,
    /// Settings form buffer.
    pub form: Credentials,
    pub reveal: bool,
    pub portfolio: Option<Box<Portfolio>>,
    pub error: Option<String>,
    pub loading: bool,
    pub fetched: Option<Instant>,
    /// Feedback line in Settings › Brokerage: text and whether it is an error.
    pub message: Option<(String, bool)>,
    /// A request in flight from Settings ("Registering…").
    pub busy: Option<&'static str>,
    /// Last Connection Portal link, so it can be copied if no browser opened.
    pub portal_url: Option<String>,
    pub connecting_since: Option<Instant>,
    /// Sample portfolio requested (offline provider or `TR_BROKER=sample`).
    pub simulate: bool,
    pub last_quotes: Option<Instant>,
    /// "Test Connection" is waiting for a portfolio load.
    pub testing: bool,
    /// Connection awaiting "Disconnect" confirmation, and the one being removed.
    pub confirm_disconnect: Option<String>,
    pub disconnecting: Option<String>,
}

impl BrokerState {
    pub fn load(offline: bool) -> Self {
        Self::with_store(SecretStore::platform(&data_dir()), offline)
    }

    /// [`Self::load`] with credentials from `store` instead of the platform's.
    pub fn with_store(store: SecretStore, offline: bool) -> Self {
        let mut message = None;
        // The browser build can't reach SnapTrade, and keys don't belong in page storage.
        let stored = if cfg!(target_arch = "wasm32") { Ok(None) } else { store.load() };
        let stored = match stored {
            Ok(c) => c.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("SnapTrade credentials unavailable: {e:#}");
                message = Some((format!("Could not read saved credentials: {e:#}"), true));
                Credentials::default()
            }
        };
        let mut creds = stored.clone();
        let from_env = creds.apply_env(|k| std::env::var(k).ok());
        let simulate = offline
            || cfg!(target_arch = "wasm32")
            || std::env::var("TR_BROKER").is_ok_and(|v| v.eq_ignore_ascii_case("sample"));
        Self {
            store,
            saved: stored,
            form: creds.clone(),
            creds,
            from_env,
            reveal: false,
            portfolio: None,
            error: None,
            loading: false,
            fetched: None,
            message,
            busy: None,
            portal_url: None,
            connecting_since: None,
            simulate,
            last_quotes: None,
            testing: false,
            confirm_disconnect: None,
            disconnecting: None,
        }
    }

    pub fn mode(&self) -> BrokerMode {
        if self.creds.is_ready() {
            BrokerMode::Live
        } else if self.simulate {
            BrokerMode::Simulated
        } else {
            BrokerMode::Setup
        }
    }

    /// The terminal's own quote for a position, when it maps onto a market key (options are
    /// never repriced from their underlying).
    pub fn live_price(&self, quotes: &std::collections::HashMap<String, tr_data::Quote>, p: &Position) -> Option<f64> {
        if p.kind == tr_broker::InstrumentKind::Option {
            return None;
        }
        quotes.get(&p.market_key()?).map(|q| q.last).filter(|x| x.is_finite() && *x > 0.0)
    }
}

/// Open a URL or path with the system handler.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_external(target: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener).arg(target).spawn().map(|_| ())
}

/// Open a URL in a new browser tab.
#[cfg(target_arch = "wasm32")]
pub fn open_external(target: &str) -> std::io::Result<()> {
    let window = web_sys::window().ok_or_else(|| std::io::Error::other("no browser window"))?;
    window
        .open_with_url_and_target_and_features(target, "_blank", "noopener")
        .map(|_| ())
        .map_err(|_| std::io::Error::other("the browser blocked the new tab"))
}

/// A stable, non-identifying SnapTrade user id for this installation.
pub fn new_user_id() -> String {
    let nanos = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    // Browsers have no process ids.
    let pid = if cfg!(target_arch = "wasm32") { 0 } else { std::process::id() };
    let mix = nanos ^ (u128::from(pid) << 64);
    format!("terminal-{:016x}", (mix as u64) ^ ((mix >> 64) as u64))
}

impl TerminalApp {
    /// Request a portfolio load now.
    pub(super) fn refresh_portfolio(&mut self) {
        let b = &mut self.broker;
        let creds = match b.mode() {
            BrokerMode::Live => Some(b.creds.clone()),
            BrokerMode::Simulated => None,
            BrokerMode::Setup => return,
        };
        b.loading = true;
        b.fetched = Some(Instant::now());
        self.worker.send(Request::Portfolio(creds));
    }

    /// Periodic portfolio and quote refresh while a portfolio pane is visible.
    pub(super) fn broker_periodic(&mut self, visible: &[usize]) {
        let shown = visible.iter().any(|&i| matches!(self.panes[i].kind, PaneKind::Portfolio(_)));
        if !shown {
            return;
        }
        let b = &self.broker;
        let connecting = b.connecting_since.is_some_and(|t| t.elapsed() < CONNECT_WINDOW);
        let every = if connecting { CONNECT_POLL } else { REFRESH };
        // The demo account is local: live accounts load only while they are shown.
        let live = !self.demo_mode();
        if live && !b.loading && b.mode() != BrokerMode::Setup && b.fetched.is_none_or(|t| t.elapsed() > every) {
            self.refresh_portfolio();
        }
        // Reprice holdings with the terminal's own quotes.
        if let Some(pf) = self.shown_portfolio()
            && self.broker.last_quotes.is_none_or(|t| t.elapsed() > Duration::from_secs(20))
        {
            let mut keys: Vec<String> = pf
                .positions
                .iter()
                .filter(|p| p.kind != tr_broker::InstrumentKind::Option)
                .filter_map(Position::market_key)
                .collect();
            keys.sort();
            keys.dedup();
            if !keys.is_empty() {
                self.worker.send(Request::Quotes(keys));
            }
            self.broker.last_quotes = Some(Instant::now());
        }
    }

    pub(super) fn on_portfolio(&mut self, result: Result<Box<Portfolio>, String>) {
        let b = &mut self.broker;
        b.loading = false;
        if std::mem::take(&mut b.testing) {
            b.message = Some(match &result {
                Ok(pf) if pf.accounts.is_empty() => {
                    ("Credentials work. No brokerage connected yet: use Connect Brokerage.".into(), false)
                }
                Ok(pf) => (
                    format!(
                        "Connected: {} account{} at {}.",
                        pf.accounts.len(),
                        if pf.accounts.len() == 1 { "" } else { "s" },
                        {
                            let mut inst: Vec<&str> = pf.accounts.iter().map(|a| a.institution.as_str()).collect();
                            inst.sort();
                            inst.dedup();
                            inst.join(", ")
                        }
                    ),
                    false,
                ),
                Err(e) => (format!("Connection failed: {e}"), true),
            });
        }
        match result {
            Ok(pf) => {
                let n = pf.accounts.len();
                // New accounts after connecting: stop the fast polling.
                if b.connecting_since.is_some() && b.portfolio.as_ref().is_some_and(|old| n > old.accounts.len()) {
                    b.connecting_since = None;
                    self.status = format!("Brokerage connected: {n} account{}", if n == 1 { "" } else { "s" });
                }
                b.portfolio = Some(pf);
                b.error = None;
                b.last_quotes = None;
            }
            Err(e) => {
                tracing::warn!("portfolio: {e}");
                b.error = Some(e);
            }
        }
    }

    pub(super) fn on_broker_registered(&mut self, result: Result<(String, String), String>) {
        self.broker.busy = None;
        match result {
            Ok((id, secret)) => {
                self.broker.form.user_id = id;
                self.broker.form.user_secret = secret;
                // SnapTrade returns the secret only once: persist it immediately.
                self.save_broker_form("SnapTrade user registered and saved. Next: Connect Brokerage.");
            }
            Err(e) => self.broker.message = Some((format!("Registration failed: {e}"), true)),
        }
    }

    pub(super) fn on_broker_portal(&mut self, result: Result<String, String>) {
        self.broker.busy = None;
        match result {
            Ok(url) => {
                let opened = open_external(&url);
                self.broker.connecting_since = Some(Instant::now());
                self.broker.message = Some(match opened {
                    Ok(()) => ("Connection Portal opened in your browser (link valid 5 minutes, read-only access). Accounts appear here once connected.".into(), false),
                    Err(e) => (format!("Could not open a browser ({e}); copy the link below."), true),
                });
                self.status = "SnapTrade Connection Portal opened in the browser".into();
                self.broker.portal_url = Some(url);
            }
            Err(e) => self.broker.message = Some((format!("Could not open the Connection Portal: {e}"), true)),
        }
    }

    /// Persist the settings form and make it the effective credentials.
    pub(super) fn save_broker_form(&mut self, ok: &str) {
        let b = &mut self.broker;
        match b.store.save(&b.form) {
            Ok(()) => {
                b.saved = b.form.clone();
                let mut creds = b.form.clone();
                b.from_env = creds.apply_env(|k| std::env::var(k).ok());
                b.creds = creds;
                b.message = Some((ok.into(), false));
                b.portfolio = None;
                b.error = None;
                b.fetched = None;
            }
            Err(e) => b.message = Some((format!("Could not save credentials: {e:#}"), true)),
        }
    }

    pub(super) fn forget_broker_credentials(&mut self) {
        let b = &mut self.broker;
        match b.store.clear() {
            Ok(()) => {
                b.saved = Credentials::default();
                b.form = Credentials::default();
                let mut creds = Credentials::default();
                b.from_env = creds.apply_env(|k| std::env::var(k).ok());
                b.creds = creds;
                b.portfolio = None;
                b.error = None;
                b.fetched = None;
                b.portal_url = None;
                b.message = Some((
                    if b.from_env {
                        "Saved credentials removed (SNAPTRADE_* environment variables still apply).".into()
                    } else {
                        "Saved credentials removed from this computer.".into()
                    },
                    false,
                ));
            }
            Err(e) => b.message = Some((format!("Could not remove credentials: {e:#}"), true)),
        }
    }

    pub(super) fn register_snaptrade_user(&mut self) {
        let form = self.broker.form.clone();
        if !form.has_keys() {
            self.broker.message = Some(("Enter the client ID and consumer key first.".into(), true));
            return;
        }
        let user_id = if form.user_id.trim().is_empty() { new_user_id() } else { form.user_id.trim().to_string() };
        self.broker.busy = Some("Registering user…");
        self.broker.message = None;
        self.worker.send(Request::BrokerRegister { creds: form, user_id });
    }

    pub(super) fn connect_brokerage(&mut self) {
        self.open_connection_portal(None);
    }

    /// Re-authenticate a disabled connection in the Connection Portal.
    pub(super) fn reconnect_brokerage(&mut self, id: String) {
        self.open_connection_portal(Some(id));
    }

    /// Open Settings › Brokerage (from `PORT SETUP`, the sidebar, menus and empty states).
    pub(super) fn open_brokerage_settings(&mut self) {
        self.settings_tab = super::settings_view::SettingsTab::Brokerage;
        self.open_panel(PaneKind::Settings);
    }

    pub(super) fn disconnect_brokerage(&mut self, id: String) {
        self.broker.confirm_disconnect = None;
        if !self.broker.creds.is_ready() {
            return;
        }
        self.broker.disconnecting = Some(id.clone());
        self.broker.message = None;
        self.worker.send(Request::BrokerDisconnect { creds: self.broker.creds.clone(), id });
    }

    pub(super) fn on_broker_disconnected(&mut self, result: Result<String, String>) {
        self.broker.disconnecting = None;
        match result {
            Ok(id) => {
                let name = self
                    .broker
                    .portfolio
                    .as_ref()
                    .and_then(|pf| pf.connections.iter().find(|c| c.id == id))
                    .map_or("The brokerage".to_string(), |c| c.brokerage.clone());
                // Hide it right away; SnapTrade removes it in the background.
                if let Some(pf) = self.broker.portfolio.as_mut() {
                    pf.connections.retain(|c| c.id != id);
                    let gone: Vec<String> = pf.accounts_of(&id).map(|a| a.id.clone()).collect();
                    pf.accounts.retain(|a| a.connection_id != id);
                    pf.positions.retain(|p| !gone.contains(&p.account_id));
                    pf.orders.retain(|o| !gone.contains(&o.account_id));
                    pf.activities.retain(|a| !gone.contains(&a.account_id));
                }
                self.broker.message =
                    Some((format!("{name} disconnected. Its accounts were removed from SnapTrade."), false));
                self.status = format!("{name} disconnected");
            }
            Err(e) => self.broker.message = Some((format!("Could not disconnect: {e}"), true)),
        }
    }

    fn open_connection_portal(&mut self, reconnect: Option<String>) {
        if !self.broker.creds.is_ready() {
            self.broker.message =
                Some(("Save your SnapTrade keys and register a user before connecting a brokerage.".into(), true));
            self.settings_tab = super::settings_view::SettingsTab::Brokerage;
            self.open_panel(PaneKind::Settings);
            return;
        }
        let dark = self.applied_theme.as_ref().is_none_or(|t| t.dark);
        self.broker.busy = Some("Opening the Connection Portal…");
        self.broker.message = None;
        self.worker.send(Request::BrokerPortal { creds: self.broker.creds.clone(), dark, reconnect });
    }

    /// `PORT`, `ACCT`, `POS`, `ORD`, `ACTV` with an optional subcommand, optionally for a
    /// security (`AAPL POS`).
    pub(super) fn portfolio_command(&mut self, view: PortView, arg: &str, symbol: Option<String>) {
        match arg.trim() {
            "REFRESH" | "SYNC" | "RELOAD" => {
                if self.demo_mode() {
                    self.status =
                        "The demo account is local and always current (MODE LIVE for brokerage accounts)".into();
                } else if self.broker.mode() == BrokerMode::Setup {
                    self.status = "Brokerage not set up: PORT SETUP".into();
                } else {
                    self.refresh_portfolio();
                    self.status = "Reloading brokerage accounts…".into();
                }
                return;
            }
            "CONNECT" | "LINK" | "ADD" => return self.connect_brokerage(),
            "SETUP" | "SETTINGS" | "KEYS" | "CONFIG" => return self.open_brokerage_settings(),
            _ => {}
        }
        // Like other panels, this replaces the targeted portfolio pane or opens one.
        let mut pane = PortfolioPane::new(view);
        pane.symbol = symbol;
        self.open_panel(PaneKind::Portfolio(Box::new(pane)));
        if !self.demo_mode() && self.broker.portfolio.is_none() && !self.broker.loading {
            self.refresh_portfolio();
        }
    }
}

impl TerminalApp {
    /// Settings › Brokerage: the whole SnapTrade setup without terminal commands — status,
    /// keys, user registration, connected brokerages (connect, reconnect, disconnect) and a
    /// connection test.
    pub(super) fn brokerage_tab(&mut self, ui: &mut egui::Ui) {
        use egui::RichText;

        use crate::theme::{self, dp, mono, muted, text, ui_font};
        let section = |ui: &mut egui::Ui, title: &str| {
            ui.add_space(dp(12.0));
            ui.label(RichText::new(title).font(ui_font(13.5)).color(text()));
            ui.separator();
        };
        let note = |ui: &mut egui::Ui, s: &str| {
            ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
        };
        let mode = self.broker.mode();
        // Load accounts and connections the first time the tab is shown with working keys.
        if mode == BrokerMode::Live
            && self.broker.portfolio.as_ref().is_none_or(|p| p.simulated)
            && !self.broker.loading
            && self.broker.error.is_none()
        {
            self.refresh_portfolio();
        }
        let busy = self.broker.busy.is_some() || self.broker.disconnecting.is_some();

        let access_note = self.brokerage_access_note();
        // ---- status card ----
        ui.add_space(dp(4.0));
        // A snapshot (small, and only while this tab is open) so the UI below can mutate state.
        let live_pf = self.broker.portfolio.clone().filter(|p| !p.simulated);
        let live_pf = live_pf.as_deref();
        let (dot, headline, next) = match mode {
            BrokerMode::Setup | BrokerMode::Simulated if !self.broker.creds.has_keys() => (
                theme::amber(),
                "Not set up".to_string(),
                "Enter your SnapTrade client ID and consumer key below, then Save.",
            ),
            BrokerMode::Setup | BrokerMode::Simulated => (
                theme::amber(),
                "Keys saved, no SnapTrade user yet".to_string(),
                "Click Register User (commercial keys), or clear both user fields for personal keys.",
            ),
            BrokerMode::Live => match (&self.broker.error, live_pf) {
                _ if self.broker.loading && live_pf.is_none() => {
                    (theme::cyan(), "Checking the connection…".to_string(), "")
                }
                (Some(e), _) if !self.broker.creds.has_user() => (
                    theme::down(),
                    format!("Connection problem: {e}"),
                    "Commercial keys need a SnapTrade user: click Register User. Otherwise check the keys.",
                ),
                (Some(e), _) => {
                    (theme::down(), format!("Connection problem: {e}"), "Check the keys, then Test Connection.")
                }
                (None, Some(pf)) if pf.accounts.is_empty() => (
                    theme::up(),
                    "Credentials work · no brokerage connected yet".to_string(),
                    "Click Connect Brokerage… to sign in to your broker in the browser.",
                ),
                (None, Some(pf)) => {
                    let brokers = pf.connections.len().max(1);
                    let synced = pf.fetched.map_or(String::new(), |t| {
                        format!(" · synced {}", t.with_timezone(&chrono::Local).format("%H:%M"))
                    });
                    (
                        theme::up(),
                        format!(
                            "Connected · {} account{} at {} brokerage{}{synced}",
                            pf.accounts.len(),
                            if pf.accounts.len() == 1 { "" } else { "s" },
                            brokers,
                            if brokers == 1 { "" } else { "s" }
                        ),
                        "",
                    )
                }
                (None, None) => (theme::cyan(), "Keys saved".to_string(), "Click Test Connection."),
            },
        };
        egui::Frame::group(ui.style()).corner_radius(theme::CARD_RADIUS).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 4.5, dot);
                ui.label(RichText::new(&headline).font(ui_font(13.5)).color(text()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_test = self.broker.creds.is_ready() && !busy && !self.broker.loading;
                    if ui
                        .add_enabled(can_test, egui::Button::new("Test Connection"))
                        .on_hover_text("Load accounts and connections now")
                        .clicked()
                    {
                        self.broker.testing = true;
                        self.broker.message = None;
                        self.refresh_portfolio();
                    }
                    if self.broker.loading || busy {
                        ui.add(egui::Spinner::new().size(12.0));
                    }
                });
            });
            if !next.is_empty() {
                ui.label(RichText::new(format!("Next: {next}")).font(ui_font(12.0)).color(theme::accent()));
            }
            if mode == BrokerMode::Simulated {
                note(ui, "Until then the portfolio panes show a simulated sample portfolio.");
            }
            note(ui, &access_note);
        });
        if let Some((m, err)) = &self.broker.message {
            ui.label(RichText::new(m).font(ui_font(12.0)).color(if *err { theme::down() } else { theme::up() }));
        }
        if let Some(what) = self.broker.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                note(ui, what);
            });
        }

        // ---- credentials ----
        section(ui, "SnapTrade API keys");
        note(ui, "Create a client ID and consumer key in the SnapTrade dashboard (personal keys work too).");
        let reveal = self.broker.reveal;
        let field = |ui: &mut egui::Ui, label: &str, value: &mut String, secret: bool, hint: &str| {
            ui.horizontal(|ui| {
                ui.add_sized([130.0, 20.0], egui::Label::new(RichText::new(label).font(ui_font(12.5))));
                ui.add(
                    egui::TextEdit::singleline(value)
                        .password(secret && !reveal)
                        .hint_text(hint)
                        .font(mono(12.0))
                        .desired_width(330.0),
                );
            });
        };
        let form = &mut self.broker.form;
        field(ui, "Client ID", &mut form.client_id, false, "e.g. MY-APP-ID");
        field(ui, "Consumer key", &mut form.consumer_key, true, "secret");
        ui.horizontal(|ui| {
            ui.add_space(134.0);
            ui.checkbox(&mut self.broker.reveal, "Show secrets");
            if ui.link("Open the SnapTrade dashboard").clicked()
                && let Err(e) = open_external(tr_broker::snaptrade::DASHBOARD_URL)
            {
                self.status = format!("Could not open the browser: {e}");
            }
        });

        section(ui, "SnapTrade user");
        note(
            ui,
            "Commercial keys need one SnapTrade user for this computer: Register User creates it and saves its \
             secret (SnapTrade shows it only once). Personal keys: leave both fields empty.",
        );
        let form = &mut self.broker.form;
        field(ui, "User ID", &mut form.user_id, false, "created by Register User");
        field(ui, "User secret", &mut form.user_secret, true, "secret");

        let dirty = self.broker.form != self.broker.saved;
        ui.add_space(dp(4.0));
        let can_save = dirty && self.broker.form.is_ready() && !busy;
        let can_register = self.broker.form.has_keys() && self.broker.form.user_secret.trim().is_empty() && !busy;
        ui.horizontal_wrapped(|ui| {
            ui.add_space(134.0);
            let save = ui
                .add_enabled(can_save, egui::Button::new(RichText::new("Save").strong()))
                .on_hover_text(format!("Store in {}", self.broker.store.describe()))
                .on_disabled_hover_text(if !dirty {
                    "Nothing changed"
                } else {
                    "Enter both keys, and either both user fields or neither"
                });
            if save.clicked() {
                self.save_broker_form("Saved.");
                self.refresh_portfolio();
            }
            if ui
                .add_enabled(can_register, egui::Button::new("Register User"))
                .on_hover_text("Create a SnapTrade user for these keys and save its secret")
                .on_disabled_hover_text("Enter the keys first (and leave the user secret empty)")
                .clicked()
            {
                self.register_snaptrade_user();
            }
            if dirty && !busy && ui.button("Revert").on_hover_text("Discard unsaved changes").clicked() {
                self.broker.form = self.broker.creds.clone();
            }
        });
        note(
            ui,
            &format!("Stored in {}. Never written to the repository, layouts or logs.", self.broker.store.describe()),
        );
        if self.broker.from_env {
            ui.label(
                RichText::new("SNAPTRADE_* environment variables are set and override the saved values.")
                    .font(ui_font(11.5))
                    .color(theme::amber()),
            );
        }

        // ---- connected brokerages ----
        section(ui, "Connected brokerages");
        let ready = self.broker.creds.is_ready();
        let connections: Vec<tr_broker::Connection> = live_pf.map(|p| p.connections.clone()).unwrap_or_default();
        if !ready {
            note(ui, "Save your keys and register a user to connect a brokerage.");
        } else if live_pf.is_none() {
            note(ui, if self.broker.loading { "Loading…" } else { "Click Test Connection to load your connections." });
        } else if connections.is_empty() {
            note(ui, "No brokerages connected yet.");
        }
        let mut action: Option<(&'static str, String)> = None;
        for c in &connections {
            let accounts: Vec<String> =
                live_pf.map(|p| p.accounts_of(&c.id).map(|a| a.label()).collect()).unwrap_or_default();
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, if c.disabled { theme::down() } else { theme::up() });
                ui.label(RichText::new(&c.brokerage).font(ui_font(13.0)).color(text()));
                let mut detail = Vec::new();
                detail.push(if c.disabled { "needs reconnecting".to_string() } else { "active".to_string() });
                detail.push(match accounts.len() {
                    0 => "no accounts".into(),
                    1 => accounts[0].clone(),
                    n => format!("{n} accounts"),
                });
                if let Some(t) = c.created {
                    detail.push(format!("since {}", t.format("%b %Y")));
                }
                note(ui, &detail.join(" · "));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.broker.disconnecting.as_deref() == Some(&c.id) {
                        ui.add(egui::Spinner::new().size(12.0));
                        note(ui, "Disconnecting…");
                    } else if self.broker.confirm_disconnect.as_deref() == Some(&c.id) {
                        if ui.button("Cancel").clicked() {
                            action = Some(("cancel", c.id.clone()));
                        }
                        if ui
                            .button(RichText::new("Disconnect").color(theme::down()))
                            .on_hover_text("Removes this connection and its accounts from SnapTrade")
                            .clicked()
                        {
                            action = Some(("disconnect", c.id.clone()));
                        }
                    } else {
                        if ui.add_enabled(!busy, egui::Button::new("Disconnect")).clicked() {
                            action = Some(("confirm", c.id.clone()));
                        }
                        if c.disabled
                            && ui
                                .add_enabled(!busy, egui::Button::new("Reconnect…"))
                                .on_hover_text("Sign in to this broker again in the browser")
                                .clicked()
                        {
                            action = Some(("reconnect", c.id.clone()));
                        }
                    }
                });
            });
            if self.broker.confirm_disconnect.as_deref() == Some(&c.id) {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        RichText::new(format!(
                            "Disconnect {}? Its accounts are removed from SnapTrade and this can't be undone \
                             (you can connect it again later).",
                            c.brokerage
                        ))
                        .font(ui_font(11.5))
                        .color(theme::down()),
                    );
                });
            }
        }
        match action {
            Some(("confirm", id)) => self.broker.confirm_disconnect = Some(id),
            Some(("cancel", _)) => self.broker.confirm_disconnect = None,
            Some(("disconnect", id)) => self.disconnect_brokerage(id),
            Some(("reconnect", id)) => self.reconnect_brokerage(id),
            _ => {}
        }
        ui.add_space(dp(4.0));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ready && !busy, egui::Button::new(RichText::new("Connect Brokerage…").strong()))
                .on_hover_text("Opens SnapTrade's Connection Portal in your browser (read-only access)")
                .on_disabled_hover_text("Save your keys and register a user first")
                .clicked()
            {
                self.connect_brokerage();
            }
            if let Some(url) = self.broker.portal_url.clone() {
                note(ui, "Portal link (5 minutes):");
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(url.clone());
                }
                if ui.small_button("Open").clicked() {
                    let _ = open_external(&url);
                }
            }
        });
        note(
            ui,
            "After you finish in the browser, new accounts appear here and in the portfolio panes within a minute.",
        );

        // ---- remove ----
        section(ui, "Remove from this computer");
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy && (self.broker.saved != tr_broker::Credentials::default()),
                    egui::Button::new("Forget Credentials"),
                )
                .on_hover_text("Delete the saved keys and user secret (your brokerage connections stay at SnapTrade)")
                .clicked()
            {
                self.forget_broker_credentials();
            }
            note(ui, "Portfolio panes: PORT, ACCT, POS, ORD, ACTV (F7–F9) or the sidebar's Portfolio section.");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_ids_are_unique_and_anonymous() {
        let a = new_user_id();
        std::thread::sleep(Duration::from_millis(1));
        let b = new_user_id();
        assert!(a.starts_with("terminal-") && a.len() == "terminal-".len() + 16);
        assert_ne!(a, b);
    }

    #[test]
    fn pane_titles() {
        assert_eq!(PortfolioPane::new(PortView::Summary).title(), "Portfolio");
        assert_eq!(PortfolioPane::new(PortView::Orders).title(), "Orders");
        let mut p = PortfolioPane::new(PortView::Positions);
        p.symbol = Some("AAPL US Equity".into());
        assert_eq!(p.title(), "AAPL Positions");
    }
}
