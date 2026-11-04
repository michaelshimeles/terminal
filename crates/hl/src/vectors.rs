//! Signing pinned to the official Python SDK's published test vectors
//! (`hyperliquid-python-sdk` `tests/signing_test.py`), plus an `approveAgent` vector generated
//! with SDK 0.24.0's `sign_agent` (the SDK publishes none):
//!
//! ```text
//! python3 -m venv /tmp/hl-sdk-venv && /tmp/hl-sdk-venv/bin/pip install hyperliquid-python-sdk
//! /tmp/hl-sdk-venv/bin/python -I /tmp/hl-approve-vector.py   # sign_agent(wallet, action, False)
//! ```
//!
//! The library signs only for testnet, so it is checked against the testnet half of each
//! vector. The mainnet half is checked here too, by [`mainnet_signature`], a test-only copy
//! of the phantom agent with `source: "a"`. It exists only in this test module, to show the
//! action hashing (which both networks share) is byte-exact. No library code can build it.

use alloy_primitives::{Address, B256};
use alloy_sol_types::{SolStruct, eip712_domain};
use serde::Serialize;

use crate::actions::*;
use crate::key::{AgentKey, SigningKey, sign_hash};
use crate::signing::*;

/// The SDK's public test key. Never funded; never used on a network.
const SDK_KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

fn key() -> AgentKey {
    AgentKey::from_hex(SDK_KEY).unwrap()
}

/// TEST ONLY: the mainnet phantom agent (`source: "a"`), to check the SDK's mainnet vectors.
fn mainnet_signature(k: &AgentKey, connection_id: B256) -> WireSignature {
    let domain = eip712_domain! { name: "Exchange", version: "1", chain_id: 1337, verifying_contract: Address::ZERO, };
    let agent = Agent { source: "a".into(), connectionId: connection_id };
    let sig = sign_hash(k, &agent.eip712_signing_hash(&domain)).unwrap();
    WireSignature { r: format!("{:#x}", sig.r()), s: format!("{:#x}", sig.s()), v: 27 + u8::from(sig.v()) }
}

fn sig(r: &str, s: &str, v: u8) -> WireSignature {
    WireSignature { r: r.into(), s: s.into(), v }
}

/// Check one action against both halves of an SDK vector.
fn check<A: Serialize>(action: &A, nonce: u64, mainnet: WireSignature, testnet: WireSignature) {
    let k = key();
    assert_eq!(sign_l1(&k, action, nonce, None).unwrap(), testnet, "testnet signature (the library's)");
    let id = action_hash(action, nonce, None).unwrap();
    assert_eq!(mainnet_signature(&k, id), mainnet, "mainnet signature (test-only phantom agent)");
    assert_eq!(recover_l1(action, nonce, None, &testnet).unwrap(), k.address(), "recovers the signer");
}

fn order(a: u32, sz: &str, px: &str, t: OrderTypeWire, c: Option<String>) -> OrderAction {
    OrderAction::new(vec![OrderWire { a, b: true, p: px.into(), s: sz.into(), r: false, t, c }])
}

#[derive(Serialize)]
struct Dummy {
    #[serde(rename = "type")]
    kind: &'static str,
    num: u64,
}

#[test]
fn test_phantom_agent_creation_matches_production() {
    let o = order(4, "0.0147", "1670.1", OrderTypeWire::limit(Tif::Ioc), None);
    let h = action_hash(&o, 1677777606040, None).unwrap();
    assert_eq!(format!("{h:#x}"), "0x0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908");
}

#[test]
fn test_l1_action_signing_matches() {
    // float_to_int_for_hashing(1000) = 1000 × 10^8
    let action = Dummy { kind: "dummy", num: 100_000_000_000 };
    check(
        &action,
        0,
        sig(
            "0x53749d5b30552aeb2fca34b530185976545bb22d0b3ce6f62e31be961a59298",
            "0x755c40ba9bf05223521753995abb2f73ab3229be8ec921f350cb447e384d8ed8",
            27,
        ),
        sig(
            "0x542af61ef1f429707e3c76c5293c80d01f74ef853e34b76efffcb57e574f9510",
            "0x17b8b32f086e8cdede991f1e2c529f5dd5297cbe8128500e00cbaf766204a613",
            28,
        ),
    );
}

#[test]
fn test_l1_action_signing_order_matches() {
    check(
        &order(1, "100", "100", OrderTypeWire::limit(Tif::Gtc), None),
        0,
        sig(
            "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e",
            "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e",
            28,
        ),
        sig(
            "0x82b2ba28e76b3d761093aaded1b1cdad4960b3af30212b343fb2e6cdfa4e3d54",
            "0x6b53878fc99d26047f4d7e8c90eb98955a109f44209163f52d8dc4278cbbd9f5",
            27,
        ),
    );
}

