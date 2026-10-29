//! Where preferences, settings, layouts, themes, palette history and the demo account are
//! kept: files under the data directory on the desktop, `localStorage` entries keyed by path
//! in the browser. Every saved document is UTF-8 text (JSON or TOML).
//!
//! [`export`], [`import`] and [`reveal`] are for files the user names: on the desktop they are
//! ordinary files and folders; in the browser an export is a download, and there is no import
//! or folder yet.

use std::io;
use std::path::{Path, PathBuf};

pub use imp::*;

/// Saved documents keyed by path, as the browser keeps them.
#[cfg(any(target_arch = "wasm32", test))]
trait KeyStore {
    fn keys(&self) -> Vec<String>;
    fn get(&self, key: &str) -> Option<String>;
    /// False when the store refuses the write (full or blocked).
    fn set(&mut self, key: &str, value: &str) -> bool;
    fn remove(&mut self, key: &str);
}

/// Copies every key under `old/` to the same path under `new/`, once. Returns whether to use
/// `new`. Keys already under `new/` are never overwritten, and the old keys stay as they are. A
/// marker key records that the copy finished, so a file deleted afterwards doesn't come back on
/// the next load. If the store refuses a write, the copies made so far are removed and this
/// returns false, so the page keeps its complete old set and the next load tries again.
#[cfg(any(target_arch = "wasm32", test))]
fn copy_keys(store: &mut impl KeyStore, old: &str, new: &str) -> bool {
    let marker = marker(new);
    if store.get(&marker).is_some() {
        return true;
    }
    let keys: Vec<(String, String)> = store
        .keys()
        .into_iter()
        .filter_map(|k| Some((format!("{new}/{}", k.strip_prefix(old)?.strip_prefix('/')?), k)))
        .collect();
    if keys.is_empty() {
        return true;
    }
    let mut copied = Vec::new();
    let mut ok = true;
    for (to, from) in keys {
        if store.get(&to).is_some() {
            continue;
        }
        let Some(value) = store.get(&from) else { continue };
        if !store.set(&to, &value) {
            ok = false;
            break;
        }
        copied.push(to);
    }
    if ok && store.set(&marker, old) {
        tracing::info!("copied {} saved files from {old}/ to {new}/", copied.len());
        return true;
    }
    for k in &copied {
        store.remove(k);
    }
    tracing::warn!("browser storage refused copying saved files from {old}/ to {new}/; using {old}/ for now");
    false
}

/// The key [`copy_keys`] sets once everything under `new/` is in place.
#[cfg(any(target_arch = "wasm32", test))]
fn marker(new: &str) -> String {
    format!("{new}/.migrated")
}

