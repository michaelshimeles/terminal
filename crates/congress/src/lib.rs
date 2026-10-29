//! STOCK Act congressional trade disclosures, ingested directly from the
//! official sources: the Senate eFD system and the House Clerk's PTR filings.
//!
//! The browser build (`wasm32`) has no SQLite and can't reach either site (they don't allow
//! cross-origin requests), so there [`db`] is an empty stand-in and there is no ingest.

#[cfg(not(target_arch = "wasm32"))]
pub mod db;
#[cfg(target_arch = "wasm32")]
#[path = "db_web.rs"]
pub mod db;
#[cfg(not(target_arch = "wasm32"))]
pub mod house;
pub mod model;
pub mod performance;
#[cfg(not(target_arch = "wasm32"))]
pub mod refresh;
#[cfg(not(target_arch = "wasm32"))]
pub mod roster;
#[cfg(not(target_arch = "wasm32"))]
pub mod senate;

pub use db::Db;
pub use model::{AmountRange, Chamber, ParsedTrade, Party, Politician, Trade, TxType};
