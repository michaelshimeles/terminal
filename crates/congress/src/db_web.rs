//! The browser build's stand-in for the SQLite store: always empty, because the browser
//! can't fetch disclosures yet. It keeps the desktop store's method signatures so callers
//! compile unchanged.
//!
//! TODO(web proxy): fill this from a same-origin proxy for Senate eFD and the House Clerk.

use std::path::Path;

use anyhow::{Result, bail};

pub use crate::model::{Alert, PoliticianSummary};
use crate::model::{Politician, Trade};

pub struct Db;

impl Db {
    pub fn open(_path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self)
    }

    pub fn open_in_memory() -> Result<Self> {
        Ok(Self)
    }

    pub fn latest_trades(&self, _limit: usize) -> Result<Vec<Trade>> {
        Ok(Vec::new())
    }

    pub fn trades_for_ticker(&self, _ticker: &str, _limit: usize) -> Result<Vec<Trade>> {
        Ok(Vec::new())
    }

    pub fn trades_for_politician(&self, _politician_id: i64) -> Result<Vec<Trade>> {
        Ok(Vec::new())
    }

    pub fn politician(&self, _id: i64) -> Result<Option<Politician>> {
        Ok(None)
    }

    pub fn search_politicians(&self, _query: &str, _limit: usize) -> Result<Vec<PoliticianSummary>> {
        Ok(Vec::new())
    }

    pub fn set_tracked(&self, _politician_id: i64, _tracked: bool) -> Result<()> {
        bail!("tracking needs congressional data, which the browser build can't load yet")
    }

    pub fn is_tracked(&self, _politician_id: i64) -> Result<bool> {
        Ok(false)
    }

    pub fn alerts(&self, _unseen_only: bool, _limit: usize) -> Result<Vec<Alert>> {
        Ok(Vec::new())
    }

    pub fn mark_alerts_seen(&self) -> Result<()> {
        Ok(())
    }
}