/// Where a page that kept the old prefix (because [`copy_keys`] failed) should read and write
/// `key`. Once another tab has finished the copy, keys under `old` map to `new`, so this page's
/// saves land where later loads look instead of under keys they no longer read.
#[cfg(any(target_arch = "wasm32", test))]
fn redirect(store: &impl KeyStore, old: &str, new: &str, key: String) -> String {
    match key.strip_prefix(old) {
        Some(rest) if (rest.is_empty() || rest.starts_with('/')) && store.get(&marker(new)).is_some() => {
            format!("{new}{rest}")
        }
        _ => key,
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

    /// Moves the data folder from `old` (its name before the app was renamed) to `new` the first
    /// time, and leaves a link at the old path so builds that still use the old name find the
    /// same files. Returns the folder to use: `new`, or `old` if it couldn't be moved. Never
    /// replaces an existing `new`.
    pub fn migrate(old: &Path, new: &Path) -> PathBuf {
        if new.exists() || !old.is_dir() {
            return new.to_path_buf();
        }
        if let Err(e) = std::fs::rename(old, new) {
            // Another copy of the app may have moved it first.
            if new.is_dir() {
                return new.to_path_buf();
            }
            tracing::warn!("could not move {} to {} ({e}); using it where it is", old.display(), new.display());
            return old.to_path_buf();
        }
        tracing::info!("moved {} to {}", old.display(), new.display());
        #[cfg(unix)]
        {
            // Name the sibling rather than repeating `new`, which may be relative (an unset HOME
            // or a relative XDG_DATA_HOME) and would then resolve from the link's folder.
            let target = match new.file_name() {
                Some(name) if old.parent() == new.parent() => PathBuf::from(name),
                _ => std::path::absolute(new).unwrap_or_else(|_| new.to_path_buf()),
            };
            if let Err(e) = std::os::unix::fs::symlink(&target, old) {
                tracing::warn!("linking {} to {}: {e}", old.display(), target.display());
            }
        }
        new.to_path_buf()
    }

    pub fn read(path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// Write via a temp file and rename, so a crash never leaves a half-written file.
    pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path)
    }

    /// Entries directly inside `dir` (none if it doesn't exist).
    pub fn list(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect()
    }

    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    pub fn remove(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    /// Write a file the user exports. Returns where it went, for the status line.
    pub fn export(path: &Path, bytes: &[u8]) -> io::Result<String> {
        write_atomic(path, bytes)?;
        Ok(path.display().to_string())
    }

    /// Read a file the user imports.
    pub fn import(path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// Open a folder in the system file manager, creating it first.
    pub fn reveal(dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(opener).arg(dir).spawn().map(|_| ())
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;

    fn store() -> io::Result<web_sys::Storage> {
        web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "browser storage is unavailable"))
    }

    thread_local! {
        /// The old and new prefixes while this page keeps the old one (see [`migrate`]).
        static FALLBACK: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
    }

    fn key(path: &Path) -> String {
        let key = path.to_string_lossy().into_owned();
        let Some((old, new)) = FALLBACK.with_borrow(Clone::clone) else { return key };
        match store() {
            Ok(s) => redirect(&s, &old, &new, key),
            Err(_) => key,
        }
    }

    impl KeyStore for web_sys::Storage {
        fn keys(&self) -> Vec<String> {
            (0..self.length().unwrap_or(0)).filter_map(|i| self.key(i).ok().flatten()).collect()
        }
        fn get(&self, key: &str) -> Option<String> {
            self.get_item(key).ok().flatten()
        }
        fn set(&mut self, key: &str, value: &str) -> bool {
            self.set_item(key, value).is_ok()
        }
        fn remove(&mut self, key: &str) {
            let _ = self.remove_item(key);
        }
    }

    /// Copies what the page saved under `old/` (its prefix before the app was renamed) to
    /// `new/`, once (see [`copy_keys`]). Returns the prefix to use. If the copy fails, the page
    /// keeps `old`, and its keys follow [`redirect`] from then on, in case another tab finishes
    /// the copy while this one is open.
    pub fn migrate(old: &Path, new: &Path) -> PathBuf {
        let Ok(mut s) = store() else { return new.to_path_buf() };
        let (old, new) = (key(old), key(new));
        if copy_keys(&mut s, &old, &new) {
            return PathBuf::from(new);
        }
        FALLBACK.with_borrow_mut(|f| *f = Some((old.clone(), new)));
        PathBuf::from(old)
    }

    pub fn read(path: &Path) -> io::Result<Vec<u8>> {
        match store()?.get_item(&key(path)) {
            Ok(Some(text)) => Ok(text.into_bytes()),
            Ok(None) => Err(io::ErrorKind::NotFound.into()),
            Err(_) => Err(io::Error::other("browser storage refused the read")),
        }
    }

    /// A single `setItem`, which is already all-or-nothing.
    pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let text = std::str::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        store()?.set_item(&key(path), text).map_err(|_| io::Error::other("browser storage is full or blocked"))
    }

    /// Keys directly under `dir/`.
    pub fn list(dir: &Path) -> Vec<PathBuf> {
        let Ok(s) = store() else { return Vec::new() };
        let prefix = format!("{}/", key(dir).trim_end_matches('/'));
        (0..s.length().unwrap_or(0))
            .filter_map(|i| s.key(i).ok().flatten())
            .filter(|k| k.strip_prefix(&prefix).is_some_and(|rest| !rest.is_empty() && !rest.contains('/')))
            .map(PathBuf::from)
            .collect()
    }

    pub fn exists(path: &Path) -> bool {
        store().ok().and_then(|s| s.get_item(&key(path)).ok().flatten()).is_some()
    }

    pub fn remove(path: &Path) -> io::Result<()> {
        if !exists(path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        store()?.remove_item(&key(path)).map_err(|_| io::Error::other("browser storage refused the delete"))
    }

    pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
        write_atomic(to, &read(from)?)?;
        remove(from)
    }

    /// Download the exported file under its file name. Returns where it went, for the status line.
    pub fn export(path: &Path, bytes: &[u8]) -> io::Result<String> {
        use wasm_bindgen::JsCast;
        let fail = |what: &str| io::Error::other(format!("the browser refused the download ({what})"));
        let name = path.file_name().map_or_else(|| "export".into(), |n| n.to_string_lossy().into_owned());
        let doc = web_sys::window().and_then(|w| w.document()).ok_or_else(|| fail("no document"))?;
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).map_err(|_| fail("blob"))?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|_| fail("object URL"))?;
        let link: web_sys::HtmlAnchorElement =
            doc.create_element("a").map_err(|_| fail("link"))?.dyn_into().map_err(|_| fail("link"))?;
        link.set_href(&url);
        link.set_download(&name);
        link.click();
        let _ = web_sys::Url::revoke_object_url(&url);
        Ok(format!("your downloads as {name}"))
    }

    // TODO(web): read imports through a file picker instead of a typed path.
    pub fn import(_path: &Path) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "importing files needs the desktop app for now"))
    }

    pub fn reveal(_dir: &Path) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the browser keeps these in its local storage, not a folder"))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("tr-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = dir.join("sub/a.json");
        assert!(list(&dir.join("sub")).is_empty());
        assert_eq!(read(&a).unwrap_err().kind(), io::ErrorKind::NotFound);
        write_atomic(&a, b"{}").unwrap();
        assert!(exists(&a));
        assert_eq!(list(&dir.join("sub")), vec![a.clone()]);
        let b = dir.join("sub/b.json");
        rename(&a, &b).unwrap();
        assert!(!exists(&a));
        assert_eq!(read(&b).unwrap(), b"{}");
        remove(&b).unwrap();
        assert!(list(&dir.join("sub")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tr-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migrate_moves_the_old_folder_and_links_it() {
        let root = scratch("move");
        let (old, new) = (root.join("terminal-rs"), root.join("terminal"));
        write_atomic(&old.join("layouts/Desk.json"), b"{}").unwrap();
        assert_eq!(migrate(&old, &new), new);
        assert_eq!(read(&new.join("layouts/Desk.json")).unwrap(), b"{}");
        #[cfg(unix)]
        {
            assert_eq!(std::fs::read_link(&old).unwrap(), Path::new("terminal"), "relative to the link's folder");
            assert_eq!(read(&old.join("layouts/Desk.json")).unwrap(), b"{}", "old builds still find the files");
        }
        // Later launches find the new folder and leave everything as it is.
        write_atomic(&new.join("settings.json"), b"[]").unwrap();
        assert_eq!(migrate(&old, &new), new);
        assert_eq!(read(&new.join("settings.json")).unwrap(), b"[]");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migrate_never_replaces_an_existing_folder() {
        let root = scratch("keep");
        let (old, new) = (root.join("terminal-rs"), root.join("terminal"));
        write_atomic(&old.join("settings.json"), b"old").unwrap();
        write_atomic(&new.join("settings.json"), b"new").unwrap();
        assert_eq!(migrate(&old, &new), new);
        assert_eq!(read(&new.join("settings.json")).unwrap(), b"new");
        assert_eq!(read(&old.join("settings.json")).unwrap(), b"old", "the old folder is left alone");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migrate_without_an_old_folder_creates_nothing() {
        let root = scratch("fresh");
        let (old, new) = (root.join("terminal-rs"), root.join("terminal"));
        assert_eq!(migrate(&old, &new), new);
        assert!(!old.exists() && !new.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn migrate_keeps_using_the_old_folder_when_it_cannot_move() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("stuck");
        let (old, new) = (root.join("terminal-rs"), root.join("terminal"));
        write_atomic(&old.join("settings.json"), b"{}").unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::create_dir(root.join("probe")).is_ok() {
            // Running as root, where a read-only folder doesn't stop the move.
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(&root).unwrap();
            return;
        }
        let used = migrate(&old, &new);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(used, old);
        assert_eq!(read(&old.join("settings.json")).unwrap(), b"{}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `localStorage` stand-in: a map that refuses writes once it holds `room` keys.
    struct Keys {
        map: std::collections::BTreeMap<String, String>,
        room: usize,
    }

    impl Keys {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self { map: entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), room: usize::MAX }
        }
    }

    impl KeyStore for Keys {
        fn keys(&self) -> Vec<String> {
            self.map.keys().cloned().collect()
        }
        fn get(&self, key: &str) -> Option<String> {
            self.map.get(key).cloned()
        }
        fn set(&mut self, key: &str, value: &str) -> bool {
            if !self.map.contains_key(key) && self.map.len() >= self.room {
                return false;
            }
            self.map.insert(key.into(), value.into());
            true
        }
        fn remove(&mut self, key: &str) {
            self.map.remove(key);
        }
    }

    #[test]
    fn copy_keys_copies_once_without_overwriting() {
        let mut s = Keys::new(&[
            ("terminal-rs/settings.json", "old settings"),
            ("terminal-rs/layouts/Desk.json", "desk"),
            ("terminal/settings.json", "newer settings"),
            ("other-site-key", "untouched"),
        ]);
        assert!(copy_keys(&mut s, "terminal-rs", "terminal"));
        assert_eq!(s.get("terminal/settings.json").as_deref(), Some("newer settings"), "newer data wins");
        assert_eq!(s.get("terminal/layouts/Desk.json").as_deref(), Some("desk"));
        assert_eq!(s.get("terminal-rs/layouts/Desk.json").as_deref(), Some("desk"), "the old keys stay");
        assert_eq!(s.get("other-site-key").as_deref(), Some("untouched"));
        // A layout deleted after the copy doesn't come back on the next load.
        s.remove("terminal/layouts/Desk.json");
        assert!(copy_keys(&mut s, "terminal-rs", "terminal"));
        assert_eq!(s.get("terminal/layouts/Desk.json"), None);
    }

    #[test]
    fn copy_keys_undoes_a_partial_copy() {
        let mut s = Keys::new(&[("terminal-rs/a.json", "a"), ("terminal-rs/b.json", "b")]);
        s.room = 3;
        assert!(!copy_keys(&mut s, "terminal-rs", "terminal"), "storage is full: keep using the old keys");
        assert_eq!(s.keys(), ["terminal-rs/a.json", "terminal-rs/b.json"]);
        s.room = usize::MAX;
        assert!(copy_keys(&mut s, "terminal-rs", "terminal"));
        assert_eq!(s.get("terminal/b.json").as_deref(), Some("b"));
    }

    #[test]
    fn a_page_left_on_the_old_prefix_follows_another_tabs_copy() {
        let mut s = Keys::new(&[("terminal-rs/settings.json", "a")]);
        s.room = 1;
        // Tab A can't copy, so it keeps the old prefix.
        assert!(!copy_keys(&mut s, "terminal-rs", "terminal"));
        let key = |s: &Keys, k: &str| redirect(s, "terminal-rs", "terminal", k.into());
        assert_eq!(key(&s, "terminal-rs/settings.json"), "terminal-rs/settings.json");
        // Space frees up and tab B finishes the copy; tab A's saves now go where loads read.
        s.room = usize::MAX;
        assert!(copy_keys(&mut s, "terminal-rs", "terminal"));
        assert_eq!(key(&s, "terminal-rs/settings.json"), "terminal/settings.json");
        assert_eq!(key(&s, "terminal-rs/layouts"), "terminal/layouts");
        assert_eq!(key(&s, "terminal-rs"), "terminal");
        assert_eq!(key(&s, "terminal-rsx/a.json"), "terminal-rsx/a.json", "only that prefix");
    }

    #[test]
    fn copy_keys_with_nothing_saved_writes_nothing() {
        let mut s = Keys::new(&[]);
        assert!(copy_keys(&mut s, "terminal-rs", "terminal"));
        assert!(s.keys().is_empty());
    }
}
