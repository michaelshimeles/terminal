//! A live check of this crate's signatures against Hyperliquid **testnet**, with throwaway
//! keys. Never run in CI. It only uses the crate's public API, so every order goes through
//! the same caps as the app.
//!
//! ```text
//! # keys: a 0600 JSON file outside the repo, {"main": "0x…", "agent": "0x…"}
//! cargo run -p tr-hl --example testnet_check -- --generate /tmp/hl-throwaway/keys.json
//! cargo run -p tr-hl --example testnet_check -- /tmp/hl-throwaway/keys.json
//! ```
//!
//! It never prints a key or a signature, and masks every address (`0x1234…abcd`). Without
//! test USDC the account doesn't exist on Hyperliquid, so the expected answers are business
//! rejections. Hyperliquid reports a signature it can't match as "User or API Wallet 0x… does
//! not exist", where 0x… is the address it recovered: a match with the signer means the
//! signature is right. A deliberately corrupted signature is sent as the control.

use std::path::Path;

use serde_json::{Value, json};
use tr_hl::SigningKey;
use tr_hl::actions::{Tif, cloid};
use tr_hl::client::{Client, ExchangeResponse, SendError, recovered_address};
use tr_hl::key::{AgentKey, MainKey, mask_address};
use tr_hl::nonce::{NonceManager, now_ms};
use tr_hl::trader::*;

/// Replace every `0x` + 40 hex digits with its masked form.
fn mask(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"0x") && b.len() >= i + 42 && b[i + 2..i + 42].iter().all(u8::is_ascii_hexdigit) {
            let after = b.get(i + 42).is_none_or(|c| !c.is_ascii_hexdigit());
            if after && let Ok(a) = tr_hl::key::parse_address(&text[i..i + 42]) {
                out.push_str(&mask_address(&a));
                i += 42;
                continue;
            }
        }
        // Copy one whole character (the text has `…` and `×`).
        let ch = text[i..].chars().next().expect("on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn generate(path: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let (main, agent) = (AgentKey::generate(), AgentKey::generate());
    let text = zeroize::Zeroizing::new(json!({"main": *main.to_hex(), "agent": *agent.to_hex()}).to_string());
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(text.as_bytes())?;
    println!(
        "wrote throwaway keys (0600) for main {} and agent {}",
        mask_address(&main.address()),
        mask_address(&agent.address())
    );
    Ok(())
}

struct Check {
    client: Client,
    out: Vec<String>,
}

impl Check {
    fn say(&mut self, s: impl AsRef<str>) {
        let line = mask(s.as_ref());
        println!("{line}");
        self.out.push(line);
    }

    async fn info(&mut self, label: &str, body: Value) -> Value {
        match self.client.info(body).await {
            Ok(v) => {
                self.say(format!("info {label}: {v}"));
                v
            }
            Err(e) => {
                self.say(format!("info {label}: ERROR {e:#}"));
                Value::Null
            }
        }
    }