#[test]
fn test_l1_action_signing_order_with_cloid_matches() {
    check(
        &order(1, "100", "100", OrderTypeWire::limit(Tif::Gtc), Some("0x00000000000000000000000000000001".into())),
        0,
        sig(
            "0x41ae18e8239a56cacbc5dad94d45d0b747e5da11ad564077fcac71277a946e3",
            "0x3c61f667e747404fe7eea8f90ab0e76cc12ce60270438b2058324681a00116da",
            27,
        ),
        sig(
            "0xeba0664bed2676fc4e5a743bf89e5c7501aa6d870bdb9446e122c9466c5cd16d",
            "0x7f3e74825c9114bc59086f1eebea2928c190fdfbfde144827cb02b85bbe90988",
            28,
        ),
    );
}

#[test]
fn test_l1_action_signing_tpsl_order_matches() {
    check(
        &order(1, "100", "100", OrderTypeWire::trigger(true, "103".into(), Tpsl::Sl), None),
        0,
        sig(
            "0x98343f2b5ae8e26bb2587daad3863bc70d8792b09af1841b6fdd530a2065a3f9",
            "0x6b5bb6bb0633b710aa22b721dd9dee6d083646a5f8e581a20b545be6c1feb405",
            27,
        ),
        sig(
            "0x971c554d917c44e0e1b6cc45d8f9404f32172a9d3b3566262347d0302896a2e4",
            "0x206257b104788f80450f8e786c329daa589aa0b32ba96948201ae556d5637eac",
            28,
        ),
    );
}

#[test]
fn test_schedule_cancel_action() {
    check(
        &ScheduleCancelAction::new(None),
        0,
        sig(
            "0x6cdfb286702f5917e76cd9b3b8bf678fcc49aec194c02a73e6d4f16891195df9",
            "0x6557ac307fa05d25b8d61f21fb8a938e703b3d9bf575f6717ba21ec61261b2a0",
            27,
        ),
        sig(
            "0xc75bb195c3f6a4e06b7d395acc20bbb224f6d23ccff7c6a26d327304e6efaeed",
            "0x342f8ede109a29f2c0723bd5efb9e9100e3bbb493f8fb5164ee3d385908233df",
            28,
        ),
    );
    check(
        &ScheduleCancelAction::new(Some(123456789)),
        0,
        sig(
            "0x609cb20c737945d070716dcc696ba030e9976fcf5edad87afa7d877493109d55",
            "0x16c685d63b5c7a04512d73f183b3d7a00da5406ff1f8aad33f8ae2163bab758b",
            28,
        ),
        sig(
            "0x4e4f2dbd4107c69783e251b7e1057d9f2b9d11cee213441ccfa2be63516dc5bc",
            "0x706c656b23428c8ba356d68db207e11139ede1670481a9e01ae2dfcdb0e1a678",
            27,
        ),
    );
}

/// The SDK's testnet `test_sign_usd_transfer_action` checks the hand-built EIP-712 encoding
/// that `approveAgent` uses (this crate has no transfer action; the vector only exercises
/// the encoder).
#[test]
fn user_signed_encoding_matches_test_sign_usd_transfer_action() {
    let dest: Address = "0x5e9ee1089755c3435139848e47e6635505d5a13a".parse().unwrap();
    let fields = [Field::Str("Testnet"), Field::Str(&format!("{dest:#x}")), Field::Str("1"), Field::U64(1687816341423)];
    let hash = user_signed_hash(
        "HyperliquidTransaction:UsdSend(string hyperliquidChain,string destination,string amount,uint64 time)",
        &fields,
        0x66eee,
    );
    let s = sign_hash(&key(), &hash).unwrap();
    assert_eq!(format!("{:#x}", s.r()), "0x637b37dd731507cdd24f46532ca8ba6eec616952c56218baeff04144e4a77073");
    assert_eq!(format!("{:#x}", s.s()), "0x11a6a24900e6e314136d2592e2f8d502cd89b7c15b198e1bee043c9589f9fad7");
    assert_eq!(27 + u8::from(s.v()), 27);
}

