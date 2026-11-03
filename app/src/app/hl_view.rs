//! Hyperliquid's windows: the ticket body for a Hyperliquid account, the confirmations,
//! the enable dialog and Settings › Trading › Hyperliquid. Every one shows the amber
//! TESTNET badge.

use egui::{Color32, RichText, Ui};
use tr_broker::trading::Side;
use tr_hl::actions::Tif;
use tr_hl::caps::{MAX_LEVERAGE, MAX_MARKET_SLIPPAGE, MAX_ORDER_NOTIONAL_USD};
use tr_hl::nonce::now_ms;
use tr_hl::rounding::to_wire;
use tr_hl::trader::{EXPIRES_AFTER_MS, OrderKind};

use super::TerminalApp;
use super::hl::{HlConfirm, HlKind, intent, is_hl_account, resolve_coin, settle_size};
use super::portfolio_view::{money, price, qty};
use super::settings_view::SettingsCmd;
use super::trading::TicketAct;
use crate::prefs::AccountMode;
use crate::theme::{self, amber, dp, mono, muted, text, ui_font};
use crate::widgets;

/// The amber TESTNET pill.
pub fn testnet_badge(ui: &mut Ui) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new("TESTNET").font(mono(11.0)).strong().color(Color32::BLACK))
            .fill(amber())
            .corner_radius(9.0)
            .sense(egui::Sense::hover()),
    )
    .on_hover_text("Hyperliquid testnet: test funds only. This build cannot reach mainnet.")
}

pub(super) fn masked(addr: &str) -> String {
    tr_hl::key::parse_address(addr).map(|a| tr_hl::mask_address(&a)).unwrap_or_else(|_| addr.to_string())
}

/// The pasted main key's field (its state is wiped once the key has signed).
#[cfg(not(target_arch = "wasm32"))]
const MAIN_KEY_FIELD: &str = "hl-main-key";

const HIP3_RISK: &str = "A leveraged perpetual future with funding, priced by the deployer's oracle. It can diverge from the \
listed stock (off-hours, opens, earnings, corporate actions), and the deployer can halt the market and settle it at mark.";

