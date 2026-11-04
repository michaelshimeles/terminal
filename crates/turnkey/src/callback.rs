//! Desktop sign-in: the browser hands the session back to Terminal by redirecting to
//! `http://127.0.0.1:<port>/callback?state=…&session=…` (a loopback redirect, RFC 8252 §7.3).
//!
//! - The listener binds 127.0.0.1 on a port the OS picks, so nothing off this machine can
//!   reach it, and it closes after one good answer or [`SIGN_IN_TIMEOUT`].
//! - `state` is 256 random bits made for this sign-in. Requests without it (another tab,
//!   another program poking the port) get an error page and don't end the wait.
//! - The session token is bound to this device's key ([`crate::session::Session::accept`]
//!   checks that), so even an intercepted redirect is useless to anyone else.
//! - Only `GET /callback` is answered; requests are read up to 8 KiB.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::session::same_secret;

pub use crate::SIGN_IN_TIMEOUT;
/// Bad requests tolerated before the sign-in gives up.
const MAX_STRAY: usize = 32;
const MAX_REQUEST: usize = 8 * 1024;

/// What the browser came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The session token for this device's key.
    Session(String),
    /// The page reported that the user cancelled or sign-in failed.
    Failed(String),
}

/// A loopback listener waiting for one sign-in.
pub struct Callback {
    listener: TcpListener,
    port: u16,
}

impl Callback {
    /// Listen on 127.0.0.1 at a port the OS picks.
    pub async fn bind() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("listening on 127.0.0.1 for the sign-in")?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for `GET /callback` carrying `state`, up to `timeout`.
    pub async fn wait(self, state: &str, timeout: Duration) -> Result<Answer> {
        tokio::time::timeout(timeout, self.serve(state))
            .await
            .map_err(|_| anyhow!("the browser didn't come back within {} minutes; try again", timeout.as_secs() / 60))?
    }

    async fn serve(self, state: &str) -> Result<Answer> {
        let mut stray = 0;
        loop {
            let (mut sock, _) = self.listener.accept().await?;
            match read_request(&mut sock).await {
                Ok(Some(target)) => match judge(&target, state) {
                    Verdict::Done(answer) => {
                        let page = match &answer {
                            Answer::Session(_) => PAGE_DONE,
                            Answer::Failed(_) => PAGE_FAILED,
                        };
                        let _ = respond(&mut sock, "200 OK", page).await;
                        return Ok(answer);
                    }
                    Verdict::NotFound => {
                        let _ = respond(&mut sock, "404 Not Found", PAGE_STRAY).await;
                    }
                    Verdict::WrongState => {
                        let _ = respond(&mut sock, "400 Bad Request", PAGE_STRAY).await;
                    }
                },
                Ok(None) | Err(_) => {
                    let _ = respond(&mut sock, "400 Bad Request", PAGE_STRAY).await;
                }
            }
            stray += 1;
            if stray >= MAX_STRAY {
                bail!("too many unexpected requests reached the sign-in listener; try again");
            }
        }
    }
}

enum Verdict {
    Done(Answer),
    WrongState,
    NotFound,
}

/// Decide what a request target (`/callback?…`) means for the sign-in with `state`.
fn judge(target: &str, state: &str) -> Verdict {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        return Verdict::NotFound;
    }
    let get = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')).map(percent_decode);
    match get("state") {
        Some(s) if same_secret(&s, state) => {}
        _ => return Verdict::WrongState,
    }
    match (get("session"), get("error")) {
        (Some(token), None) if !token.is_empty() => Verdict::Done(Answer::Session(token)),
        (_, Some(e)) => Verdict::Done(Answer::Failed(e.chars().filter(|c| !c.is_control()).take(200).collect())),
        _ => Verdict::WrongState,
    }
}

/// The request target of a `GET`, or `None` for anything else.
async fn read_request(sock: &mut TcpStream) -> Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return Ok(None);
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut first = text.lines().next().unwrap_or_default().split(' ');
    match (first.next(), first.next()) {
        (Some("GET"), Some(target)) => Ok(Some(target.to_string())),
        _ => Ok(None),
    }
}

async fn respond(sock: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.shutdown().await?;
    Ok(())
}

/// `%XX` and `+` decoding for a query value.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const PAGE_DONE: &str = concat!(
    "<!doctype html><meta charset=utf-8><title>Terminal</title><style>",
    "body{font:15px -apple-system,system-ui,sans-serif;background:#0b0b0c;color:#e6e6e6;display:grid;place-items:center;height:100vh;margin:0}main{max-width:420px;text-align:center}b{color:#f5a623}",
    "</style><main><h2>Signed in</h2><p>Your wallet is connected to Terminal. You can close this tab.</p><p><b>TESTNET</b></p></main>"
);
const PAGE_FAILED: &str = concat!(
    "<!doctype html><meta charset=utf-8><title>Terminal</title><style>",
    "body{font:15px -apple-system,system-ui,sans-serif;background:#0b0b0c;color:#e6e6e6;display:grid;place-items:center;height:100vh;margin:0}main{max-width:420px;text-align:center}",
    "</style><main><h2>Not signed in</h2><p>Terminal got the answer. Go back to it to try again.</p></main>"
);
const PAGE_STRAY: &str = "<!doctype html><meta charset=utf-8><title>Terminal</title><p>This sign-in link isn't for this Terminal window.</p>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_callback_with_this_state_ends_the_sign_in() {
        let st = "ab".repeat(32);
        let ok = format!("/callback?state={st}&session=h.p.s");
        assert!(matches!(judge(&ok, &st), Verdict::Done(Answer::Session(t)) if t == "h.p.s"));
        let failed = format!("/callback?error=cancelled+by+user&state={st}");
        assert!(matches!(judge(&failed, &st), Verdict::Done(Answer::Failed(e)) if e == "cancelled by user"));
        for bad in [
            format!("/callback?state={}&session=h.p.s", "cd".repeat(32)),
            "/callback?session=h.p.s".to_string(),
            format!("/callback?state={}&session=h.p.s", &st[..63]),
            format!("/callback?state={st}"),
            format!("/callback?state={st}&session="),
        ] {
            assert!(matches!(judge(&bad, &st), Verdict::WrongState), "{bad}");
        }
        assert!(matches!(judge(&format!("/other?state={st}&session=x"), &st), Verdict::NotFound));
        assert!(matches!(judge("/favicon.ico", &st), Verdict::NotFound));
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
