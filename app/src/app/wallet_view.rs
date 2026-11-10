//! The built-in wallet's views: its part of Settings › Trading › Hyperliquid (with "Get
//! testnet USDC", the faucet), the Deposit window (address, QR code, testnet balances) and
//! the warning before a backup. Each shows the amber TESTNET badge.

use std::time::Duration;

use egui::{Color32, RichText, Ui};
use tr_hl::faucet::DRIP_USDC;
use tr_hl::nonce::now_ms;
use web_time::Instant;

use super::TerminalApp;
use super::faucet::{Tone, drip_message, thousands};
use super::hl_view::{masked, testnet_badge};
use super::portfolio_view::money;
use crate::prefs::HlPrefs;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};

/// A QR code of `data`, `side` points square, dark modules on white with a 4-module margin.
pub fn qr_code(ui: &mut Ui, data: &str, side: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    let Ok(code) = qrcode::QrCode::new(data.as_bytes()) else { return resp };
    let n = code.width();
    let colors = code.to_colors();
    let cell = side / (n + 8) as f32;
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, Color32::WHITE);
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let (x, y) = ((i % n) as f32 + 4.0, (i / n) as f32 + 4.0);
            let min = rect.min + egui::vec2(x * cell, y * cell);
            p.rect_filled(egui::Rect::from_min_size(min, egui::vec2(cell, cell)), 0.0, Color32::BLACK);
        }
    }
    resp
}

