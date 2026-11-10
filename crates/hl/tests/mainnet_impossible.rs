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

/// A main wallet held elsewhere (Turnkey) signs a digest this crate hands it. It is only
/// ever asked for a testnet `approveAgent` at the testnet signature chain, and its answer is
/// used only if it recovers the main wallet.
#[test]
fn a_remote_main_wallet_signs_only_testnet_approvals() {
    use std::cell::Cell;
    use tr_hl::actions::ApproveAgentAction;
    use tr_hl::signing::{recover_approve_agent, sign_approve_agent_with};
    use tr_hl::trader::approve_agent;

    // The remote wallet is a plain k256 key here, signing whatever 32 bytes it is given.
    let main = k256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
    let master = alloy_primitives::Address::from_private_key(&main);
    let sign = |key: &k256::ecdsa::SigningKey,
                digest: alloy_primitives::B256|
     -> anyhow::Result<alloy_primitives::Signature> {
        let (sig, recid) = key.sign_prehash_recoverable(digest.as_slice())?;
        Ok(alloy_primitives::Signature::from_signature_and_parity(sig, recid.is_y_odd()))
    };
    let agent = tr_hl::AgentKey::generate();
    let a = approve_agent(&tr_hl::SigningKey::address(&agent), "ci", 180, 1_793_667_000_000);
    assert_eq!((a.hyperliquid_chain.as_str(), a.signature_chain_id.as_str()), ("Testnet", "0x66eee"));

    let asked = Cell::new(0);
    let remote = |digest: alloy_primitives::B256| {
        asked.set(asked.get() + 1);
        let sig = sign(&main, digest);
        async move { sig }
    };
    let sig = futures::executor::block_on(sign_approve_agent_with(&a, &master, remote)).unwrap();
    assert_eq!(recover_approve_agent(&a, &sig).unwrap(), master);
    assert_eq!(asked.get(), 1);

    // Mainnet, or a mainnet signature chain (Arbitrum One, 0xa4b1), never reaches the signer.
    for bad in [
        ApproveAgentAction { hyperliquid_chain: "Mainnet".into(), ..a.clone() },
        ApproveAgentAction { signature_chain_id: "0xa4b1".into(), ..a.clone() },
        ApproveAgentAction { kind: "usdSend".into(), ..a.clone() },
    ] {
        assert!(futures::executor::block_on(sign_approve_agent_with(&bad, &master, remote)).is_err(), "{bad:?}");
    }
    assert_eq!(asked.get(), 1, "the remote signer was asked to sign a non-testnet action");

    // A signature from any other key is refused rather than sent.
    let other = k256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
    let wrong = |digest: alloy_primitives::B256| {
        let sig = sign(&other, digest);
        async move { sig }
    };
    let err = futures::executor::block_on(sign_approve_agent_with(&a, &master, wrong)).unwrap_err();
    assert!(err.to_string().contains("not the wallet's"), "{err}");
}

/// The built-in wallet (Turnkey's API; here its local stand-in) is never sent anything for
/// mainnet: the request is refused before it is made.
#[tokio::test]
async fn the_built_in_wallet_is_never_asked_to_sign_for_mainnet() {
    use tr_hl::actions::ApproveAgentAction;
    use tr_hl::signing::sign_approve_agent_with;
    use tr_hl::trader::approve_agent;

    let mock = tr_turnkey::mock::MockTurnkey::start(0, None).await.unwrap();
    let key = tr_turnkey::SessionKey::generate();
    let token = mock.mint_session(&key.public_hex(), 60);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let session = tr_turnkey::Session::accept(key, &token, now).unwrap();
    let client = tr_turnkey::Client::with_base(&mock.base).unwrap();
    let master: alloy_primitives::Address = mock.address().parse().unwrap();
    let sign = |a: ApproveAgentAction| {
        let (client, session) = (&client, &session);
        async move {
            sign_approve_agent_with(&a, &master, |d| async move { client.sign_digest(session, &master, &d).await })
                .await
        }
    };
    let a = approve_agent(&tr_hl::SigningKey::address(&tr_hl::AgentKey::generate()), "ci", 180, 1_793_667_000_000);
    for bad in [
        ApproveAgentAction { hyperliquid_chain: "Mainnet".into(), ..a.clone() },
        ApproveAgentAction { signature_chain_id: "0xa4b1".into(), ..a.clone() },
    ] {
        assert!(sign(bad).await.is_err());
    }
    assert!(mock.requests().is_empty(), "the wallet was asked to sign a mainnet action");
    // The testnet approval goes through and is the only thing signed.
    assert!(sign(a).await.is_ok());
    let paths: Vec<String> = mock.requests().into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, ["/public/v1/submit/sign_raw_payload"]);
}

