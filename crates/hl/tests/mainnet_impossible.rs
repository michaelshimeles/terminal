//! This build cannot reach or sign for Hyperliquid mainnet. These tests fail if that changes.

use std::path::Path;

use tr_hl::Network;
use tr_hl::network::{API_URL, WS_URL};

/// Mainnet hosts that must never appear in this crate (the testnet hosts are
/// `api.hyperliquid-testnet.xyz`, which none of these match).
const MAINNET_HOSTS: [&str; 3] = ["api.hyperliquid.xyz", "app.hyperliquid.xyz", "wss://api.hyperliquid.xyz"];

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs" || x == "toml") {
            out.push(p);
        }
    }
}

#[test]
fn the_crate_names_no_mainnet_host() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("Cargo.toml")];
    for d in ["src", "examples", "tests"] {
        if root.join(d).exists() {
            sources(&root.join(d), &mut files);
        }
    }
    assert!(files.len() > 10, "found the sources: {files:?}");
    let me = file!().rsplit('/').next().unwrap();
    for f in files {
        // This file lists the hosts it looks for.
        if f.file_name().is_some_and(|n| n == me) {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for host in MAINNET_HOSTS {
            assert!(!text.contains(host), "{} names the mainnet host {host}", f.display());
        }
    }
}

#[test]
fn the_endpoints_are_the_testnet_hosts() {
    assert_eq!(API_URL, "https://api.hyperliquid-testnet.xyz");
    assert_eq!(WS_URL, "wss://api.hyperliquid-testnet.xyz/ws");
    let n = Network::default();
    assert!(n.info_url().starts_with("https://api.hyperliquid-testnet.xyz/"));
    assert!(n.exchange_url().starts_with("https://api.hyperliquid-testnet.xyz/"));
}

#[test]
fn there_is_one_network_and_it_signs_as_testnet() {
    // Adding a variant makes this match non-exhaustive, so the test stops compiling.
    match Network::default() {
        Network::Testnet => {}
    }
    assert_eq!(Network::Testnet.phantom_source(), "b");
    assert_eq!(Network::Testnet.hyperliquid_chain(), "Testnet");
    assert_eq!(Network::Testnet.label(), "TESTNET");
}

#[test]
fn stored_preferences_cannot_select_mainnet() {
    #[derive(serde::Deserialize)]
    struct Prefs {
        network: Network,
    }
    for v in ["\"mainnet\"", "\"Mainnet\"", "\"MAINNET\"", "\"testnet\"", "\"arbitrum\"", "0", "true", "null"] {
        let p: Prefs = serde_json::from_str(&format!(r#"{{"network": {v}}}"#)).unwrap();
        assert_eq!(p.network, Network::Testnet, "{v}");
    }
}

#[test]
fn signatures_are_testnet_signatures() {
    use tr_hl::actions::ApproveAgentAction;
    use tr_hl::signing::{recover_approve_agent, sign_approve_agent};
    use tr_hl::trader::approve_agent;
    let main = tr_hl::MainKey::generate();
    let agent = tr_hl::AgentKey::generate();
    let a = approve_agent(&tr_hl::SigningKey::address(&agent), "ci", 30, 1_793_667_000_000);
    assert_eq!(a.hyperliquid_chain, "Testnet");
    let sig = sign_approve_agent(&main, &a).unwrap();
    assert_eq!(recover_approve_agent(&a, &sig).unwrap(), tr_hl::SigningKey::address(&main));
    // Re-addressed to mainnet, the same action can't be signed, and the testnet signature
    // doesn't recover the signer for it.
    let mainnet = ApproveAgentAction { hyperliquid_chain: "Mainnet".into(), ..a };
    assert!(sign_approve_agent(&main, &mainnet).is_err());
    assert!(recover_approve_agent(&mainnet, &sig).is_err());
}
