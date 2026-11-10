//! "Get testnet USDC": asks Hyperliquid testnet's faucet for a drip for the built-in wallet,
//! and says what came of it. Testnet only: the request is `tr_hl::client::Client::claim_drip`,
//! which has no way to reach mainnet. Nothing is signed.
//!
//! The button is off while a request is out and for [`COOLDOWN`] after each answer.

use std::time::Duration;

use tr_hl::faucet::DripOutcome;
use web_time::Instant;

use crate::wallet_worker::WalletRequest;

/// How long the button stays off after the faucet answers.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// How a result reads: good news, a refusal, or a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Refused,
    Failed,
}

/// The faucet request for the wallet: in flight, or the last answer.
#[derive(Default)]
pub struct DripState {
    /// The address a request is out for.
    asking: Option<String>,
    /// The last answer: for which address, what, and when.
    last: Option<(String, DripOutcome, Instant)>,
}

impl DripState {
    /// A request is out.
    pub fn in_flight(&self) -> bool {
        self.asking.is_some()
    }

    /// Time left before the button works again, if it is cooling down.
    pub fn cooldown_left(&self, now: Instant) -> Option<Duration> {
        let (_, _, at) = self.last.as_ref()?;
        let left = COOLDOWN.saturating_sub(now.saturating_duration_since(*at));
        (!left.is_zero()).then_some(left)
    }

    /// Start a request for `user`. `false`: one is out, or the cooldown hasn't ended.
    pub fn start(&mut self, user: &str, now: Instant) -> bool {
        if self.in_flight() || self.cooldown_left(now).is_some() {
            return false;
        }
        self.asking = Some(user.to_string());
        true
    }

    /// The faucet's answer for `user`. `None` when nothing was asked for that address (a late
    /// answer for a wallet that was removed); otherwise whether to re-read the balances.
    pub fn finish(&mut self, user: &str, outcome: DripOutcome, now: Instant) -> Option<bool> {
        if !self.asking.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(user)) {
            return None;
        }
        self.asking = None;
        let refresh = matches!(outcome, DripOutcome::Claimed { .. });
        self.last = Some((user.to_string(), outcome, now));
        Some(refresh)
    }

    /// The last answer, if it was for `user`.
    pub fn last_for(&self, user: &str) -> Option<&DripOutcome> {
        self.last.as_ref().filter(|(u, _, _)| u.eq_ignore_ascii_case(user)).map(|(_, o, _)| o)
    }
}

/// What to tell the user about an answer.
pub fn drip_message(outcome: &DripOutcome) -> (String, Tone) {
    match outcome {
        DripOutcome::Claimed { usdc } => (
            format!(
                "The faucet sent {} test USDC to this wallet. Deposit… shows the new balances once they refresh.",
                thousands(*usdc)
            ),
            Tone::Good,
        ),
        DripOutcome::NoMainnetDeposit => (
            "The faucet said no. It only pays addresses that have deposited on Hyperliquid mainnet, and this wallet never has. This build can't reach mainnet, so open Deposit… and send test USDC to this address from another testnet account."
                .into(),
            Tone::Refused,
        ),
        DripOutcome::AlreadyClaimed => (
            "The faucet says this wallet has already claimed its test USDC, and it pays each address once. For more, open Deposit… and send some from another testnet account."
                .into(),
            Tone::Refused,
        ),
        DripOutcome::RateLimited => (
            "The faucet is rate limiting requests. Wait a minute, then try again.".into(),
            Tone::Refused,
        ),
        DripOutcome::Refused(why) => (format!("The faucet refused. It said \"{why}\""), Tone::Refused),
        DripOutcome::Network(why) => (
            format!("Couldn't reach Hyperliquid's testnet faucet. Error: {why}. Check your connection, then try again."),
            Tone::Failed,
        ),
    }
}

