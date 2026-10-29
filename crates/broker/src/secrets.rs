//! SnapTrade credentials and where they are kept.
//!
//! The partner keys (`clientId`, `consumerKey`) and the per-user `userId`/`userSecret` are
//! stored together as one small JSON document: in the macOS Keychain (generic password,
//! service `terminal`, account `snaptrade`), or elsewhere in an owner-only (0600) file in
//! the app's data directory. `SNAPTRADE_*` environment variables override stored values
//! field by field. Nothing is ever written inside the repository.
//!
//! Before the app was renamed from terminal-rs, the Keychain service was `terminal-rs`; the
//! first load moves credentials from there.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const KEYCHAIN_SERVICE: &str = "terminal";
/// The service credentials were saved under before the rename.
pub const LEGACY_KEYCHAIN_SERVICE: &str = "terminal-rs";
pub const KEYCHAIN_ACCOUNT: &str = "snaptrade";

/// Environment variables that override stored credentials.
pub const ENV_CLIENT_ID: &str = "SNAPTRADE_CLIENT_ID";
pub const ENV_CONSUMER_KEY: &str = "SNAPTRADE_CONSUMER_KEY";
pub const ENV_USER_ID: &str = "SNAPTRADE_USER_ID";
pub const ENV_USER_SECRET: &str = "SNAPTRADE_USER_SECRET";

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub consumer_key: String,
    /// Empty for personal API keys, which act on the key owner's own connections.
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_secret: String,
}

impl Credentials {
    /// Partner keys are present (the minimum to call the API).
    pub fn has_keys(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.consumer_key.trim().is_empty()
    }

    /// A SnapTrade user is registered (commercial keys) or not needed (personal keys).
    pub fn has_user(&self) -> bool {
        !self.user_id.trim().is_empty() && !self.user_secret.trim().is_empty()
    }

    /// Ready to read accounts: keys plus either a full user or no user at all (personal key).
    pub fn is_ready(&self) -> bool {
        self.has_keys() && (self.has_user() || (self.user_id.trim().is_empty() && self.user_secret.trim().is_empty()))
    }

    /// Fill fields from `SNAPTRADE_*` variables where they are set. Returns whether any were.
    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> bool {
        let mut any = false;
        for (var, field) in [
            (ENV_CLIENT_ID, &mut self.client_id),
            (ENV_CONSUMER_KEY, &mut self.consumer_key),
            (ENV_USER_ID, &mut self.user_id),
            (ENV_USER_SECRET, &mut self.user_secret),
        ] {
            if let Some(v) = get(var).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
                *field = v;
                any = true;
            }
        }
        any
    }

    fn trimmed(&self) -> Self {
        Self {
            client_id: self.client_id.trim().into(),
            consumer_key: self.consumer_key.trim().into(),
            user_id: self.user_id.trim().into(),
            user_secret: self.user_secret.trim().into(),
        }
    }
}

/// Secrets are never printed, even in debug logs.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |s: &str| if s.is_empty() { "<empty>" } else { "<set>" };
        f.debug_struct("Credentials")
            .field("client_id", &self.client_id)
            .field("consumer_key", &set(&self.consumer_key))
            .field("user_id", &self.user_id)
            .field("user_secret", &set(&self.user_secret))
            .finish()
    }
}

