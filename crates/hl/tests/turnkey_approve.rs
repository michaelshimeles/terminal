//! `approveAgent` signed by the built-in wallet, through the Turnkey client, against the
//! local Turnkey stand-in (`tr_turnkey::mock`, not Turnkey). The wallet there is the Python
//! SDK's public test key, so the signature must be the SDK's own vector.

use alloy_primitives::Address;
use tr_hl::actions::ApproveAgentAction;
use tr_hl::signing::{approve_agent_hash, sign_approve_agent_with};
use tr_turnkey::mock::MockTurnkey;
use tr_turnkey::{Client, Session, SessionKey};

/// The SDK's public test key (signing_test.py). Never funded; never used on a network.
const SDK_KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[tokio::test]
async fn the_built_in_wallet_reproduces_the_python_sdk_vector() {
    let mock = MockTurnkey::start(0, Some(SDK_KEY)).await.unwrap();
    let key = SessionKey::generate();
    let session = Session::accept(
        SessionKey::from_hex(&key.to_hex()).unwrap(),
        &mock.mint_session(&key.public_hex(), 60),
        now_secs(),
    )
    .unwrap();
    let client = Client::with_base(&mock.base).unwrap();
    let master: Address = client.ethereum_account(&session).await.unwrap().address.parse().unwrap();
    assert_eq!(format!("{master:#x}"), "0x14791697260e4c9a71f18484c9f997b308e59325");

    let a = ApproveAgentAction {
        kind: "approveAgent".into(),
        hyperliquid_chain: "Testnet".into(),
        signature_chain_id: "0x66eee".into(),
        agent_address: "0x5e9ee1089755c3435139848e47e6635505d5a13a".into(),
        agent_name: "terminal-vector valid_until 1700000000000".into(),
        nonce: 1700000000000,
    };
    let sig = sign_approve_agent_with(&a, &master, |d| {
        let (client, session) = (&client, &session);
        async move { client.sign_digest(session, &master, &d).await }
    })
    .await
    .unwrap();
    assert_eq!(sig.r, "0x8138ea721e1fb5afe9239b21b19ffe1e4690dc2c06b0aaf661aeb9859f598a25");
    assert_eq!(sig.s, "0x169543c6dbf8c0caa9e6225e6612e5c3b495afbdc840537be74a6fb465cfc321");
    assert_eq!(sig.v, 28);
    // Turnkey was handed exactly the testnet approval's digest.
    let signed: Vec<_> = mock.requests().into_iter().filter(|(p, _)| p.ends_with("/sign_raw_payload")).collect();
    assert_eq!(signed.len(), 1);
    let digest = approve_agent_hash(&a).unwrap();
    assert_eq!(signed[0].1["parameters"]["payload"], alloy_primitives::hex::encode(digest));
}