/// `1000.0` as `1,000`.
pub(super) fn thousands(n: f64) -> String {
    let whole = format!("{:.0}", n.max(0.0));
    let mut out = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl super::TerminalApp {
    /// The button: ask the faucet for the wallet shown, unless a request is out or cooling down.
    pub(super) fn wallet_claim_drip(&mut self) {
        let Some(w) = self.wallet().cloned() else { return };
        if self.hl.wallet.drip.start(&w.address, Instant::now()) {
            self.wallet_send(WalletRequest::ClaimDrip { user: w.address });
        }
    }

    /// The worker's answer.
    pub(super) fn on_drip(&mut self, user: String, outcome: DripOutcome) {
        let refresh = self.hl.wallet.drip.finish(&user, outcome, Instant::now());
        // The user stream brings the perp balance on its own. Read spot USDC now.
        if refresh == Some(true) && self.wallet().is_some_and(|w| w.address == user) {
            self.wallet_refresh_balances();
        }
    }
}

#[cfg(test)]
mod tests {
    use tr_hl::faucet::DRIP_USDC;

    use super::*;

    const A: &str = "0x00000000000000000000000000000000000000aa";
    const B: &str = "0x00000000000000000000000000000000000000bb";

    fn all() -> Vec<DripOutcome> {
        vec![
            DripOutcome::Claimed { usdc: DRIP_USDC },
            DripOutcome::NoMainnetDeposit,
            DripOutcome::AlreadyClaimed,
            DripOutcome::RateLimited,
            DripOutcome::Refused("Drip is paused.".into()),
            DripOutcome::Network("timed out".into()),
        ]
    }

    #[test]
    fn a_drip_says_how_much() {
        let (m, tone) = drip_message(&DripOutcome::Claimed { usdc: DRIP_USDC });
        assert_eq!(tone, Tone::Good);
        assert!(m.contains("1,000 test USDC"), "{m}");
        assert!(m.contains("refresh"), "{m}");
    }

    /// The faucet's known refusal says why, and sends the user to Deposit.
    #[test]
    fn no_mainnet_deposit_explains_and_points_to_deposit() {
        let (m, tone) = drip_message(&DripOutcome::NoMainnetDeposit);
        assert_eq!(tone, Tone::Refused);
        assert!(m.contains("deposited on Hyperliquid mainnet"), "{m}");
        assert!(m.contains("Deposit"), "{m}");
        assert!(m.contains("can't reach mainnet"), "{m}");
    }

    #[test]
    fn every_outcome_has_its_own_message() {
        let msgs: Vec<(String, Tone)> = all().iter().map(drip_message).collect();
        let distinct: std::collections::HashSet<&String> = msgs.iter().map(|(m, _)| m).collect();
        assert_eq!(distinct.len(), msgs.len(), "{msgs:#?}");
        let (rate, _) = drip_message(&DripOutcome::RateLimited);
        assert!(rate.contains("rate limit") && rate.contains("try again"), "{rate}");
        let (already, _) = drip_message(&DripOutcome::AlreadyClaimed);
        assert!(already.contains("already") && already.contains("Deposit"), "{already}");
        let (net, tone) = drip_message(&DripOutcome::Network("timed out".into()));
        assert_eq!(tone, Tone::Failed);
        assert!(net.contains("Couldn't reach") && net.contains("timed out"), "{net}");
        let (other, tone) = drip_message(&DripOutcome::Refused("Drip is paused.".into()));
        assert_eq!(tone, Tone::Refused);
        assert!(other.contains("Drip is paused."), "{other}");
    }

    /// Off while asking, off for the cooldown after any answer, then on again.
    #[test]
    fn the_button_is_off_while_asking_and_for_the_cooldown() {
        for outcome in all() {
            let mut s = DripState::default();
            let t0 = Instant::now();
            assert!(!s.in_flight());
            assert_eq!(s.cooldown_left(t0), None);
            assert!(s.start(A, t0));
            assert!(s.in_flight());
            assert!(!s.start(A, t0), "a second request while one is out");
            let refresh = s.finish(A, outcome.clone(), t0);
            assert_eq!(refresh, Some(matches!(outcome, DripOutcome::Claimed { .. })), "{outcome:?}");
            assert!(!s.in_flight());
            assert_eq!(s.last_for(A), Some(&outcome));
            assert_eq!(s.cooldown_left(t0), Some(COOLDOWN));
            let later = t0 + COOLDOWN / 2;
            assert_eq!(s.cooldown_left(later), Some(COOLDOWN / 2));
            assert!(!s.start(A, later), "{outcome:?}: started during the cooldown");
            let after = t0 + COOLDOWN;
            assert_eq!(s.cooldown_left(after), None);
            assert!(s.start(A, after), "{outcome:?}");
        }
    }

    /// An answer for another address, or when nothing was asked, changes nothing.
    #[test]
    fn a_stray_answer_is_ignored() {
        let t0 = Instant::now();
        let mut s = DripState::default();
        assert_eq!(s.finish(A, DripOutcome::Claimed { usdc: DRIP_USDC }, t0), None);
        assert_eq!(s.cooldown_left(t0), None);
        assert!(s.start(A, t0));
        assert_eq!(s.finish(B, DripOutcome::Claimed { usdc: DRIP_USDC }, t0), None);
        assert!(s.in_flight(), "still waiting for A's answer");
        assert_eq!(s.finish(A, DripOutcome::NoMainnetDeposit, t0), Some(false));
        assert_eq!(s.last_for(B), None, "A's answer isn't shown for B");
    }
}