impl TerminalApp {
    /// Settings › Trading › Hyperliquid: the wallet (or a watched address) and the trading key.
    pub(super) fn wallet_section(&mut self, ui: &mut Ui, h: &HlPrefs) {
        let note = |ui: &mut Ui, s: &str| {
            ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
        };
        let busy = self.hl.busy.is_some();
        let signing_in = self.hl.wallet.signing_in.is_some();

        match (&h.wallet, &h.master) {
            (Some(w), _) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("Wallet {}", masked(&w.address))).font(mono(12.5)).color(text()));
                    self.stream_dot(ui);
                    if !w.user.is_empty() {
                        note(ui, &format!("· Turnkey · {}", w.user));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Deposit…")
                        .on_hover_text("The wallet's address, a QR code and its testnet balances")
                        .clicked()
                    {
                        self.wallet_open_deposit();
                    }
                    self.drip_button(ui);
                    if ui
                        .button("Back Up Wallet…")
                        .on_hover_text("Show the recovery phrase in Turnkey's own window")
                        .clicked()
                    {
                        self.hl.wallet.backup_prompt = true;
                        self.hl.wallet.backup_ack = false;
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Remove").small())
                        .on_hover_text("Stop showing this wallet here (the trading key stays unless revoked)")
                        .clicked()
                    {
                        self.hl_disconnect();
                    }
                });
                self.drip_status(ui, &w.address);
                ui.horizontal_wrapped(|ui| match self.hl.wallet.session_minutes() {
                    Some(m) => {
                        note(ui, &format!("Signed in to Turnkey for {m} more min."));
                        if ui.add(egui::Button::new("End Session").small()).clicked() {
                            self.wallet_end_session();
                        }
                    }
                    None => {
                        note(ui, "Not signed in to Turnkey. You only need to be to approve or revoke a trading key.")
                    }
                });
            }
            (None, Some(m)) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("Watching {} (read-only)", masked(m))).font(mono(12.5)).color(text()),
                    );
                    self.stream_dot(ui);
                    if ui
                        .add_enabled(!busy, egui::Button::new("Remove").small())
                        .on_hover_text("Stop showing this account")
                        .clicked()
                    {
                        self.hl_disconnect();
                    }
                });
                self.sign_in_button(ui, signing_in, "Create Wallet / Sign In…");
            }
            (None, None) => {
                self.sign_in_button(ui, signing_in, "Create Wallet / Sign In…");
                note(
                    ui,
                    "Opens Terminal's sign-in page in your browser. Sign in with an email code or a passkey; a new account gets a wallet held by Turnkey. Terminal never sees the wallet's key, and you never paste one.",
                );
                egui::CollapsingHeader::new(RichText::new("Watch an address (read-only)").font(ui_font(12.0)))
                    .id_salt("hl-watch")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.hl.connect_form)
                                    .hint_text("0x… (testnet address)")
                                    .font(mono(12.0))
                                    .desired_width(dp(330.0)),
                            );
                            if ui.button("Show Account").on_hover_text("Read-only: no key involved").clicked() {
                                let a = self.hl.connect_form.clone();
                                self.hl_connect(&a);
                            }
                        });
                        note(ui, "Fills PORT, POS, ORD and ACTV from Hyperliquid testnet. Nothing is signed.");
                    });
            }
        }

        if let Some(s) = &self.hl.wallet.signing_in {
            let waited = s.started.elapsed().as_secs() / 60;
            let reopen = s.url.is_some();
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                note(
                    ui,
                    &format!("Finish signing in in your browser (terminal.rasmic.xyz/wallet). {waited} min so far, 10 at most."),
                );
                if reopen && ui.add(egui::Button::new("Open Again").small()).clicked() {
                    self.wallet_reopen();
                }
                if ui.add(egui::Button::new("Cancel").small()).clicked() {
                    self.wallet_cancel_sign_in();
                }
            });
        }

        // The trading (agent) key.
        ui.add_space(dp(6.0));
        match &h.agent {
            Some(a) => {
                let days = (a.valid_until.saturating_sub(now_ms())) / (24 * 3600 * 1000);
                let expiry = chrono::DateTime::from_timestamp_millis(a.valid_until as i64)
                    .map(|t| t.with_timezone(&chrono::Local).format("%d %b %Y").to_string())
                    .unwrap_or_default();
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Trading key {} · {}",
                            tr_hl::trader::agent_base_name(&a.name),
                            masked(&a.address)
                        ))
                        .font(mono(12.0))
                        .color(text()),
                    );
                    note(ui, &format!("· expires {expiry} · key in {}", self.hl.store.describe()));
                });
                if days < 7 {
                    ui.label(
                        RichText::new(format!(
                            "The trading key expires in {days} days: revoke it and approve a new one."
                        ))
                        .font(ui_font(12.0))
                        .color(amber()),
                    );
                }
                if self.hl.key.is_none() && !self.hl.key_loading {
                    ui.label(
                        RichText::new("This device has no key for this trading key.")
                            .font(ui_font(12.0))
                            .color(amber()),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if h.wallet.is_some()
                        && ui
                            .add_enabled(!busy && !signing_in, egui::Button::new("Revoke on Hyperliquid"))
                            .on_hover_text(
                                "The wallet replaces this key on Hyperliquid with one nobody holds: one Turnkey signature, after a sign-in if needed",
                            )
                            .clicked()
                    {
                        self.wallet_revoke();
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Delete Key Here"))
                        .on_hover_text("Terminal can't sign with it any more; it stays approved on Hyperliquid until it expires or is revoked")
                        .clicked()
                    {
                        self.hl_delete_agent_key();
                    }
                });
            }
            None if h.wallet.is_some() => {
                let go = ui
                    .add_enabled(
                        !busy && !signing_in && self.hl.store_error.is_none(),
                        egui::Button::new(RichText::new("Approve Trading Key").strong()),
                    )
                    .on_hover_text("Signs in to Turnkey first if there's no session")
                    .clicked();
                if go {
                    self.wallet_approve();
                }
                note(
                    ui,
                    "Terminal makes a trading key on this device, and your wallet signs approveAgent for it once on Turnkey (one signature). It is named terminal-<device> and valid 180 days. It can trade but not withdraw. The wallet itself never signs orders.",
                );
            }
            None => {}
        }
    }

    /// "Get testnet USDC". Off while a request is out and for a minute after the faucet answers.
    fn drip_button(&mut self, ui: &mut Ui) {
        let drip = &self.hl.wallet.drip;
        let asking = drip.in_flight();
        let left = drip.cooldown_left(Instant::now());
        let label = match left {
            Some(d) => format!("Get testnet USDC ({} s)", d.as_secs() + u64::from(d.subsec_nanos() > 0)),
            None => "Get testnet USDC".to_string(),
        };
        let hover = format!(
            "Asks Hyperliquid's testnet faucet for {} test USDC for this wallet. Testnet only, and nothing is signed. The faucet pays only addresses that have deposited on Hyperliquid mainnet.",
            thousands(DRIP_USDC)
        );
        let waiting = if asking { "Asking the faucet…" } else { "You can ask again when the countdown ends" };
        if ui
            .add_enabled(!asking && left.is_none(), egui::Button::new(label))
            .on_hover_text(hover)
            .on_disabled_hover_text(waiting)
            .clicked()
        {
            self.wallet_claim_drip();
        }
        if left.is_some() {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
    }

    /// The faucet request in flight, or what the faucet last said for `address`.
    fn drip_status(&self, ui: &mut Ui, address: &str) {
        let drip = &self.hl.wallet.drip;
        if drip.in_flight() {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("Asking Hyperliquid's testnet faucet…").font(ui_font(11.5)).color(muted()));
            });
        } else if let Some(outcome) = drip.last_for(address) {
            let (message, tone) = drip_message(outcome);
            let color = match tone {
                Tone::Good => theme::up(),
                Tone::Refused => amber(),
                Tone::Failed => theme::down(),
            };
            ui.label(RichText::new(message).font(ui_font(12.0)).color(color));
        }
    }

    fn sign_in_button(&mut self, ui: &mut Ui, signing_in: bool, label: &str) {
        let b = egui::Button::new(RichText::new(label).strong().color(Color32::BLACK)).fill(amber());
        if ui.add_enabled(!signing_in && self.hl.busy.is_none(), b).clicked() {
            self.wallet_sign_in(None);
        }
    }

    fn stream_dot(&self, ui: &mut Ui) {
        let (dot, status) = if self.hl.connected {
            (theme::up(), self.hl.stream_status.clone())
        } else if self.hl.stream_status.is_empty() {
            (amber(), "Connecting…".to_string())
        } else {
            (amber(), self.hl.stream_status.clone())
        };
        let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, dot);
        ui.label(RichText::new(status).font(ui_font(11.5)).color(muted()));
        if let Some(u) = &self.hl.user {
            ui.label(
                RichText::new(format!("· value {}", money(u.account_value(), "USD")))
                    .font(ui_font(11.5))
                    .color(muted()),
            );
        }
    }

    /// The Deposit window and the backup warning, when open.
    pub(super) fn wallet_windows(&mut self, ctx: &egui::Context) {
        if self.hl.wallet.deposit {
            self.deposit_window(ctx);
        }
        if self.hl.wallet.backup_prompt {
            self.backup_dialog(ctx);
        }
    }

    fn deposit_window(&mut self, ctx: &egui::Context) {
        let Some(w) = self.wallet().cloned() else {
            self.hl.wallet.deposit = false;
            return;
        };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("wallet-deposit")).show(ctx, |ui| {
            ui.set_width(dp(440.0));
            ui.horizontal(|ui| {
                testnet_badge(ui);
                ui.label(RichText::new("Deposit").font(ui_font(16.0)).strong());
            });
            ui.add_space(dp(4.0));
            ui.label(
                RichText::new(
                    "Hyperliquid testnet only. Send test USDC on Hyperliquid testnet to this address. Never send real funds: this build can't reach mainnet.",
                )
                .font(ui_font(12.0))
                .color(amber()),
            );
            ui.add_space(dp(8.0));
            ui.vertical_centered(|ui| {
                qr_code(ui, &w.address, dp(196.0)).on_hover_text("Your wallet's address");
            });
            ui.add_space(dp(8.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new(&w.address).font(mono(12.0)).color(text()));
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(w.address.clone());
                    self.trade_toast("Wallet address copied", true);
                }
            });
            ui.add_space(dp(6.0));
            let user = self.hl.user.clone();
            egui::Grid::new("wallet-balances").num_columns(2).spacing([dp(24.0), dp(4.0)]).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).font(ui_font(12.0)).color(muted()));
                    ui.label(RichText::new(v).font(mono(12.5)).color(text()));
                    ui.end_row();
                };
                let perp = |f: fn(&tr_hl::stream::UserState) -> f64| {
                    user.as_deref().map(|u| money(f(u), "USD")).unwrap_or_else(|| "loading…".into())
                };
                row(ui, "Perp account value", perp(|u| u.account_value()));
                row(ui, "Withdrawable", perp(|u| u.withdrawable()));
                let spot = match &self.hl.wallet.spot {
                    Some((_, Ok(s))) if s.hold > 0.0 => format!("{} ({} on hold)", money(s.total, "USD"), money(s.hold, "USD")),
                    Some((_, Ok(s))) => money(s.total, "USD"),
                    Some((_, Err(e))) => format!("unavailable ({})", tr_hl::mask_addresses(e)),
                    None => "loading…".into(),
                };
                row(ui, "Spot USDC", spot);
            });
            ui.add_space(dp(6.0));
            ui.label(
                RichText::new("Test USDC comes from Hyperliquid's testnet faucet or a transfer from another testnet account.")
                    .font(ui_font(11.5))
                    .color(muted()),
            );
            ui.add_space(dp(6.0));
            if ui.button("Close").clicked() {
                close = true;
            }
        });
        if close || modal.should_close() {
            self.hl.wallet.deposit = false;
        }
    }

    fn backup_dialog(&mut self, ctx: &egui::Context) {
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("wallet-backup")).show(ctx, |ui| {
            ui.set_width(dp(450.0));
            ui.horizontal(|ui| {
                testnet_badge(ui);
                ui.label(RichText::new("Back up your wallet?").font(ui_font(16.0)).strong());
            });
            ui.add_space(dp(4.0));
            for line in [
                "Your recovery phrase controls the wallet. Anyone who has it can take everything in it.",
                "You'll sign in again on terminal.rasmic.xyz, and Turnkey shows the phrase in its own secure frame there. Terminal and Goshen never see it and can't recover it for you.",
                "Write it down offline. Never type it into a website, and never share it, not even with support.",
            ] {
                ui.label(RichText::new(format!("• {line}")).font(ui_font(12.0)).color(text()));
            }
            ui.add_space(dp(6.0));
            ui.checkbox(&mut self.hl.wallet.backup_ack, "I'm somewhere private, and I understand the above");
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
                let on = self.hl.wallet.backup_ack;
                let b = if on {
                    egui::Button::new(RichText::new("Open the Backup Page").strong().color(Color32::BLACK)).fill(amber())
                } else {
                    egui::Button::new(RichText::new("Open the Backup Page").strong())
                };
                if ui.add_enabled(on, b).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        if let Some(go) = choice {
            self.hl.wallet.backup_prompt = false;
            self.hl.wallet.backup_ack = false;
            if go {
                self.wallet_backup();
            }
        }
    }
}