impl TerminalApp {
    /// The ticket's body when a Hyperliquid account is selected.
    pub(super) fn hl_ticket_ui(&mut self, ui: &mut Ui) -> Option<TicketAct> {
        let accounts = self.trade_accounts_pub();
        let blocker = self.hl_blocker();
        let reg = self.hl.registry.clone();
        let sending = self.hl.sending.is_some();
        let coin = self
            .trading
            .ticket
            .as_ref()
            .and_then(|t| reg.as_ref().map(|r| resolve_coin(&t.symbol, r)))
            .unwrap_or_else(|| Err("Loading Hyperliquid testnet markets…".into()));
        let ctx = coin.as_ref().map(|c| self.hl.ctx(c)).unwrap_or_default();
        let asset = coin.as_ref().ok().and_then(|c| reg.as_ref()?.get(c).cloned());
        let t = self.trading.ticket.as_mut()?;
        let mut act = None;
        let mut review = false;
        let mut leverage = None;
        let mut settings = false;
        ui.spacing_mut().item_spacing.y = dp(6.0);

        ui.horizontal(|ui| {
            if let Some(i) = widgets::segmented(ui, "ticket-mode", &["DEMO", "LIVE"], Some(1), 22.0)
                && i == 0
                && !sending
            {
                act = Some(TicketAct::Mode(AccountMode::Demo));
            }
            testnet_badge(ui);
            ui.label(RichText::new("Hyperliquid · test funds only").font(ui_font(12.0)).color(amber()));
        });
        let current = t
            .account
            .as_ref()
            .and_then(|id| accounts.iter().find(|a| &a.id == id))
            .map_or("Choose an account…".to_string(), |a| format!("{} {}", a.label(), a.masked_number()));
        ui.add_enabled_ui(!sending, |ui| {
            egui::ComboBox::from_id_salt("hl-ticket-account")
                .width(ui.available_width())
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for a in &accounts {
                        let label = if is_hl_account(&a.id) {
                            format!("{} {} · TESTNET", a.label(), a.masked_number())
                        } else {
                            format!("{} · {} {}", a.label(), a.institution, a.masked_number())
                        };
                        ui.selectable_value(&mut t.account, Some(a.id.clone()), label);
                    }
                });
        });
        if let Some(b) = &blocker {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(b).font(ui_font(11.5)).color(amber()));
                if ui.small_button("Settings…").clicked() {
                    settings = true;
                }
            });
        }

        ui.horizontal(|ui| {
            let w = (ui.available_width() - dp(6.0)) / 2.0;
            for (side, color) in [(Side::Buy, theme::up()), (Side::Sell, theme::down())] {
                let b = if t.side == side {
                    egui::Button::new(RichText::new(side.label()).strong().color(Color32::WHITE)).fill(color)
                } else {
                    egui::Button::new(RichText::new(side.label()).color(color))
                };
                if ui.add_enabled(!sending, b.min_size(egui::vec2(w, dp(26.0)))).clicked() {
                    t.side = side;
                }
            }
        });
        let label = |ui: &mut Ui, s: &str| {
            ui.add_sized([dp(64.0), dp(20.0)], egui::Label::new(RichText::new(s).font(ui_font(12.0)).color(muted())));
        };
        ui.horizontal(|ui| {
            label(ui, "Market");
            let r = ui.add_enabled(
                !sending,
                egui::TextEdit::singleline(&mut t.symbol)
                    .hint_text("BTC, xyz:AAPL")
                    .font(mono(13.0))
                    .desired_width(dp(96.0)),
            );
            if r.changed() {
                t.error = None;
                // If the size came from dollars, size the amount again for the new market.
                if let Some(d) = t.hl.from_dollars.take() {
                    t.dollars = true;
                    t.qty = to_wire(d).unwrap_or_default();
                }
            }
            match ctx.mid {
                Some(m) => {
                    ui.label(RichText::new(price(m)).font(mono(12.5)).color(text()))
                        .on_hover_text("Hyperliquid testnet mid (not the Terminal server's mainnet quote)");
                }
                None if coin.is_ok() => {
                    ui.label(RichText::new("testnet mid…").font(ui_font(11.5)).color(muted()));
                }
                None => {}
            }
        });
        match (&coin, &asset) {
            (Ok(c), Some(a)) => {
                let kind = if a.is_hip3() { "perpetual, not a share" } else { "perpetual" };
                ui.horizontal(|ui| {
                    ui.add_space(dp(70.0));
                    ui.label(
                        RichText::new(format!("{} ({kind})", a.label())).font(mono(11.5)).strong().color(theme::cyan()),
                    )
                    .on_hover_text(format!("{c}: asset {} on Hyperliquid testnet", a.id));
                });
            }
            (Err(e), _) if !t.symbol.trim().is_empty() => {
                ui.label(RichText::new(e).font(ui_font(11.5)).color(amber()));
            }
            _ => {}
        }
        let mut unsized_note = None;
        ui.horizontal(|ui| {
            label(ui, if t.dollars { "Amount" } else { "Size" });
            let r = ui.add_enabled(
                !sending,
                egui::TextEdit::singleline(&mut t.qty)
                    .hint_text(if t.dollars { "$" } else { "units" })
                    .font(mono(13.0))
                    .desired_width(dp(84.0)),
            );
            if r.changed() {
                t.hl.from_dollars = None;
            }
            if t.focus_qty > 0 {
                t.focus_qty -= 1;
                if !r.has_focus() {
                    r.request_focus();
                }
            }
            // `BUY xyz:AAPL $50` gets its size from the testnet mid. It is never 50 contracts.
            unsized_note = settle_size(t, ctx.mid, asset.as_ref());
            if let (false, Some(m), Ok(q)) = (t.dollars, ctx.mid, t.qty.trim().parse::<f64>()) {
                let from = t.hl.from_dollars.map_or(String::new(), |d| format!(" (from ${d}, at the testnet mid)"));
                ui.label(RichText::new(format!("≈ {}{from}", money(q * m, "USD"))).font(mono(11.5)).color(muted()));
            }
        });
        if let Some(n) = unsized_note {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(dp(70.0));
                ui.label(RichText::new(n).font(ui_font(11.5)).color(amber()));
            });
        }
        ui.horizontal(|ui| {
            label(ui, "Type");
            let labels: Vec<&str> = HlKind::ALL.iter().map(|k| k.label()).collect();
            let sel = HlKind::ALL.iter().position(|k| *k == t.hl.kind);
            if let Some(i) = widgets::segmented(ui, "hl-ticket-type", &labels, sel, 20.0)
                && !sending
            {
                t.hl.kind = HlKind::ALL[i];
                if t.hl.kind == HlKind::Limit
                    && t.limit.trim().is_empty()
                    && let Some(m) = ctx.mid
                {
                    t.limit = to_wire(tr_hl::rounding::round_price(
                        m,
                        asset.as_ref().map_or(0, |a| a.sz_decimals),
                        tr_hl::rounding::Round::Nearest,
                    ))
                    .unwrap_or_default();
                }
            }
        });
        match t.hl.kind {
            HlKind::Limit => {
                ui.horizontal(|ui| {
                    label(ui, "Limit");
                    ui.add_enabled(
                        !sending,
                        egui::TextEdit::singleline(&mut t.limit)
                            .hint_text("price")
                            .font(mono(13.0))
                            .desired_width(dp(84.0)),
                    );
                    if let Some(m) = ctx.mid
                        && ui.small_button("Mid").on_hover_text("Use the testnet mid").clicked()
                    {
                        t.limit = format!("{m}");
                    }
                });
                ui.horizontal(|ui| {
                    label(ui, "Time");
                    let tifs = [Tif::Gtc, Tif::Ioc, Tif::Alo];
                    let labels: Vec<&str> = tifs.iter().map(|x| x.label()).collect();
                    if let Some(i) =
                        widgets::segmented(ui, "hl-ticket-tif", &labels, tifs.iter().position(|x| *x == t.hl.tif), 20.0)
                        && !sending
                    {
                        t.hl.tif = tifs[i];
                    }
                });
            }
            HlKind::StopLoss | HlKind::TakeProfit => {
                ui.horizontal(|ui| {
                    label(ui, "Trigger");
                    ui.add_enabled(
                        !sending,
                        egui::TextEdit::singleline(&mut t.hl.trigger)
                            .hint_text("price")
                            .font(mono(13.0))
                            .desired_width(dp(84.0)),
                    );
                    ui.label(
                        RichText::new("market order when it trades, reduce-only").font(ui_font(11.0)).color(muted()),
                    );
                });
            }
            HlKind::Market => {
                ui.horizontal(|ui| {
                    ui.add_space(dp(70.0));
                    ui.label(
                        RichText::new(format!("IOC limit {:.0}% through the testnet mid", MAX_MARKET_SLIPPAGE * 100.0))
                            .font(ui_font(11.0))
                            .color(muted()),
                    );
                });
            }
        }
        ui.horizontal(|ui| {
            ui.add_space(dp(70.0));
            let forced = matches!(t.hl.kind, HlKind::StopLoss | HlKind::TakeProfit);
            let mut ro = t.hl.reduce_only || forced;
            if ui.add_enabled(!forced && !sending, egui::Checkbox::new(&mut ro, "Reduce only")).changed() {
                t.hl.reduce_only = ro;
            }
        });
        if let (Ok(c), Some(a)) = (&coin, &asset) {
            ui.horizontal_wrapped(|ui| {
                label(ui, "Leverage");
                match ctx.leverage {
                    Some(l) => {
                        let over = l.value > MAX_LEVERAGE;
                        ui.label(
                            RichText::new(format!("{}× {}", l.value, if l.cross { "cross" } else { "isolated" }))
                                .font(mono(12.0))
                                .color(if over { amber() } else { text() }),
                        );
                        if over {
                            ui.label(
                                RichText::new(format!("over this build's {MAX_LEVERAGE}× limit"))
                                    .font(ui_font(11.0))
                                    .color(amber()),
                            );
                        }
                    }
                    None => {
                        ui.label(RichText::new("reading…").font(ui_font(11.5)).color(muted()));
                    }
                }
                let target = MAX_LEVERAGE.min(a.max_leverage);
                if ctx.leverage.is_none_or(|l| l.value != target)
                    && blocker.is_none()
                    && ui
                        .add_enabled(!sending, egui::Button::new(format!("Set {target}×")))
                        .on_hover_text("Asks for confirmation, then signs updateLeverage")
                        .clicked()
                {
                    leverage = Some((c.clone(), target));
                }
            });
        }

        ui.separator();
        let planned = reg.as_ref().map(|r| {
            intent(t, r).and_then(|i| {
                tr_hl::trader::plan_order(r, &i, &ctx, &tr_hl::actions::cloid(1)).map_err(|e| e.to_string())
            })
        });
        let row = |ui: &mut Ui, k: &str, v: String, strong: bool| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [dp(110.0), dp(16.0)],
                    egui::Label::new(RichText::new(k).font(ui_font(11.5)).color(muted())),
                );
                let rt = RichText::new(v).font(mono(if strong { 13.0 } else { 12.0 })).color(text());
                ui.label(if strong { rt.strong() } else { rt });
            });
        };
        if let Some(Ok(p)) = &planned {
            row(ui, "Est. value", money(p.notional(), "USD"), true);
            row(
                ui,
                "Sends",
                format!("{} @ {}", to_wire(p.size()).unwrap_or_default(), to_wire(p.price()).unwrap_or_default()),
                false,
            );
        }
        row(
            ui,
            "Limits",
            format!(
                "${MAX_ORDER_NOTIONAL_USD:.0}/order · {MAX_LEVERAGE}× · {:.0}% slippage",
                MAX_MARKET_SLIPPAGE * 100.0
            ),
            false,
        );
        if asset.as_ref().is_some_and(|a| a.is_hip3()) {
            let coin = asset.as_ref().map(|a| a.coin.clone()).unwrap_or_default();
            let under = coin.split(':').nth(1).unwrap_or_default().to_string();
            ui.label(
                RichText::new(format!("⚠ Perpetual future on Hyperliquid (xyz). Not a share of {under}."))
                    .font(ui_font(11.5))
                    .strong()
                    .color(amber()),
            );
        }
        if let Some(e) = &t.error {
            ui.label(RichText::new(e).font(ui_font(12.0)).color(theme::down()));
        } else if let Some(Err(e)) = &planned
            && !t.qty.trim().is_empty()
        {
            ui.label(RichText::new(e).font(ui_font(11.5)).color(amber()));
        }
        if let Some(d) = &t.done {
            ui.label(RichText::new(d).font(ui_font(12.0)).color(theme::up()));
        }
        let can = blocker.is_none() && !sending && matches!(&planned, Some(Ok(_)));
        let fill = if t.side == Side::Buy { theme::up() } else { theme::down() };
        ui.horizontal(|ui| {
            let label = format!("Review {}…", t.side.label());
            let b = if can {
                egui::Button::new(RichText::new(&label).strong().color(Color32::WHITE)).fill(fill)
            } else {
                egui::Button::new(RichText::new(&label).strong())
            };
            // Review stays clickable with a refusal, so the cap's reason shows in the ticket.
            let clickable = blocker.is_none() && !sending;
            if ui
                .add_enabled(clickable, b.min_size(egui::vec2(ui.available_width() - dp(24.0), dp(30.0))))
                .on_hover_text("Rounds the order, checks the limits, then asks you to confirm")
                .clicked()
            {
                review = true;
            }
            if sending {
                ui.add(egui::Spinner::new().size(14.0));
            }
        });
        ui.label(
            RichText::new(format!(
                "Signed on this device by the agent key and sent to Hyperliquid testnet; valid {} s.",
                EXPIRES_AFTER_MS / 1000
            ))
            .font(ui_font(11.0))
            .color(muted()),
        );
        if review {
            self.hl_review();
        }
        if let Some((c, l)) = leverage {
            self.hl_request_leverage(&c, l);
        }
        if settings {
            self.settings_tab = super::settings_view::SettingsTab::Trading;
            self.open_panel(super::PaneKind::Settings);
        }
        act
    }

    /// The one confirmation for every Hyperliquid action.
    pub(super) fn hl_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = self.hl.confirm.clone() else { return };
        // Editing the ticket after Review invalidates the confirmation.
        if let HlConfirm::Order { ticket, plan, .. } = &c {
            let same = self
                .trading
                .ticket
                .as_ref()
                .filter(|t| t.id == *ticket)
                .zip(self.hl.registry.as_ref())
                .is_some_and(|(t, r)| intent(t, r).is_ok_and(|i| &i == plan.intent()));
            if !same {
                self.hl.confirm = None;
                return;
            }
        }
        let master = self.hl_master().map(|m| masked(&m)).unwrap_or_default();
        let agent = self
            .prefs
            .trading
            .hl
            .agent
            .as_ref()
            .map(|a| tr_hl::trader::agent_base_name(&a.name).to_string())
            .unwrap_or_default();
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("hl-confirm")).show(ctx, |ui| {
            ui.set_width(dp(430.0));
            let grid_row = |ui: &mut Ui, k: &str, v: RichText| {
                ui.label(RichText::new(k).font(ui_font(12.0)).color(muted()));
                ui.label(v);
                ui.end_row();
            };
            let plain = |s: String| RichText::new(s).font(ui_font(12.5)).color(text());
            let (title, go, go_color) = match &c {
                HlConfirm::Order { plan, modify, .. } => {
                    let side = if plan.intent().is_buy { "buy" } else { "sell" };
                    let t =
                        if modify.is_some() { format!("Confirm modify ({side})") } else { format!("Confirm {side}") };
                    let color = if plan.intent().is_buy { theme::up() } else { theme::down() };
                    (
                        t,
                        format!(
                            "{} {} — Sign & send",
                            if plan.intent().is_buy { "Buy" } else { "Sell" },
                            plan.asset().coin
                        ),
                        color,
                    )
                }
                HlConfirm::Cancel { .. } => {
                    ("Cancel order?".to_string(), "Cancel order — Sign & send".to_string(), theme::down())
                }
                HlConfirm::Kill { .. } => {
                    ("Kill switch".to_string(), "Cancel all and turn orders off".to_string(), theme::down())
                }
                HlConfirm::Leverage { coin, leverage, .. } => {
                    (format!("Set {coin} leverage?"), format!("Set {leverage}× — Sign & send"), theme::accent())
                }
            };
            ui.horizontal(|ui| {
                testnet_badge(ui);
                ui.label(RichText::new(title).font(ui_font(16.0)).strong());
            });
            ui.add_space(dp(4.0));
            egui::Grid::new("hl-confirm-grid").num_columns(2).spacing([dp(14.0), dp(5.0)]).show(ui, |ui| {
                grid_row(
                    ui,
                    "Network",
                    RichText::new("Hyperliquid TESTNET (test funds)").font(ui_font(12.5)).strong().color(amber()),
                );
                grid_row(ui, "Account", plain(format!("{master} · agent {agent}")));
                match &c {
                    HlConfirm::Order { plan, modify, .. } => {
                        let a = plan.asset();
                        let kind = if a.is_hip3() { "perpetual, not a share" } else { "perpetual" };
                        grid_row(
                            ui,
                            "Market",
                            RichText::new(format!("{} · {} ({kind})", a.coin, a.label()))
                                .font(mono(12.5))
                                .color(amber()),
                        );
                        if let Some(oid) = modify {
                            grid_row(ui, "Replaces", plain(format!("order #{oid}")));
                        }
                        let side = if plan.intent().is_buy { "BUY" } else { "SELL" };
                        let color = if plan.intent().is_buy { theme::up() } else { theme::down() };
                        grid_row(
                            ui,
                            "Order",
                            RichText::new(format!("{side} {} {}", qty(plan.size()), a.coin))
                                .font(mono(13.0))
                                .strong()
                                .color(color),
                        );
                        grid_row(ui, "Type", plain(plan.kind_label()));
                        let px = match plan.intent().kind {
                            OrderKind::Market => format!(
                                "IOC limit {} (at most {:.0}% from the mid)",
                                price(plan.price()),
                                MAX_MARKET_SLIPPAGE * 100.0
                            ),
                            OrderKind::Trigger { .. } => format!(
                                "trigger {} · fills down to {}",
                                plan.trigger().map_or("—".into(), price),
                                price(plan.price())
                            ),
                            OrderKind::Limit { .. } => price(plan.price()),
                        };
                        grid_row(ui, "Price", RichText::new(px).font(mono(12.5)).color(text()));
                        grid_row(
                            ui,
                            "Value",
                            RichText::new(money(plan.notional(), "USD")).font(mono(14.0)).strong().color(text()),
                        );
                        let lev = self.hl.ctx(&a.coin).leverage.map_or("unknown".into(), |l| {
                            format!("{}× {}", l.value, if l.cross { "cross" } else { "isolated" })
                        });
                        grid_row(ui, "Leverage", plain(lev));
                        grid_row(
                            ui,
                            "Reduce only",
                            plain(if plan.intent().reduce_only { "Yes".into() } else { "No".into() }),
                        );
                        grid_row(ui, "Fees", plain("Hyperliquid's maker/taker fee · no builder fee".into()));
                        grid_row(ui, "Expires", plain(format!("{} s after you confirm", EXPIRES_AFTER_MS / 1000)));
                        grid_row(ui, "Client id", RichText::new(plan.cloid()).font(mono(11.0)).color(muted()));
                    }
                    HlConfirm::Cancel { line, oid, .. } => {
                        grid_row(ui, "Order", RichText::new(line).font(mono(12.5)).color(text()));
                        grid_row(ui, "Order id", plain(format!("#{oid}")));
                    }
                    HlConfirm::Kill { orders } => {
                        grid_row(
                            ui,
                            "Cancels",
                            plain(format!("every open order ({} seen; read again before cancelling)", orders.len())),
                        );
                        grid_row(ui, "Then", plain("Hyperliquid orders turn off until you enable them again".into()));
                    }
                    HlConfirm::Leverage { coin, leverage, cross, current } => {
                        grid_row(ui, "Market", RichText::new(coin).font(mono(12.5)).color(amber()));
                        let now = current.map_or("unknown".into(), |l| {
                            format!("{}× {}", l.value, if l.cross { "cross" } else { "isolated" })
                        });
                        grid_row(ui, "Now", plain(now));
                        grid_row(
                            ui,
                            "New",
                            plain(format!("{leverage}× {}", if *cross { "cross" } else { "isolated" })),
                        );
                    }
                }
            });
            ui.add_space(dp(6.0));
            if let HlConfirm::Order { plan, .. } = &c {
                if plan.asset().is_hip3() {
                    ui.label(RichText::new(HIP3_RISK).font(ui_font(11.5)).color(amber()));
                }
                ui.label(
                    RichText::new("Orders on Hyperliquid are on-chain and can't be undone once they fill.")
                        .font(ui_font(11.5))
                        .color(muted()),
                );
            }
            if let HlConfirm::Kill { orders } = &c
                && orders.is_empty()
            {
                ui.label(RichText::new("No open orders seen right now.").font(ui_font(11.5)).color(muted()));
            }
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
                let b = egui::Button::new(RichText::new(go).strong().color(Color32::WHITE)).fill(go_color);
                if ui.add(b).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        match choice {
            Some(true) => self.hl_confirmed(),
            Some(false) => self.hl.confirm = None,
            None => {}
        }
    }

    pub(super) fn hl_enable_dialog(&mut self, ctx: &egui::Context) {
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("hl-enable")).show(ctx, |ui| {
            ui.set_width(dp(450.0));
            ui.horizontal(|ui| {
                testnet_badge(ui);
                ui.label(RichText::new("Enable Hyperliquid orders?").font(ui_font(16.0)).strong());
            });
            ui.add_space(dp(4.0));
            for line in [
                "This build trades on Hyperliquid testnet only, with test funds. It cannot reach mainnet.",
                "Orders are on-chain transactions: once they fill they can't be undone.",
                "Perps are leveraged and can be liquidated. HIP-3 stock markets (xyz) are perpetual futures with funding, not shares.",
                "Hyperliquid's Terms of Use restrict some regions, including Ontario, Canada and the United States. You are responsible for whether you may use it.",
                "Fixed limits: $100 per order, 3× leverage, 1% market slippage. Every order, cancel and leverage change asks for confirmation.",
            ] {
                ui.label(RichText::new(format!("• {line}")).font(ui_font(12.0)).color(text()));
            }
            ui.add_space(dp(6.0));
            ui.checkbox(&mut self.hl.ack, "I am not a Restricted Person under Hyperliquid's Terms, and I understand the above");
            ui.add_space(dp(6.0));
            ui.horizontal(|ui| {
                if ui.button("Keep Off").clicked() {
                    choice = Some(false);
                }
                let on = self.hl.ack;
                let b = if on {
                    egui::Button::new(RichText::new("Enable Hyperliquid orders").strong().color(Color32::BLACK)).fill(amber())
                } else {
                    egui::Button::new(RichText::new("Enable Hyperliquid orders").strong())
                };
                if ui.add_enabled(on, b).clicked() {
                    choice = Some(true);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(false);
        }
        if let Some(on) = choice {
            self.hl.enable_prompt = false;
            self.hl.ack = false;
            if on {
                self.prefs.trading.hl.enabled = true;
                if let Err(e) = self.prefs.save(&super::prefs_path()) {
                    self.status = format!("Could not save preferences: {e}");
                }
                self.hl.message = Some(("Hyperliquid testnet orders enabled.".into(), false));
            }
        }
    }

    /// Settings › Trading › Hyperliquid.
    pub(super) fn hl_settings(&mut self, ui: &mut Ui, cmds: &mut Vec<SettingsCmd>) {
        let note = |ui: &mut Ui, s: &str| {
            ui.label(RichText::new(s).font(ui_font(11.5)).color(muted()));
        };
        ui.add_space(dp(10.0));
        ui.horizontal(|ui| {
            ui.label(RichText::new("Hyperliquid").font(ui_font(13.5)).color(text()));
            testnet_badge(ui);
        });
        ui.separator();
        let h = self.prefs.trading.hl.clone();
        let on = h.enabled && self.hl_signing_blocker().is_none();
        ui.label(
            RichText::new(match (h.master.is_some(), h.agent.is_some(), on) {
                (false, _, _) => {
                    "Not connected. Perps on the core dex and HIP-3 xyz (BTC, xyz:AAPL), testnet only.".to_string()
                }
                (true, false, _) => {
                    "Read-only: positions and orders show in PORT, POS and ORD. Approve an agent to trade.".into()
                }
                (true, true, true) => {
                    "Orders are ON (testnet): confirmed orders are signed here and sent to Hyperliquid testnet.".into()
                }
                (true, true, false) => "Orders are OFF.".into(),
            })
            .font(ui_font(12.5))
            .color(if on { amber() } else { theme::accent() }),
        );

        // Account.
        match &h.master {
            Some(m) => {
                let value = self.hl.user.as_ref().map(|u| money(u.account_value(), "USD"));
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("Account {}", masked(m))).font(mono(12.5)).color(text()));
                    let (dot, status) = if self.hl.connected {
                        (theme::up(), self.hl.stream_status.clone())
                    } else {
                        (
                            amber(),
                            if self.hl.stream_status.is_empty() {
                                "Connecting…".into()
                            } else {
                                self.hl.stream_status.clone()
                            },
                        )
                    };
                    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, dot);
                    note(ui, &status);
                    if let Some(v) = value {
                        note(ui, &format!("· value {v}"));
                    }
                    // Not while an approval is in flight: its answer belongs to this account.
                    if ui
                        .add_enabled(self.hl.busy.is_none(), egui::Button::new("Remove").small())
                        .on_hover_text("Stop showing this account (the agent key stays unless revoked)")
                        .clicked()
                    {
                        self.hl_disconnect();
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Account").font(ui_font(12.5)));
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
                note(
                    ui,
                    "A read-only address fills PORT, POS, ORD and ACTV from Hyperliquid testnet. Nothing is signed.",
                );
            }
        }

        // Agent.
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
                            "Agent {} · {}",
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
                        RichText::new(format!("The agent expires in {days} days: revoke it and approve a new one."))
                            .font(ui_font(12.0))
                            .color(amber()),
                    );
                }
                if self.hl.key.is_none() && !self.hl.key_loading {
                    ui.label(
                        RichText::new("This device has no key for this agent.").font(ui_font(12.0)).color(amber()),
                    );
                }
            }
            None => {
                note(
                    ui,
                    "Approve an agent (API wallet): Terminal makes a new key on this device; your main wallet signs approveAgent once, named terminal-<device>, valid 30 days. The agent can trade but not withdraw.",
                );
            }
        }
        if let Some(e) = &self.hl.store_error {
            ui.label(
                RichText::new(format!("Trading is off on this device: {e}")).font(ui_font(12.0)).color(theme::down()),
            );
        }
        if h.master.is_some() || h.agent.is_none() {
            self.hl_key_controls(ui, &h);
        }

        // Orders on or off.
        ui.add_space(dp(6.0));
        ui.horizontal(|ui| {
            if h.enabled {
                if ui.button("Turn Hyperliquid Orders Off").clicked() {
                    self.prefs.trading.hl.enabled = false;
                    self.hl.confirm = None;
                    cmds.push(SettingsCmd::SavePrefs);
                }
            } else if ui
                .add_enabled(h.agent.is_some(), egui::Button::new(RichText::new("Enable Hyperliquid Orders…").strong()))
                .on_disabled_hover_text("Approve an agent first")
                .clicked()
            {
                self.hl.enable_prompt = true;
                self.hl.ack = false;
            }
            if h.master.is_some()
                && ui.button("KILL").on_hover_text("Cancel every open Hyperliquid order and turn orders off").clicked()
            {
                self.hl_kill();
            }
        });
        note(
            ui,
            &format!(
                "Fixed in this build: ${MAX_ORDER_NOTIONAL_USD:.0} per order, {MAX_LEVERAGE}× leverage, {:.0}% market slippage, no builder fee. KILL cancels every open order and turns orders off.",
                MAX_MARKET_SLIPPAGE * 100.0
            ),
        );
        let mut dms = self.prefs.trading.hl.dead_mans_switch;
        if ui
            .checkbox(&mut dms, "Dead man's switch: cancel all open orders if Terminal stops for a minute")
            .on_hover_text(
                "Keeps a scheduleCancel about 60 s ahead while Terminal runs. It also cancels orders placed elsewhere.",
            )
            .changed()
        {
            self.prefs.trading.hl.dead_mans_switch = dms;
            cmds.push(SettingsCmd::SavePrefs);
        }
        if cfg!(target_arch = "wasm32") {
            let mut keep = self.prefs.trading.hl.remember_on_browser;
            if ui
                .checkbox(
                    &mut keep,
                    "Remember the agent key on this browser (encrypted with a key the page can't read)",
                )
                .on_hover_text("Off: the key lasts for this tab only, and you approve again next visit.")
                .changed()
            {
                self.prefs.trading.hl.remember_on_browser = keep;
                cmds.push(SettingsCmd::SavePrefs);
                #[cfg(target_arch = "wasm32")]
                if keep {
                    self.hl_web_remember_key();
                } else {
                    self.hl_web_forget_key();
                }
            }
        }

        // Roles.
        if h.master.is_some() {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(!self.hl.roles_busy, egui::Button::new("Check on Hyperliquid"))
                    .on_hover_text("userRole and extraAgents")
                    .clicked()
                {
                    self.hl_check_roles();
                }
                match &self.hl.roles {
                    Some(Ok(r)) => {
                        let agents: Vec<String> =
                            r.agents.iter().map(|a| format!("{} ({})", a.name, masked(&a.address))).collect();
                        note(
                            ui,
                            &format!(
                                "account role: {}{} · approved agents: {}",
                                r.master,
                                r.agent.as_ref().map_or(String::new(), |a| format!(" · agent role: {a}")),
                                if agents.is_empty() { "none".into() } else { agents.join(", ") }
                            ),
                        );
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).font(ui_font(11.5)).color(theme::down()));
                    }
                    None => {}
                }
            });
        }
        if let Some(b) = self.hl.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                note(ui, b);
            });
        }
        if let Some((m, err)) = &self.hl.message {
            ui.label(RichText::new(m).font(ui_font(12.0)).color(if *err { theme::down() } else { theme::up() }));
        }
    }

    /// Approve or revoke with a pasted testnet key (desktop), or the browser wallet (web).
    fn hl_key_controls(&mut self, ui: &mut Ui, h: &crate::prefs::HlPrefs) {
        let busy = self.hl.busy.is_some();
        #[cfg(target_arch = "wasm32")]
        {
            self.hl_web_wallet_controls(ui, h, busy);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            ui.label(
                RichText::new(
                    "Testnet only: paste a throwaway TESTNET main wallet key. Terminal uses it once to sign approveAgent (or the revoke), wipes it from memory and never stores it. Never paste a key that holds real funds.",
                )
                .font(ui_font(11.5))
                .color(amber()),
            );
            ui.horizontal(|ui| {
                let field = egui::TextEdit::singleline(&mut *self.hl.main_key_form)
                    .id(egui::Id::new(MAIN_KEY_FIELD))
                    .password(true)
                    .hint_text("testnet main key (0x…)")
                    .font(mono(12.0))
                    .desired_width(dp(300.0));
                ui.add_enabled(!busy, field);
                let pasted = !self.hl.main_key_form.trim().is_empty();
                let used = if h.agent.is_none() {
                    let go = ui
                        .add_enabled(pasted && !busy, egui::Button::new(RichText::new("Approve Agent").strong()))
                        .clicked();
                    if go {
                        self.hl_approve_with_pasted_key();
                    }
                    go
                } else {
                    let go = ui
                        .add_enabled(pasted && !busy, egui::Button::new("Revoke on Hyperliquid"))
                        .on_hover_text(
                            "Deletes the key here and replaces the agent on Hyperliquid with an address nobody holds",
                        )
                        .clicked();
                    if go {
                        self.hl_revoke(true);
                    }
                    go
                };
                // The field's undo history holds copies of what was typed: drop it with the key.
                if used {
                    egui::text_edit::TextEditState::default().store(ui.ctx(), egui::Id::new(MAIN_KEY_FIELD));
                }
            });
        }
        if h.agent.is_some()
            && ui
                .add_enabled(!busy, egui::Button::new("Delete Agent Key Here"))
                .on_hover_text("Terminal can't sign with it any more; it stays approved on Hyperliquid until it expires or is revoked")
                .clicked()
        {
            self.hl_revoke(false);
        }
    }

    /// Accounts the ticket can choose from (as `trade_accounts`, for the HL body).
    fn trade_accounts_pub(&self) -> Vec<tr_broker::Account> {
        self.broker.portfolio.as_ref().map(|p| p.accounts.clone()).unwrap_or_default()
    }
}
