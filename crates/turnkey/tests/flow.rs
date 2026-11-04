//! The desktop sign-in and the Turnkey calls, end to end against the local mock
//! (`tr_turnkey::mock`, a stand-in for Turnkey; see its docs). Live Turnkey checks are at the
//! bottom, `#[ignore]`d and skipped unless the environment names a Turnkey organization.

use std::time::Duration;

use alloy_primitives::{Address, B256};
use tr_turnkey::callback::{Answer, Callback};
use tr_turnkey::mock::{MockTurnkey, USERNAME};
use tr_turnkey::session::{SESSION_SECONDS, random_state};
use tr_turnkey::{Client, Session, SessionKey, desktop_sign_in_url};

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// What the browser does: open the page's continue link and follow its redirect to the
/// loopback callback.
async fn browser_signs_in(mock: &MockTurnkey, pk: &str, state: &str, port: u16) -> reqwest::Response {
    let url = format!("{}/wallet/complete?pk={pk}&state={state}&port={port}", mock.base);
    reqwest::get(url).await.unwrap()
}

#[tokio::test]
async fn the_desktop_signs_in_through_the_browser_and_finds_its_wallet() {
    let mock = MockTurnkey::start(0, None).await.unwrap();
    let key = SessionKey::generate();
    let pk = key.public_hex();
    let state = random_state();
    let cb = Callback::bind().await.unwrap();
    let port = cb.port();
    let url = desktop_sign_in_url(&format!("{}/wallet/", mock.base), &pk, &state, port);
    assert!(url.contains(&format!("pk={pk}")) && url.contains("flow=desktop"), "{url}");
    assert!(!url.contains(&key.to_hex()[..]), "the private key never goes in the URL");

    let waiter = tokio::spawn({
        let state = state.clone();
        async move { cb.wait(&state, Duration::from_secs(20)).await }
    });
    let page = browser_signs_in(&mock, &pk, &state, port).await;
    assert_eq!(page.status(), 200, "the loopback answered the redirected browser");
    assert!(page.text().await.unwrap().contains("Signed in"));

    let Answer::Session(token) = waiter.await.unwrap().unwrap() else { panic!("no session") };
    let session = Session::accept(key, &token, now_secs()).unwrap();
    assert!(session.remaining(now_secs()) <= SESSION_SECONDS);

    let client = Client::with_base(&mock.base).unwrap();
    let me = client.whoami(&session).await.unwrap();
    assert_eq!(me.username, USERNAME);
    let account = client.ethereum_account(&session).await.unwrap();
    assert_eq!(account.address.to_ascii_lowercase(), mock.address());
    assert_eq!(account.path, "m/44'/60'/0'/0/0");
}

#[tokio::test]
async fn a_request_without_this_sign_ins_state_does_not_end_it() {
    let mock = MockTurnkey::start(0, None).await.unwrap();
    let key = SessionKey::generate();
    let state = random_state();
    let cb = Callback::bind().await.unwrap();
    let port = cb.port();
    let waiter = tokio::spawn({
        let state = state.clone();
        async move { cb.wait(&state, Duration::from_secs(20)).await }
    });
    // Another tab (or program) with the wrong state, and a stray path.
    let stray = browser_signs_in(&mock, &key.public_hex(), &random_state(), port).await;
    assert_eq!(stray.status(), 400);
    let favicon = reqwest::get(format!("http://127.0.0.1:{port}/favicon.ico")).await.unwrap();
    assert_eq!(favicon.status(), 404);
    assert!(!waiter.is_finished());
    // The real one still gets through.
    browser_signs_in(&mock, &key.public_hex(), &state, port).await;
    assert!(matches!(waiter.await.unwrap().unwrap(), Answer::Session(_)));
}

#[tokio::test]
async fn the_sign_in_gives_up_after_its_timeout() {
    let cb = Callback::bind().await.unwrap();
    let err = cb.wait(&random_state(), Duration::from_millis(200)).await.unwrap_err();
    assert!(err.to_string().contains("didn't come back"), "{err}");
}