/// Generated with the Python SDK 0.24.0 (`sign_agent(wallet, action, is_mainnet=False)`).
#[test]
fn approve_agent_matches_python_sdk() {
    let a = ApproveAgentAction {
        kind: "approveAgent".into(),
        hyperliquid_chain: "Testnet".into(),
        signature_chain_id: "0x66eee".into(),
        agent_address: "0x5e9ee1089755c3435139848e47e6635505d5a13a".into(),
        agent_name: "terminal-vector valid_until 1700000000000".into(),
        nonce: 1700000000000,
    };
    let main = crate::key::MainKey::from_hex(SDK_KEY).unwrap();
    let want = sig(
        "0x8138ea721e1fb5afe9239b21b19ffe1e4690dc2c06b0aaf661aeb9859f598a25",
        "0x169543c6dbf8c0caa9e6225e6612e5c3b495afbdc840537be74a6fb465cfc321",
        28,
    );
    assert_eq!(sign_approve_agent(&main, &a).unwrap(), want);
    assert_eq!(format!("{:#x}", main.address()), "0x14791697260e4c9a71f18484c9f997b308e59325");
    assert_eq!(recover_approve_agent(&a, &want).unwrap(), main.address());
    // A wallet's r ‖ s ‖ v hex (as eth_signTypedData_v4 returns it) reads as the same signature.
    let rsv = format!("0x{:0>64}{:0>64}{:02x}", &want.r[2..], &want.s[2..], want.v);
    assert_eq!(WireSignature::from_rsv_hex(&rsv).unwrap(), want);
    let zero_one = format!("{}{:02x}", &rsv[..rsv.len() - 2], want.v - 27);
    assert_eq!(WireSignature::from_rsv_hex(&zero_one).unwrap(), want, "v as 0/1");
    assert!(WireSignature::from_rsv_hex("0x1234").is_err());
    // The same fields addressed to mainnet are refused rather than signed.
    let mainnet = ApproveAgentAction { hyperliquid_chain: "Mainnet".into(), ..a.clone() };
    assert!(sign_approve_agent(&main, &mainnet).is_err());
    // The wallet payload names the same domain and message.
    let td = approve_agent_typed_data(&a).unwrap();
    assert_eq!(td["domain"]["chainId"], 0x66eee);
    assert_eq!(td["message"]["hyperliquidChain"], "Testnet");
    assert_eq!(td["primaryType"], "HyperliquidTransaction:ApproveAgent");
}

#[test]
fn expires_after_is_part_of_the_hash() {
    let n = NoopAction::default();
    let a = action_hash(&n, 5, None).unwrap();
    let b = action_hash(&n, 5, Some(1_700_000_030_000)).unwrap();
    assert_ne!(a, b);
    let s = sign_l1(&key(), &n, 5, Some(1_700_000_030_000)).unwrap();
    assert_eq!(recover_l1(&n, 5, Some(1_700_000_030_000), &s).unwrap(), key().address());
    assert_ne!(recover_l1(&n, 5, None, &s).unwrap(), key().address(), "a different expiry recovers someone else");
    // A corrupted signature recovers a different address instead of failing: the control
    // for the live check.
    let mut bad = s.clone();
    bad.s = format!(
        "{:#x}",
        alloy_primitives::U256::from_str_radix(&bad.s[2..], 16).unwrap() ^ alloy_primitives::U256::from(1)
    );
    assert_ne!(recover_l1(&n, 5, Some(1_700_000_030_000), &bad).unwrap(), key().address());
}

/// The same approval signed by a main wallet held elsewhere (as Turnkey's `sign_raw_payload`
/// with `HASH_FUNCTION_NO_OP` signs the digest it is given) is the Python SDK's signature.
#[test]
fn a_remote_main_wallet_reproduces_the_approve_agent_vector() {
    let a = ApproveAgentAction {
        kind: "approveAgent".into(),
        hyperliquid_chain: "Testnet".into(),
        signature_chain_id: "0x66eee".into(),
        agent_address: "0x5e9ee1089755c3435139848e47e6635505d5a13a".into(),
        agent_name: "terminal-vector valid_until 1700000000000".into(),
        nonce: 1700000000000,
    };
    let remote = k256::ecdsa::SigningKey::from_slice(&alloy_primitives::hex::decode(SDK_KEY).unwrap()).unwrap();
    let master = Address::from_private_key(&remote);
    let signed = futures::executor::block_on(sign_approve_agent_with(&a, &master, |digest: B256| {
        let (s, recid) = remote.sign_prehash_recoverable(digest.as_slice()).unwrap();
        async move { Ok(alloy_primitives::Signature::from_signature_and_parity(s, recid.is_y_odd())) }
    }))
    .unwrap();
    let want = sig(
        "0x8138ea721e1fb5afe9239b21b19ffe1e4690dc2c06b0aaf661aeb9859f598a25",
        "0x169543c6dbf8c0caa9e6225e6612e5c3b495afbdc840537be74a6fb465cfc321",
        28,
    );
    assert_eq!(signed, want);
}

/// `approveAgent` is only hashed for testnet's signature chain (0x66eee): never Arbitrum One
/// (0xa4b1, the chain Hyperliquid's docs show for mainnet), and never an unparseable id.
#[test]
fn approve_agent_is_hashed_only_at_the_testnet_signature_chain() {
    let a = crate::trader::approve_agent(&key().address(), "vector", 30, 1_700_000_000_000);
    assert!(approve_agent_hash(&a).is_ok());
    for chain in ["0xa4b1", "0x1", "0x66EEE0", "", "zz"] {
        let other = ApproveAgentAction { signature_chain_id: chain.into(), ..a.clone() };
        assert!(approve_agent_hash(&other).is_err(), "{chain}");
    }
    // The chain id may be written in either case.
    assert!(approve_agent_hash(&ApproveAgentAction { signature_chain_id: "0x66EEE".into(), ..a }).is_ok());
}