/// Mask a secret for display: `••••••••3f9a`.
pub fn mask(secret: &str) -> String {
    let s = secret.trim();
    if s.is_empty() {
        return String::new();
    }
    let tail: String = s.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if s.chars().count() <= 8 { "••••••••".into() } else { format!("••••••••{tail}") }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// macOS Keychain generic password under `service`, moved there from `legacy` on first load.
    Keychain { service: String, legacy: Option<String> },
    /// Owner-only JSON file.
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct SecretStore {
    pub backend: Backend,
}

impl SecretStore {
    /// The platform store: the Keychain on macOS (unless `TR_SECRET_STORE=file`), otherwise
    /// `<data_dir>/secrets/snaptrade.json`.
    pub fn platform(data_dir: &Path) -> Self {
        let file = Backend::File(data_dir.join("secrets").join("snaptrade.json"));
        let force_file = std::env::var("TR_SECRET_STORE").is_ok_and(|v| v.eq_ignore_ascii_case("file"));
        let backend = if cfg!(target_os = "macos") && !force_file {
            Backend::Keychain { service: KEYCHAIN_SERVICE.into(), legacy: Some(LEGACY_KEYCHAIN_SERVICE.into()) }
        } else {
            file
        };
        Self { backend }
    }

    /// The Keychain under `service`, taking over an item left under `legacy`.
    pub fn keychain(service: &str, legacy: Option<&str>) -> Self {
        Self { backend: Backend::Keychain { service: service.into(), legacy: legacy.map(Into::into) } }
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self { backend: Backend::File(path.into()) }
    }

    /// Where credentials live, for the settings screen.
    pub fn describe(&self) -> String {
        match &self.backend {
            Backend::Keychain { service, .. } => format!("macOS Keychain ({service} / {KEYCHAIN_ACCOUNT})"),
            Backend::File(p) => format!("{} (owner-only)", p.display()),
        }
    }

    pub fn load(&self) -> Result<Option<Credentials>> {
        let text = match &self.backend {
            Backend::Keychain { service, legacy } => load_keychain(service, legacy.as_deref())?,
            Backend::File(p) => match std::fs::read_to_string(p) {
                Ok(t) => Some(t),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
            },
        };
        match text {
            Some(t) if !t.trim().is_empty() => {
                Ok(Some(serde_json::from_str(&t).context("SnapTrade credentials are not valid JSON")?))
            }
            _ => Ok(None),
        }
    }

    pub fn save(&self, creds: &Credentials) -> Result<()> {
        let json = serde_json::to_string(&creds.trimmed())?;
        match &self.backend {
            Backend::Keychain { service, .. } => keychain::set(service, &json),
            Backend::File(p) => write_private(p, &json),
        }
    }

    /// Remove stored credentials (environment variables are unaffected).
    pub fn clear(&self) -> Result<()> {
        match &self.backend {
            Backend::Keychain { service, legacy } => {
                keychain::delete(service)?;
                // The old item too, or the credentials would come back from it on the next load.
                legacy.as_deref().map_or(Ok(()), keychain::delete)
            }
            Backend::File(p) => match std::fs::remove_file(p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
        }
    }
}

/// Reads the Keychain item under `service`. If there is none, the item under `legacy` moves
/// there: it is deleted only after the copy is written, so a failed move leaves it in place.
fn load_keychain(service: &str, legacy: Option<&str>) -> Result<Option<String>> {
    if let Some(text) = keychain::get(service)? {
        return Ok(Some(text));
    }
    let Some(legacy) = legacy else { return Ok(None) };
    let Some(text) = keychain::get(legacy)? else { return Ok(None) };
    match keychain::set(service, &text) {
        Ok(()) => match keychain::delete(legacy) {
            Ok(()) => tracing::info!("moved SnapTrade credentials from Keychain service {legacy} to {service}"),
            Err(e) => tracing::warn!("removing SnapTrade credentials from Keychain service {legacy}: {e:#}"),
        },
        Err(e) => tracing::warn!("moving SnapTrade credentials to Keychain service {service}: {e:#}"),
    }
    Ok(Some(text))
}

/// Write `text` to `path` readable only by the owner (0700 directory, 0600 file), replacing it
/// atomically.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().context("credentials path has no directory")?;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod keychain {
    use anyhow::{Context, Result};
    use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

    use super::KEYCHAIN_ACCOUNT;

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    pub fn get(service: &str) -> Result<Option<String>> {
        match get_generic_password(service, KEYCHAIN_ACCOUNT) {
            Ok(bytes) => Ok(Some(String::from_utf8(bytes).context("Keychain item is not UTF-8")?)),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(anyhow::anyhow!("reading the Keychain: {e}")),
        }
    }

    pub fn set(service: &str, value: &str) -> Result<()> {
        set_generic_password(service, KEYCHAIN_ACCOUNT, value.as_bytes())
            .map_err(|e| anyhow::anyhow!("writing the Keychain: {e}"))
    }

    pub fn delete(service: &str) -> Result<()> {
        match delete_generic_password(service, KEYCHAIN_ACCOUNT) {
            Err(e) if e.code() != NOT_FOUND => Err(anyhow::anyhow!("deleting from the Keychain: {e}")),
            _ => Ok(()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    use anyhow::{Result, bail};

    pub fn get(_: &str) -> Result<Option<String>> {
        bail!("the Keychain is only available on macOS")
    }
    pub fn set(_: &str, _: &str) -> Result<()> {
        bail!("the Keychain is only available on macOS")
    }
    pub fn delete(_: &str) -> Result<()> {
        bail!("the Keychain is only available on macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials {
            client_id: "PARTNER-ID".into(),
            consumer_key: "consumer-key-0123456789".into(),
            user_id: "terminal-1".into(),
            user_secret: "user-secret-abcdef".into(),
        }
    }

    #[test]
    fn readiness() {
        assert!(!Credentials::default().has_keys());
        assert!(creds().is_ready());
        let personal = Credentials { user_id: String::new(), user_secret: String::new(), ..creds() };
        assert!(personal.is_ready() && !personal.has_user());
        let half = Credentials { user_secret: String::new(), ..creds() };
        assert!(!half.is_ready(), "a user id without its secret cannot sign user requests");
    }

    #[test]
    fn debug_never_prints_secrets() {
        let d = format!("{:?}", creds());
        assert!(!d.contains("consumer-key") && !d.contains("user-secret"), "{d}");
        assert!(d.contains("PARTNER-ID"));
        assert_eq!(mask("abcdefghijkl"), "••••••••ijkl");
        assert_eq!(mask("short"), "••••••••");
        assert_eq!(mask(""), "");
    }

    #[test]
    fn env_overrides_fields() {
        let mut c = Credentials { client_id: "stored".into(), ..Default::default() };
        let any = c.apply_env(|k| match k {
            ENV_CONSUMER_KEY => Some(" from-env ".into()),
            ENV_CLIENT_ID => Some(String::new()),
            _ => None,
        });
        assert!(any);
        assert_eq!(c.client_id, "stored", "empty variables do not override");
        assert_eq!(c.consumer_key, "from-env");
    }

    #[test]
    fn file_store_round_trip_is_private() {
        let dir = std::env::temp_dir().join(format!("tr-broker-secrets-{}", std::process::id()));
        let store = SecretStore::file(dir.join("secrets/snaptrade.json"));
        assert_eq!(store.load().unwrap(), None);
        let mut c = creds();
        c.client_id = "  PARTNER-ID ".into();
        store.save(&c).unwrap();
        assert_eq!(store.load().unwrap(), Some(creds()), "values are trimmed on save");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("secrets/snaptrade.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dmode = std::fs::metadata(dir.join("secrets")).unwrap().permissions().mode();
            assert_eq!(dmode & 0o777, 0o700);
        }
        store.clear().unwrap();
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes throwaway items (dummy credentials) to the login Keychain, so it runs only when
    /// asked: `cargo test -p tr-broker -- --ignored keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes to the login Keychain"]
    fn keychain_moves_credentials_from_the_old_service() {
        let pid = std::process::id();
        let (service, legacy) = (format!("terminal-test-{pid}"), format!("terminal-rs-test-{pid}"));
        let store = SecretStore::keychain(&service, Some(&legacy));
        let new_only = SecretStore::keychain(&service, None);
        let old_only = SecretStore::keychain(&legacy, None);
        old_only.save(&creds()).unwrap();

        assert_eq!(store.load().unwrap(), Some(creds()), "found under the old service");
        assert_eq!(new_only.load().unwrap(), Some(creds()), "and moved to the new one");
        assert_eq!(old_only.load().unwrap(), None, "the old item is gone");

        // A stale item under the old service never replaces what the new one holds.
        old_only.save(&Credentials { client_id: "STALE".into(), ..creds() }).unwrap();
        assert_eq!(store.load().unwrap(), Some(creds()));

        // Forgetting credentials removes both, so the old item can't bring them back.
        store.clear().unwrap();
        assert_eq!(old_only.load().unwrap(), None);
        assert_eq!(store.load().unwrap(), None);
    }
}