#[tokio::test]
async fn a_session_for_another_key_or_one_turnkey_never_issued_is_refused() {
    let mock = MockTurnkey::start(0, None).await.unwrap();
    let mine = SessionKey::generate();
    let theirs = SessionKey::generate();
    // A page that hands back a session for someone else's key: refused before any request.
    let token = mock.mint_session(&theirs.public_hex(), 60);
    assert!(Session::accept(SessionKey::from_hex(&mine.to_hex()).unwrap(), &token, now_secs()).is_err());

    // Turnkey (here the mock) refuses stamps from keys it issued no session for.
    let unknown = SessionKey::generate();
    let pk = unknown.public_hex();
    let fake = tr_turnkey::session::unsigned_token(&tr_turnkey::session::Claims {
        public_key: pk,
        ..tr_turnkey::session::Claims::parse(&token).unwrap()
    });
    let s = Session::accept(unknown, &fake, now_secs()).unwrap();
    let err = Client::with_base(&mock.base).unwrap().whoami(&s).await.unwrap_err();
    assert!(err.to_string().contains("HTTP 401"), "{err}");
    assert!(mock.requests().is_empty());
}

/// `sign_digest` sends what `alloy-signer-turnkey` sends, and the signature recovers the
/// wallet.
#[tokio::test]
async fn the_wallet_signs_exactly_the_digest_it_is_given() {
    let mock = MockTurnkey::start(0, None).await.unwrap();
    let key = SessionKey::generate();
    let token = mock.mint_session(&key.public_hex(), 60);
    let session = Session::accept(key, &token, now_secs()).unwrap();
    let client = Client::with_base(&mock.base).unwrap();
    let wallet: Address = mock.address().parse().unwrap();
    let digest = B256::from([7u8; 32]);
    let sig = client.sign_digest(&session, &wallet, &digest).await.unwrap();
    assert_eq!(sig.recover_address_from_prehash(&digest).unwrap(), wallet);
    let (path, body) = mock.requests().pop().unwrap();
    assert_eq!(path, "/public/v1/submit/sign_raw_payload");
    let mut want = tr_turnkey::client::sign_raw_payload_body(tr_turnkey::mock::ORG_ID, &wallet, &digest, 0);
    want["timestampMs"] = body["timestampMs"].clone();
    assert_eq!(body, want);
    // Another address is not this organization's wallet: Turnkey refuses, nothing is signed.
    let other = Address::from([1u8; 20]);
    assert!(client.sign_digest(&session, &other, &digest).await.is_err());
}

// ---- live Turnkey (needs an organization and an API key from Micky) ------------------------

/// `TURNKEY_ORGANIZATION_ID`, `TURNKEY_API_PRIVATE_KEY` (hex P-256) and
/// `TURNKEY_SIGN_WITH` (an Ethereum address in that organization). Skips when unset.
fn live() -> Option<(Session, Address)> {
    let org = std::env::var("TURNKEY_ORGANIZATION_ID").ok()?;
    let key = SessionKey::from_hex(&std::env::var("TURNKEY_API_PRIVATE_KEY").ok()?).ok()?;
    let with: Address = std::env::var("TURNKEY_SIGN_WITH").ok()?.parse().ok()?;
    Some((Session::for_api_key(key, &org), with))
}

#[tokio::test]
#[ignore = "live Turnkey: set TURNKEY_ORGANIZATION_ID, TURNKEY_API_PRIVATE_KEY, TURNKEY_SIGN_WITH"]
async fn live_turnkey_signs_a_digest_that_recovers_the_wallet() {
    let Some((session, with)) = live() else {
        eprintln!("skipped: no Turnkey organization in the environment");
        return;
    };
    let client = Client::new().unwrap();
    client.whoami(&session).await.unwrap();
    let digest = B256::from([0x42; 32]);
    let sig = client.sign_digest(&session, &with, &digest).await.unwrap();
    assert_eq!(sig.recover_address_from_prehash(&digest).unwrap(), with);
}