    /// Send, and say whom Hyperliquid recovered if it names an address.
    fn report(&mut self, label: &str, signer: &alloy_primitives::Address, r: &Result<ExchangeResponse, SendError>) {
        match r {
            Ok(resp) => {
                self.say(format!("{label}: {}", resp.summary()));
                if let Some(rec) = resp.first_error().and_then(recovered_address) {
                    let verdict = if &rec == signer { "MATCHES the signer" } else { "does NOT match the signer" };
                    self.say(format!("  recovered {} {verdict} {}", mask_address(&rec), mask_address(signer)));
                }
            }
            Err(e) => self.say(format!("{label}: send error: {e}")),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--generate") {
        return generate(Path::new(args.get(1).expect("path to the keys file")));
    }
    let path = args.first().expect("path to the keys file");
    let keys: Value = serde_json::from_str(&zeroize::Zeroizing::new(std::fs::read_to_string(path)?))?;
    let main = MainKey::from_hex(keys["main"].as_str().unwrap())?;
    let agent = AgentKey::from_hex(keys["agent"].as_str().unwrap())?;
    drop(keys);
    let (m, a) = (main.address(), agent.address());
    let mut c = Check { client: Client::new()?, out: Vec::new() };
    c.say(format!("testnet check at {} UTC ms {}; main {m:#x}, agent {a:#x}", chrono_like(now_ms()), now_ms()));

    // 1. Before approving.
    c.info("userRole(main) before", json!({"type": "userRole", "user": format!("{m:#x}")})).await;
    c.info("userRole(agent) before", json!({"type": "userRole", "user": format!("{a:#x}")})).await;
    c.info("extraAgents(main) before", json!({"type": "extraAgents", "user": format!("{m:#x}")})).await;

    // 2. approveAgent, signed by the main key.
    let approve = approve_agent(&a, "check", DEFAULT_AGENT_DAYS, now_ms());
    c.say(format!(
        "approveAgent name {:?}, chain {}, signatureChainId {}",
        approve.agent_name, approve.hyperliquid_chain, approve.signature_chain_id
    ));
    let sig = tr_hl::signing::sign_approve_agent(&main, &approve)?;
    let r = c.client.send_user_signed(&user_signed_body(&approve, &sig)).await;
    c.report("approveAgent (main key)", &m, &r);

    // 3. After.
    c.info("userRole(main) after", json!({"type": "userRole", "user": format!("{m:#x}")})).await;
    c.info("userRole(agent) after", json!({"type": "userRole", "user": format!("{a:#x}")})).await;
    c.info("extraAgents(main) after", json!({"type": "extraAgents", "user": format!("{m:#x}")})).await;

    // 4. Markets, leverage and testnet mids.
    let reg = c.client.registry().await?;
    c.say(format!(
        "registry: {} markets; xyz is perpDexs index {:?}; BTC id {}; xyz:AAPL id {}",
        reg.len(),
        reg.dex_index("xyz"),
        reg.resolve("BTC").map(|x| x.id).unwrap_or(0),
        reg.resolve("xyz:AAPL").map(|x| x.id).unwrap_or(0)
    ));
    let (agent_nonces, main_nonces) = (NonceManager::new(), NonceManager::new());
    for coin in ["BTC", "xyz:AAPL"] {
        let asset = reg.resolve(coin).map_err(anyhow::Error::msg)?.clone();
        let dex = asset.dex.clone();
        let mid = c.client.mids(&dex).await?.get(coin).copied();
        let data = c.client.asset_data(&m, coin).await?;
        let lev = data.as_ref().and_then(|d| d.leverage);
        c.say(format!("{coin}: testnet mid {mid:?}, account leverage {lev:?}"));

        // 5. Leverage to 3× (agent), as the ticket does before a first order.
        let s = sign_update_leverage(&agent, &agent_nonces, &asset, !asset.only_isolated, 3, now_ms())?;
        let r = c.client.send(&s).await;
        c.report(&format!("updateLeverage {coin} 3× (agent)"), &a, &r);
        let lev = c.client.asset_data(&m, coin).await?.and_then(|d| d.leverage);
        c.say(format!("{coin}: account leverage now {lev:?}"));

        // 6. A small limit far below the market, by the agent and by the main key.
        let mid = mid.expect("testnet mid");
        let px = mid * 0.6;
        let size = 12.0 / px;
        let intent = OrderIntent {
            coin: coin.into(),
            is_buy: true,
            size,
            kind: OrderKind::Limit { px, tif: Tif::Gtc },
            reduce_only: false,
        };
        let ctx = MarketCtx { mid: Some(mid), leverage: lev };
        for (who, signer) in [("agent", a), ("main key", m)] {
            let id = cloid(now_ms() as u128);
            match plan_order(&reg, &intent, &ctx, &id) {
                Ok(plan) => {
                    c.say(format!(
                        "{coin} order plan: BUY {} @ {} (notional ${:.2}), cloid {}",
                        plan.size(),
                        plan.price(),
                        plan.notional(),
                        plan.cloid()
                    ));
                    let s = if who == "agent" {
                        sign_order(&agent, &agent_nonces, &plan, &ctx, now_ms())?
                    } else {
                        sign_order(&main, &main_nonces, &plan, &ctx, now_ms())?
                    };
                    let r = c.client.send(&s).await;
                    c.report(&format!("order {coin} limit buy (signed by {who})"), &signer, &r);
                }
                Err(refusal) => {
                    c.say(format!("{coin} order (signed by {who}): REFUSED before signing: {refusal}"));
                    // A reduce-only sell far above the market passes the caps whatever the
                    // leverage, and still exercises the order signature.
                    let ro = OrderIntent {
                        is_buy: false,
                        reduce_only: true,
                        kind: OrderKind::Limit { px: mid * 1.4, tif: Tif::Gtc },
                        size: 12.0 / (mid * 1.4),
                        ..intent.clone()
                    };
                    let plan = plan_order(&reg, &ro, &ctx, &id).map_err(|e| anyhow::anyhow!("{e}"))?;
                    c.say(format!(
                        "{coin} reduce-only plan: SELL {} @ {} (notional ${:.2})",
                        plan.size(),
                        plan.price(),
                        plan.notional()
                    ));
                    let s = if who == "agent" {
                        sign_order(&agent, &agent_nonces, &plan, &ctx, now_ms())?
                    } else {
                        sign_order(&main, &main_nonces, &plan, &ctx, now_ms())?
                    };
                    let r = c.client.send(&s).await;
                    c.report(&format!("order {coin} reduce-only limit sell (signed by {who})"), &signer, &r);
                }
            }
        }
    }

    // 7. The control: a damaged agent signature recovers someone else.
    let noop = sign_noop(&agent, &agent_nonces, now_ms())?;
    let r = c.client.send(&noop.corrupted()).await;
    c.report("noop with a CORRUPTED agent signature (control)", &a, &r);

    // 8. Cancel an order that doesn't exist, and a noop.
    let btc = reg.resolve("BTC").map_err(anyhow::Error::msg)?.clone();
    let s = sign_cancel(&agent, &agent_nonces, &btc, 1, now_ms())?;
    let r = c.client.send(&s).await;
    c.report("cancel BTC oid 1 (agent)", &a, &r);
    let r = c.client.send(&sign_noop(&agent, &agent_nonces, now_ms())?).await;
    c.report("noop (agent)", &a, &r);
    let r = c.client.send(&sign_noop(&main, &main_nonces, now_ms())?).await;
    c.report("noop (main key)", &m, &r);

    if let Some(out) = args.get(1) {
        std::fs::write(out, c.out.join("\n") + "\n")?;
    }
    Ok(())
}

/// `2026-11-03T01:02:03Z` without a date crate.
fn chrono_like(ms: u64) -> String {
    let s = ms / 1000;
    let (days, rem) = (s / 86_400, s % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}
