//! Where preferences, settings, layouts, themes, palette history and the demo account are
//! kept: files under the data directory on the desktop, `localStorage` entries keyed by path
//! in the browser. Every saved document is UTF-8 text (JSON or TOML).

use std::io;
use std::path::{Path, PathBuf};

pub use imp::*;

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;

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
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;

    fn store() -> io::Result<web_sys::Storage> {
        web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "browser storage is unavailable"))
    }

    fn key(path: &Path) -> String {
        path.to_string_lossy().into_owned()
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
}