/// Code added for the built-in wallet names no mainnet host either: the Turnkey crate, the
/// app's wallet modules and the sign-in page.
#[test]
fn the_wallet_code_names_no_mainnet_host() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for d in [
        "crates/turnkey/src",
        "crates/turnkey/tests",
        "crates/turnkey/examples",
        "site/src/routes/wallet",
        "site/src/lib/wallet",
    ] {
        if repo.join(d).exists() {
            sources_any(&repo.join(d), &mut files);
        }
    }
    files.push(repo.join("crates/turnkey/Cargo.toml"));
    for f in [
        "app/src/app/wallet.rs",
        "app/src/app/wallet_view.rs",
        "app/src/app/hl_web.rs",
        "app/src/app/hl.rs",
        "app/src/app/hl_view.rs",
    ] {
        if repo.join(f).exists() {
            files.push(repo.join(f));
        }
    }
    assert!(files.iter().filter(|f| f.starts_with(repo.join("crates/turnkey"))).count() >= 5, "{files:?}");
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for host in MAINNET_HOSTS {
            assert!(!text.contains(host), "{} names the mainnet host {host}", f.display());
        }
    }
}

fn sources_any(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources_any(&p, out);
        } else if p.extension().is_some_and(|x| ["rs", "toml", "ts", "svelte", "js", "html"].iter().any(|e| x == *e)) {
            out.push(p);
        }
    }
}

/// The faucet request can only go to testnet: the client has no URL to set, and the request
/// it builds is a POST to the testnet `/info` with nothing in it that names a network.
#[test]
fn the_faucet_asks_testnet_only() {
    let user = tr_hl::SigningKey::address(&tr_hl::AgentKey::generate());
    let req = tr_hl::client::Client::new().unwrap().claim_drip_request(&user).unwrap();
    assert_eq!(req.url().scheme(), "https");
    assert_eq!(req.url().host_str(), Some("api.hyperliquid-testnet.xyz"));
    assert_eq!(req.url().path(), "/info");
    assert_eq!(req.method(), "POST");
    let body: serde_json::Value = serde_json::from_slice(req.body().unwrap().as_bytes().unwrap()).unwrap();
    assert_eq!(body, serde_json::json!({"type": "claimDrip", "user": tr_hl::key::address_hex(&user)}));
    assert!(!body.to_string().to_ascii_lowercase().contains("mainnet"), "{body}");
}

/// The faucet code names no mainnet host, nor any other host on Hyperliquid's mainnet domain
/// (the testnet app's `api-ui.` included).
#[test]
fn the_faucet_code_names_no_mainnet_host() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let files = [
        "crates/hl/src/faucet.rs",
        "crates/hl/src/client.rs",
        "crates/hl/tests/fixtures/faucet_mock.json",
        "app/src/app/faucet.rs",
        "app/src/app/wallet_view.rs",
        "app/src/app/wallet.rs",
        "app/src/wallet_worker.rs",
    ];
    for f in files {
        let text = std::fs::read_to_string(repo.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        for host in MAINNET_HOSTS.iter().chain(&["hyperliquid.xyz"]) {
            assert!(!text.contains(host), "{f} names the mainnet host {host}");
        }
    }
}
