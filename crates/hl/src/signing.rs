//! Signatures, as the Python SDK's `hyperliquid/utils/signing.py` makes them.
//!
//! **L1 actions** (order, cancel, modify, leverage, schedule cancel, noop): the action is
//! msgpack-encoded with named fields, followed by the nonce (8 bytes, big-endian), `0x00`
//! (no vault) and, when set, `0x00 ‖ expiresAfter`. Its keccak-256 is the `connectionId` of
//! a "phantom agent" `Agent(string source, bytes32 connectionId)`, EIP-712-signed under the
//! domain `Exchange` v1, chain 1337. `source` is always testnet's `"b"`
//! ([`Network::phantom_source`]); nothing here can produce mainnet's.
//!
//! **User-signed actions** (`approveAgent`): EIP-712 over the action's own fields under the
//! domain `HyperliquidSignTransaction` v1 at `signatureChainId`, with `hyperliquidChain`
//! always `"Testnet"`.
//!
//! L1 signing is crate-private: callers go through [`crate::Trader`], which enforces the
//! hard caps before anything is signed.

use alloy_primitives::{Address, B256, Signature, U256, keccak256};
use alloy_sol_types::{SolStruct, eip712_domain, sol};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::actions::ApproveAgentAction;
use crate::key::{SigningKey, sign_hash};
use crate::network::{Network, SIGNATURE_CHAIN_ID};

sol! {
    struct Agent {
        string source;
        bytes32 connectionId;
    }
}

/// `{r, s, v}` as Hyperliquid takes it: `0x` hex without leading zeros (as the SDK's
/// `to_hex`), and `v` 27 or 28.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSignature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

impl WireSignature {
    fn from_alloy(sig: &Signature) -> Self {
        Self { r: format!("{:#x}", sig.r()), s: format!("{:#x}", sig.s()), v: 27 + u8::from(sig.v()) }
    }

    /// From a wallet's 65-byte `0x` r ‖ s ‖ v hex (`eth_signTypedData_v4`); `v` may be 0/1.
    pub fn from_rsv_hex(hex: &str) -> Result<Self> {
        let h = hex.trim().trim_start_matches("0x");
        ensure!(
            h.len() == 130 && h.chars().all(|c| c.is_ascii_hexdigit()),
            "the wallet returned a malformed signature"
        );
        let v = u8::from_str_radix(&h[128..130], 16)?;
        let num = |s: &str| U256::from_str_radix(s, 16).map(|n| format!("{n:#x}"));
        Ok(Self { r: num(&h[..64])?, s: num(&h[64..128])?, v: if v < 27 { v + 27 } else { v } })
    }

    fn to_alloy(&self) -> Result<Signature> {
        let num = |s: &str| U256::from_str_radix(s.trim_start_matches("0x"), 16).context("signature is not hex");
        ensure!(self.v == 27 || self.v == 28, "signature v must be 27 or 28");
        Ok(Signature::new(num(&self.r)?, num(&self.s)?, self.v == 28))
    }
}

/// The L1 `connectionId`: keccak-256 of msgpack(action) ‖ nonce ‖ `0x00` ‖ [`0x00` ‖ expiresAfter].
pub fn action_hash<A: Serialize>(action: &A, nonce: u64, expires_after: Option<u64>) -> Result<B256> {
    let mut data = rmp_serde::to_vec_named(action).context("msgpack-encoding the action")?;
    data.extend_from_slice(&nonce.to_be_bytes());
    // No vault or sub-account: this build only trades the signer's own account.
    data.push(0);
    if let Some(t) = expires_after {
        data.push(0);
        data.extend_from_slice(&t.to_be_bytes());
    }
    Ok(keccak256(data))
}

/// The EIP-712 hash of the testnet phantom agent for `connection_id`.
pub(crate) fn agent_signing_hash(connection_id: B256) -> B256 {
    let domain = eip712_domain! {
        name: "Exchange",
        version: "1",
        chain_id: 1337,
        verifying_contract: Address::ZERO,
    };
    let agent = Agent { source: Network::Testnet.phantom_source().into(), connectionId: connection_id };
    agent.eip712_signing_hash(&domain)
}

/// Sign an L1 action for testnet. Crate-private: see [`crate::Trader`].
pub(crate) fn sign_l1<A: Serialize>(
    key: &impl SigningKey,
    action: &A,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<WireSignature> {
    let hash = agent_signing_hash(action_hash(action, nonce, expires_after)?);
    let sig = sign_hash(key, &hash).context("signing")?;
    Ok(WireSignature::from_alloy(&sig))
}

/// The address that signed an L1 action, as Hyperliquid recovers it. A wrong signature
/// recovers some other address rather than failing, which is how Hyperliquid reports it
/// ("User or API Wallet 0x… does not exist").
pub fn recover_l1<A: Serialize>(
    action: &A,
    nonce: u64,
    expires_after: Option<u64>,
    sig: &WireSignature,
) -> Result<Address> {
    let hash = agent_signing_hash(action_hash(action, nonce, expires_after)?);
    Ok(sig.to_alloy()?.recover_address_from_prehash(&hash)?)
}

// ---- user-signed actions -------------------------------------------------------------------

/// One EIP-712 field value, encoded to 32 bytes.
pub(crate) enum Field<'a> {
    Str(&'a str),
    Addr(Address),
    U64(u64),
}

impl Field<'_> {
    fn encode(&self) -> B256 {
        match self {
            Field::Str(s) => keccak256(s.as_bytes()),
            Field::Addr(a) => a.into_word(),
            Field::U64(n) => B256::from(U256::from(*n)),
        }
    }
}

