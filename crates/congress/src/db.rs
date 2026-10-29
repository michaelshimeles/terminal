//! SQLite storage for filings, normalised trades, tracked politicians and alerts.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

pub use crate::model::{Alert, PoliticianSummary};
use crate::model::{AmountRange, Chamber, ParsedTrade, Party, Politician, Trade, TxType};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS politicians (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    chamber   TEXT NOT NULL,
    party     TEXT NOT NULL DEFAULT '?',
    state     TEXT,
    district  TEXT,
    bioguide  TEXT,
    UNIQUE (chamber, name)
);
CREATE TABLE IF NOT EXISTS filings (
    id            TEXT PRIMARY KEY,          -- 'house:<DocID>' | 'senate:<uuid>'
    chamber       TEXT NOT NULL,
    politician_id INTEGER NOT NULL REFERENCES politicians(id),
    filed_date    TEXT NOT NULL,
    url           TEXT NOT NULL,
    kind          TEXT NOT NULL,             -- 'electronic' | 'paper'
    status        TEXT NOT NULL,             -- 'parsed' | 'scanned' | 'error'
    error         TEXT,
    is_amendment  INTEGER NOT NULL DEFAULT 0,
    fetched_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS trades (
    id                INTEGER PRIMARY KEY,
    filing_id         TEXT NOT NULL REFERENCES filings(id) ON DELETE CASCADE,
    row_idx           INTEGER NOT NULL,
    politician_id     INTEGER NOT NULL REFERENCES politicians(id),
    ticker            TEXT,
    asset_name        TEXT NOT NULL,
    asset_type        TEXT,
    owner             TEXT,
    tx_type           TEXT NOT NULL,
    amount_min        INTEGER,
    amount_max        INTEGER,
    tx_date           TEXT NOT NULL,
    notification_date TEXT,
    filed_date        TEXT NOT NULL,
    comment           TEXT,
    UNIQUE (filing_id, row_idx)
);
CREATE INDEX IF NOT EXISTS trades_ticker ON trades (ticker, tx_date);
CREATE INDEX IF NOT EXISTS trades_politician ON trades (politician_id, tx_date);
CREATE INDEX IF NOT EXISTS trades_filed ON trades (filed_date DESC, id DESC);
CREATE TABLE IF NOT EXISTS tracked (
    politician_id INTEGER PRIMARY KEY REFERENCES politicians(id)
);
CREATE TABLE IF NOT EXISTS alerts (
    id            INTEGER PRIMARY KEY,
    politician_id INTEGER NOT NULL REFERENCES politicians(id),
    filing_id     TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    message       TEXT NOT NULL,
    seen          INTEGER NOT NULL DEFAULT 0,
    UNIQUE (filing_id)
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct NewPolitician {
    pub name: String,
    pub chamber: Chamber,
    pub party: Party,
    pub state: Option<String>,
    pub district: Option<String>,
    pub bioguide: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilingStatus {
    Parsed,
    Scanned,
    Error,
}

impl FilingStatus {
    fn as_str(self) -> &'static str {
        match self {
            FilingStatus::Parsed => "parsed",
            FilingStatus::Scanned => "scanned",
            FilingStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilingRecord {
    pub id: String,
    pub chamber: Chamber,
    pub politician_id: i64,
    pub filed_date: NaiveDate,
    pub url: String,
    pub paper: bool,
    pub status: FilingStatus,
    pub error: Option<String>,
    pub is_amendment: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub politicians: i64,
    pub filings: i64,
    pub trades: i64,
    pub scanned: i64,
}

pub struct Db {
    conn: Connection,
}

const TRADE_SELECT: &str = "SELECT t.id, t.filing_id, t.politician_id, p.name, p.party, p.chamber, p.state,
        t.ticker, t.asset_name, t.asset_type, t.owner, t.tx_type, t.amount_min, t.amount_max,
        t.tx_date, t.filed_date, f.url
    FROM trades t JOIN politicians p ON p.id = t.politician_id JOIN filings f ON f.id = t.filing_id";

fn date(s: String) -> NaiveDate {
    NaiveDate::parse_from_str(&s, "%Y-%m-%d").unwrap_or_default()
}

fn trade_from_row(r: &Row<'_>) -> rusqlite::Result<Trade> {
    let min: Option<i64> = r.get(12)?;
    let max: Option<i64> = r.get(13)?;
    Ok(Trade {
        id: r.get(0)?,
        filing_id: r.get(1)?,
        politician_id: r.get(2)?,
        politician: r.get(3)?,
        party: Party::from_code(&r.get::<_, String>(4)?),
        chamber: r.get::<_, String>(5)?.parse().unwrap_or(Chamber::House),
        state: r.get(6)?,
        ticker: r.get(7)?,
        asset_name: r.get(8)?,
        asset_type: r.get(9)?,
        owner: r.get(10)?,
        tx_type: TxType::parse(&r.get::<_, String>(11)?),
        amount: min.map(|m| AmountRange { min: m as u64, max: max.map(|v| v as u64) }),
        tx_date: date(r.get(14)?),
        filed_date: date(r.get(15)?),
        source_url: r.get(16)?,
    })
}

fn politician_from_row(r: &Row<'_>) -> rusqlite::Result<Politician> {
    Ok(Politician {
        id: r.get(0)?,
        name: r.get(1)?,
        chamber: r.get::<_, String>(2)?.parse().unwrap_or(Chamber::House),
        party: Party::from_code(&r.get::<_, String>(3)?),
        state: r.get(4)?,
        district: r.get(5)?,
        bioguide: r.get(6)?,
    })
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Insert or update a politician, keyed by bioguide ID when known and otherwise by
    /// (chamber, name); returns its id. Known values are never overwritten with unknowns.
    pub fn upsert_politician(&self, p: &NewPolitician) -> Result<i64> {
        if let Some(bio) = &p.bioguide {
            let existing: Option<i64> = self
                .conn
                .query_row("SELECT id FROM politicians WHERE bioguide = ?1", [bio], |r| r.get(0))
                .optional()?;
            if let Some(id) = existing {
                self.conn.execute(
                    "UPDATE OR IGNORE politicians SET name = ?2, chamber = ?3,
                       party = CASE WHEN ?4 <> '?' THEN ?4 ELSE party END,
                       state = COALESCE(?5, state), district = COALESCE(?6, district)
                     WHERE id = ?1",
                    params![id, p.name, p.chamber.as_str(), p.party.code(), p.state, p.district],
                )?;
                return Ok(id);
            }
        }
        self.conn.execute(
            "INSERT INTO politicians (name, chamber, party, state, district, bioguide)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (chamber, name) DO UPDATE SET
               party    = CASE WHEN excluded.party <> '?' THEN excluded.party ELSE party END,
               state    = COALESCE(excluded.state, state),
               district = COALESCE(excluded.district, district),
               bioguide = COALESCE(excluded.bioguide, bioguide)",
            params![p.name, p.chamber.as_str(), p.party.code(), p.state, p.district, p.bioguide],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM politicians WHERE chamber = ?1 AND name = ?2",
            params![p.chamber.as_str(), p.name],
            |r| r.get(0),
        )?)
    }

    /// Apply canonical roster identity to stored politicians: fix names/party for known
    /// bioguide IDs and merge rows that share a bioguide ID (from earlier name typos).
    /// Returns the number of rows merged away.
    pub fn apply_roster(&mut self, members: &[(String, String, Party, Option<String>)]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut merged = 0;
        for (bio, name, party, state) in members {
            let ids: Vec<i64> = {
                let mut st = tx.prepare_cached("SELECT id FROM politicians WHERE bioguide = ?1 ORDER BY id")?;
                st.query_map([bio], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            let Some((&keep, dups)) = ids.split_first() else { continue };
            for &d in dups {
                tx.execute("UPDATE trades SET politician_id = ?1 WHERE politician_id = ?2", params![keep, d])?;
                tx.execute("UPDATE filings SET politician_id = ?1 WHERE politician_id = ?2", params![keep, d])?;
                tx.execute(
                    "UPDATE OR IGNORE tracked SET politician_id = ?1 WHERE politician_id = ?2",
                    params![keep, d],
                )?;
                tx.execute("DELETE FROM tracked WHERE politician_id = ?1", [d])?;
                tx.execute("UPDATE alerts SET politician_id = ?1 WHERE politician_id = ?2", params![keep, d])?;
                tx.execute("DELETE FROM politicians WHERE id = ?1", [d])?;
                merged += 1;
            }
            tx.execute(
                "UPDATE OR IGNORE politicians SET name = ?2,
                   party = CASE WHEN ?3 <> '?' THEN ?3 ELSE party END, state = COALESCE(?4, state)
                 WHERE id = ?1",
                params![keep, name, party.code(), state],
            )?;
        }
        tx.commit()?;
        Ok(merged)
    }

    pub fn known_filing_ids(&self, chamber: Chamber) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM filings WHERE chamber = ?1 AND status <> 'error'")?;
        let ids = st.query_map([chamber.as_str()], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Store a filing and replace its trades atomically. Returns trades written.
    pub fn insert_filing(&mut self, f: &FilingRecord, trades: &[ParsedTrade]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO filings (id, chamber, politician_id, filed_date, url, kind, status, error, is_amendment, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (id) DO UPDATE SET status = excluded.status, error = excluded.error,
               filed_date = excluded.filed_date, fetched_at = excluded.fetched_at",
            params![
                f.id,
                f.chamber.as_str(),
                f.politician_id,
                f.filed_date.to_string(),
                f.url,
                if f.paper { "paper" } else { "electronic" },
                f.status.as_str(),
                f.error,
                f.is_amendment,
                Utc::now().timestamp(),
            ],
        )?;
        tx.execute("DELETE FROM trades WHERE filing_id = ?1", [&f.id])?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO trades (filing_id, row_idx, politician_id, ticker, asset_name, asset_type, owner,
                   tx_type, amount_min, amount_max, tx_date, notification_date, filed_date, comment)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT (filing_id, row_idx) DO NOTHING",
            )?;
            for t in trades {
                st.execute(params![
                    f.id,
                    t.row,
                    f.politician_id,
                    t.ticker,
                    t.asset_name,
                    t.asset_type,
                    t.owner,
                    t.tx_type.code(),
                    t.amount.map(|a| a.min as i64),
                    t.amount.and_then(|a| a.max.map(|m| m as i64)),
                    t.tx_date.to_string(),
                    t.notification_date.map(|d| d.to_string()),
                    f.filed_date.to_string(),
                    t.comment,
                ])?;
            }
        }
        tx.commit()?;
        Ok(trades.len())
    }

    fn query_trades(&self, where_sql: &str, params: impl rusqlite::Params) -> Result<Vec<Trade>> {
        let sql = format!("{TRADE_SELECT} {where_sql}");
        let mut st = self.conn.prepare_cached(&sql)?;
        let rows = st.query_map(params, trade_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Most recently disclosed trades first.
    pub fn latest_trades(&self, limit: usize) -> Result<Vec<Trade>> {
        self.query_trades("ORDER BY t.filed_date DESC, t.id DESC LIMIT ?1", [limit as i64])
    }

    /// Everyone in Congress who traded `ticker`, newest transaction first.
    pub fn trades_for_ticker(&self, ticker: &str, limit: usize) -> Result<Vec<Trade>> {
        self.query_trades(
            "WHERE t.ticker = ?1 ORDER BY t.tx_date DESC, t.id DESC LIMIT ?2",
            params![ticker.to_ascii_uppercase(), limit as i64],
        )
    }

    pub fn trades_for_politician(&self, politician_id: i64) -> Result<Vec<Trade>> {
        self.query_trades("WHERE t.politician_id = ?1 ORDER BY t.tx_date DESC, t.id DESC", [politician_id])
    }

    pub fn politician(&self, id: i64) -> Result<Option<Politician>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, chamber, party, state, district, bioguide FROM politicians WHERE id = ?1",
                [id],
                politician_from_row,
            )
            .optional()?)
    }

    /// Politicians matching `query` (case-insensitive substring; empty = all), most active first.
    pub fn search_politicians(&self, query: &str, limit: usize) -> Result<Vec<PoliticianSummary>> {
        let mut st = self.conn.prepare_cached(
            "SELECT p.id, p.name, p.chamber, p.party, p.state, p.district, p.bioguide,
                    COUNT(t.id), MAX(t.filed_date), EXISTS (SELECT 1 FROM tracked k WHERE k.politician_id = p.id)
             FROM politicians p LEFT JOIN trades t ON t.politician_id = p.id
             WHERE p.name LIKE '%' || ?1 || '%'
             GROUP BY p.id ORDER BY COUNT(t.id) DESC, p.name LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![query.trim(), limit as i64], |r| {
                Ok(PoliticianSummary {
                    politician: politician_from_row(r)?,
                    trades: r.get(7)?,
                    last_filed: r.get::<_, Option<String>>(8)?.map(date),
                    tracked: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn set_tracked(&self, politician_id: i64, tracked: bool) -> Result<()> {
        if tracked {
            self.conn.execute("INSERT OR IGNORE INTO tracked (politician_id) VALUES (?1)", [politician_id])?;
        } else {
            self.conn.execute("DELETE FROM tracked WHERE politician_id = ?1", [politician_id])?;
        }
        Ok(())
    }

    pub fn is_tracked(&self, politician_id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM tracked WHERE politician_id = ?1", [politician_id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn tracked_ids(&self) -> Result<HashSet<i64>> {
        let mut st = self.conn.prepare_cached("SELECT politician_id FROM tracked")?;
        let ids = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Record an alert once per filing; returns `None` if it already existed.
    pub fn insert_alert(&self, politician_id: i64, filing_id: &str, message: &str) -> Result<Option<i64>> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO alerts (politician_id, filing_id, created_at, message) VALUES (?1, ?2, ?3, ?4)",
            params![politician_id, filing_id, Utc::now().timestamp(), message],
        )?;
        Ok((n > 0).then(|| self.conn.last_insert_rowid()))
    }

    pub fn alerts(&self, unseen_only: bool, limit: usize) -> Result<Vec<Alert>> {
        let mut st = self.conn.prepare_cached(
            "SELECT a.id, a.politician_id, p.name, a.filing_id, a.created_at, a.message, a.seen
             FROM alerts a JOIN politicians p ON p.id = a.politician_id
             WHERE (?1 = 0 OR a.seen = 0) ORDER BY a.id DESC LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![unseen_only, limit as i64], |r| {
                Ok(Alert {
                    id: r.get(0)?,
                    politician_id: r.get(1)?,
                    politician: r.get(2)?,
                    filing_id: r.get(3)?,
                    created_at: r.get(4)?,
                    message: r.get(5)?,
                    seen: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn mark_alerts_seen(&self) -> Result<()> {
        self.conn.execute("UPDATE alerts SET seen = 1 WHERE seen = 0", [])?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        Ok(self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM politicians), (SELECT COUNT(*) FROM filings),
                    (SELECT COUNT(*) FROM trades), (SELECT COUNT(*) FROM filings WHERE status = 'scanned')",
            [],
            |r| Ok(Stats { politicians: r.get(0)?, filings: r.get(1)?, trades: r.get(2)?, scanned: r.get(3)? }),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn trade(row: u32, ticker: &str, tx: &str) -> ParsedTrade {
        ParsedTrade {
            row,
            owner: Some("Self".into()),
            ticker: Some(ticker.into()),
            asset_name: format!("{ticker} Inc"),
            asset_type: Some("ST".into()),
            tx_type: TxType::Purchase,
            tx_date: d(tx),
            notification_date: None,
            amount: AmountRange::parse("$1,001 - $15,000"),
            comment: None,
        }
    }

    #[test]
    fn roundtrip_filings_trades_alerts() {
        let mut db = Db::open_in_memory().unwrap();
        let pid = db
            .upsert_politician(&NewPolitician {
                name: "Jane Doe".into(),
                chamber: Chamber::House,
                party: Party::Unknown,
                state: Some("MO".into()),
                district: Some("MO04".into()),
                bioguide: None,
            })
            .unwrap();
        // A later upsert with a known party fills it in without changing the id.
        let pid2 = db
            .upsert_politician(&NewPolitician {
                name: "Jane Doe".into(),
                chamber: Chamber::House,
                party: Party::Republican,
                state: None,
                district: None,
                bioguide: Some("D000001".into()),
            })
            .unwrap();
        assert_eq!(pid, pid2);
        // Same bioguide under a differently spelled name (index typo) maps to the same row.
        let pid3 = db
            .upsert_politician(&NewPolitician {
                name: "Jane Q Mrs Doe".into(),
                chamber: Chamber::House,
                party: Party::Unknown,
                state: None,
                district: None,
                bioguide: Some("D000001".into()),
            })
            .unwrap();
        assert_eq!(pid3, pid);
        assert_eq!(db.politician(pid).unwrap().unwrap().party, Party::Republican);
        db.upsert_politician(&NewPolitician {
            name: "Jane Doe".into(),
            chamber: Chamber::House,
            party: Party::Republican,
            state: None,
            district: None,
            bioguide: Some("D000001".into()),
        })
        .unwrap();
        let f = FilingRecord {
            id: "house:1".into(),
            chamber: Chamber::House,
            politician_id: pid,
            filed_date: d("2026-03-20"),
            url: "u".into(),
            paper: false,
            status: FilingStatus::Parsed,
            error: None,
            is_amendment: false,
        };
        assert_eq!(db.insert_filing(&f, &[trade(1, "AAPL", "2026-03-01"), trade(2, "MSFT", "2026-03-02")]).unwrap(), 2);
        // Re-inserting replaces rather than duplicates.
        db.insert_filing(&f, &[trade(1, "AAPL", "2026-03-01"), trade(2, "MSFT", "2026-03-02")]).unwrap();
        assert_eq!(db.stats().unwrap().trades, 2);
        let latest = db.latest_trades(10).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].party, Party::Republican);
        assert_eq!(latest[0].disclosure_lag_days(), 18);
        let aapl = db.trades_for_ticker("aapl", 10).unwrap();
        assert_eq!(aapl.len(), 1);
        assert_eq!(aapl[0].politician, "Jane Doe");
        assert!(db.known_filing_ids(Chamber::House).unwrap().contains("house:1"));

        db.set_tracked(pid, true).unwrap();
        assert!(db.is_tracked(pid).unwrap());
        assert!(db.insert_alert(pid, "house:1", "new trade").unwrap().is_some());
        assert!(db.insert_alert(pid, "house:1", "dup").unwrap().is_none());
        assert_eq!(db.alerts(true, 10).unwrap().len(), 1);
        db.mark_alerts_seen().unwrap();
        assert!(db.alerts(true, 10).unwrap().is_empty());
        // Roster sync merges a stray duplicate created before bioguide was known.
        let dup = db
            .upsert_politician(&NewPolitician {
                name: "Doe Jane (typo)".into(),
                chamber: Chamber::House,
                party: Party::Unknown,
                state: None,
                district: None,
                bioguide: None,
            })
            .unwrap();
        db.conn().execute("UPDATE politicians SET bioguide = 'D000001' WHERE id = ?1", [dup]).unwrap();
        assert_eq!(
            db.apply_roster(&[("D000001".into(), "Jane Doe".into(), Party::Republican, Some("MO".into()))]).unwrap(),
            1
        );
        assert!(db.politician(dup).unwrap().is_none());
        let s = db.search_politicians("doe", 10).unwrap();
        assert_eq!(s[0].trades, 2);
        assert!(s[0].tracked);
        db.set_meta("k", "v").unwrap();
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("v"));
    }
}