/// The EIP-712 signing hash of a `HyperliquidTransaction:*` user-signed action.
pub(crate) fn user_signed_hash(type_string: &str, fields: &[Field<'_>], chain_id: u64) -> B256 {
    let mut enc = Vec::with_capacity(32 * (fields.len() + 1));
    enc.extend_from_slice(keccak256(type_string.as_bytes()).as_slice());
    for f in fields {
        enc.extend_from_slice(f.encode().as_slice());
    }
    let struct_hash = keccak256(&enc);
    let domain = eip712_domain! {
        name: "HyperliquidSignTransaction",
        version: "1",
        chain_id: chain_id,
        verifying_contract: Address::ZERO,
    };
    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(&[0x19, 0x01]);
    msg.extend_from_slice(domain.separator().as_slice());
    msg.extend_from_slice(struct_hash.as_slice());
    keccak256(msg)
}

const APPROVE_AGENT_TYPE: &str =
    "HyperliquidTransaction:ApproveAgent(string hyperliquidChain,address agentAddress,string agentName,uint64 nonce)";

/// The hash the main wallet signs to approve an agent. Refuses anything not addressed to
/// testnet: `hyperliquidChain` must be `Testnet` and `signatureChainId` the testnet
/// signature chain ([`SIGNATURE_CHAIN_ID`], 0x66eee), never Arbitrum One's 0xa4b1.
pub fn approve_agent_hash(a: &ApproveAgentAction) -> Result<B256> {
    ensure!(a.kind == "approveAgent", "not an approveAgent action");
    ensure!(a.hyperliquid_chain == Network::Testnet.hyperliquid_chain(), "this build only approves agents on testnet");
    ensure!(
        a.chain_id().ok() == Some(SIGNATURE_CHAIN_ID),
        "this build only signs at the testnet signature chain ({SIGNATURE_CHAIN_ID:#x})"
    );
    let agent = crate::key::parse_address(&a.agent_address)?;
    let fields = [Field::Str(&a.hyperliquid_chain), Field::Addr(agent), Field::Str(&a.agent_name), Field::U64(a.nonce)];
    Ok(user_signed_hash(APPROVE_AGENT_TYPE, &fields, a.chain_id()?))
}

/// The main wallet signs `approveAgent`.
pub fn sign_approve_agent(main: &impl SigningKey, a: &ApproveAgentAction) -> Result<WireSignature> {
    let sig = sign_hash(main, &approve_agent_hash(a)?).context("signing")?;
    Ok(WireSignature::from_alloy(&sig))
}

/// A main wallet held elsewhere (the built-in Turnkey wallet) signs `approveAgent`.
///
/// `sign` gets only the digest of a testnet approval: [`approve_agent_hash`] refuses
/// anything else before `sign` is called. The answer is used only if it recovers `main`,
/// so a wrong or swapped signature is never sent. This is the only way a remote wallet's
/// signature enters this crate; L1 actions (orders, cancels) are signed by the local agent
/// key alone.
pub async fn sign_approve_agent_with<F, Fut>(a: &ApproveAgentAction, main: &Address, sign: F) -> Result<WireSignature>
where
    F: FnOnce(B256) -> Fut,
    Fut: std::future::Future<Output = Result<Signature>>,
{
    let digest = approve_agent_hash(a)?;
    let sig = sign(digest).await.context("the wallet didn't sign")?;
    let signer = sig.recover_address_from_prehash(&digest).context("the wallet's signature is malformed")?;
    ensure!(
        &signer == main,
        "the signature is not the wallet's ({}); nothing was sent",
        crate::key::mask_address(&signer)
    );
    Ok(WireSignature::from_alloy(&sig))
}

/// Who signed an `approveAgent`.
pub fn recover_approve_agent(a: &ApproveAgentAction, sig: &WireSignature) -> Result<Address> {
    Ok(sig.to_alloy()?.recover_address_from_prehash(&approve_agent_hash(a)?)?)
}

/// The EIP-712 typed data of an approval, as a wallet's `eth_signTypedData_v4` takes it
/// (the tests check it names the same domain and message as the digest).
pub fn approve_agent_typed_data(a: &ApproveAgentAction) -> Result<serde_json::Value> {
    approve_agent_hash(a)?;
    Ok(serde_json::json!({
        "domain": {
            "name": "HyperliquidSignTransaction",
            "version": "1",
            "chainId": a.chain_id()?,
            "verifyingContract": "0x0000000000000000000000000000000000000000",
        },
        "types": {
            "HyperliquidTransaction:ApproveAgent": [
                {"name": "hyperliquidChain", "type": "string"},
                {"name": "agentAddress", "type": "address"},
                {"name": "agentName", "type": "string"},
                {"name": "nonce", "type": "uint64"},
            ],
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"},
            ],
        },
        "primaryType": "HyperliquidTransaction:ApproveAgent",
        "message": {
            "hyperliquidChain": a.hyperliquid_chain,
            "agentAddress": a.agent_address,
            "agentName": a.agent_name,
            "nonce": a.nonce,
        },
    }))
}
