use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Chamber {
    House,
    Senate,
}

impl Chamber {
    pub fn as_str(self) -> &'static str {
        match self {
            Chamber::House => "House",
            Chamber::Senate => "Senate",
        }
    }
}

impl FromStr for Chamber {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "House" => Ok(Chamber::House),
            "Senate" => Ok(Chamber::Senate),
            _ => Err(format!("unknown chamber '{s}'")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Party {
    Democrat,
    Republican,
    Independent,
    #[default]
    Unknown,
}

impl Party {
    pub fn from_code(code: &str) -> Self {
        match code.trim() {
            "D" => Party::Democrat,
            "R" => Party::Republican,
            "I" | "ID" => Party::Independent,
            _ => Party::Unknown,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Party::Democrat => "D",
            Party::Republican => "R",
            Party::Independent => "I",
            Party::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TxType {
    Purchase,
    Sale,
    SalePartial,
    Exchange,
    Other,
}

impl TxType {
    /// Parse both House codes (`P`, `S`, `S (partial)`, `E`) and Senate labels
    /// (`Purchase`, `Sale (Full)`, `Sale (Partial)`, `Exchange`).
    pub fn parse(s: &str) -> Self {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "p" | "purchase" => TxType::Purchase,
            "s" | "sale" | "sale (full)" => TxType::Sale,
            "s (partial)" | "sale (partial)" => TxType::SalePartial,
            "e" | "exchange" => TxType::Exchange,
            _ => TxType::Other,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            TxType::Purchase => "P",
            TxType::Sale => "S",
            TxType::SalePartial => "S (partial)",
            TxType::Exchange => "E",
            TxType::Other => "?",
        }
    }

    pub fn is_buy(self) -> bool {
        self == TxType::Purchase
    }

    pub fn is_sell(self) -> bool {
        matches!(self, TxType::Sale | TxType::SalePartial)
    }

    /// Single-letter chart marker.
    pub fn marker(self) -> char {
        match self {
            TxType::Purchase => 'B',
            TxType::Sale | TxType::SalePartial => 'S',
            TxType::Exchange => 'E',
            TxType::Other => '?',
        }
    }
}

impl FromStr for TxType {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(TxType::parse(s))
    }
}

/// Disclosed value band in whole dollars. `max == None` means open-ended ("Over $X").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AmountRange {
    pub min: u64,
    pub max: Option<u64>,
}

static DOLLARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\s*([0-9][0-9,]*)").unwrap());

impl AmountRange {
    /// Parse `$1,001 - $15,000`, `Over $50,000,000`, `Spouse/DC Over $1,000,000`,
    /// and line-wrapped forms such as `$15,001 -\n$50,000`.
    pub fn parse(s: &str) -> Option<Self> {
        let nums: Vec<u64> = DOLLARS.captures_iter(s).filter_map(|c| c[1].replace(',', "").parse().ok()).collect();
        let over = s.to_ascii_lowercase().contains("over");
        match (nums.as_slice(), over) {
            ([n], true) => Some(Self { min: n + 1, max: None }),
            ([a, b], _) if a <= b => Some(Self { min: *a, max: Some(*b) }),
            ([n], false) => Some(Self { min: *n, max: Some(*n) }),
            _ => None,
        }
    }

    /// Midpoint used for weighting; open-ended bands use 1.5x their floor.
    pub fn midpoint(&self) -> f64 {
        match self.max {
            Some(m) => (self.min as f64 + m as f64) / 2.0,
            None => self.min as f64 * 1.5,
        }
    }
}

impl fmt::Display for AmountRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn short(v: u64) -> String {
            match v {
                v if v >= 1_000_000 && v % 1_000_000 <= 1 => format!("{}M", v / 1_000_000),
                v if v >= 1_000 => format!("{}K", (v + 1) / 1_000),
                v => v.to_string(),
            }
        }
        match self.max {
            Some(m) => write!(f, "${}-${}", short(self.min), short(m)),
            None => write!(f, ">${}", short(self.min.saturating_sub(1))),
        }
    }
}

/// One disclosed transaction as parsed from a filing, before politician
/// identity is resolved against the roster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedTrade {
    pub row: u32,
    pub owner: Option<String>,
    pub ticker: Option<String>,
    pub asset_name: String,
    pub asset_type: Option<String>,
    pub tx_type: TxType,
    pub tx_date: NaiveDate,
    pub notification_date: Option<NaiveDate>,
    pub amount: Option<AmountRange>,
    pub comment: Option<String>,
}

/// A fully normalised trade row as stored and displayed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub id: i64,
    pub filing_id: String,
    pub politician_id: i64,
    pub politician: String,
    pub party: Party,
    pub chamber: Chamber,
    pub state: Option<String>,
    pub ticker: Option<String>,
    pub asset_name: String,
    pub asset_type: Option<String>,
    pub owner: Option<String>,
    pub tx_type: TxType,
    pub amount: Option<AmountRange>,
    pub tx_date: NaiveDate,
    pub filed_date: NaiveDate,
    pub source_url: String,
}

impl Trade {
    /// Days between the transaction and its public disclosure.
    pub fn disclosure_lag_days(&self) -> i64 {
        (self.filed_date - self.tx_date).num_days()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Politician {
    pub id: i64,
    pub name: String,
    pub chamber: Chamber,
    pub party: Party,
    pub state: Option<String>,
    pub district: Option<String>,
    pub bioguide: Option<String>,
}

/// Parse US-style `MM/DD/YYYY` (also accepts 2-digit years and surrounding text).
pub fn parse_us_date(s: &str) -> Option<NaiveDate> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d{1,2})/(\d{1,2})/(\d{2,4})").unwrap());
    let c = RE.captures(s)?;
    let (m, d): (u32, u32) = (c[1].parse().ok()?, c[2].parse().ok()?);
    let mut y: i32 = c[3].parse().ok()?;
    if y < 100 {
        y += 2000;
    }
    NaiveDate::from_ymd_opt(y, m, d)
}

/// Normalise a ticker: uppercase, `BRK/B` and `BRK B` become `BRK.B`, and
/// anything that does not look like an exchange ticker (CUSIPs, `--`) is rejected.
pub fn normalize_ticker(raw: &str) -> Option<String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z]{1,5}(?:\.[A-Z]{1,2})?$").unwrap());
    let t = raw.trim().to_ascii_uppercase().replace(['/', ' '], ".");
    RE.is_match(&t).then_some(t)
}

/// An alert raised when a tracked politician files new trades.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub id: i64,
    pub politician_id: i64,
    pub politician: String,
    pub filing_id: String,
    pub created_at: i64,
    pub message: String,
    pub seen: bool,
}

/// A politician with aggregate trade counts, for list views.
#[derive(Debug, Clone, PartialEq)]
pub struct PoliticianSummary {
    pub politician: Politician,
    pub trades: i64,
    pub last_filed: Option<NaiveDate>,
    pub tracked: bool,
}

/// A new filing by a tracked politician, found during a refresh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewTradeAlert {
    pub politician_id: i64,
    pub politician: String,
    pub filing_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(AmountRange::parse("$1,001 - $15,000"), Some(AmountRange { min: 1001, max: Some(15000) }));
        assert_eq!(AmountRange::parse("$15,001 -\n$50,000"), Some(AmountRange { min: 15001, max: Some(50000) }));
        assert_eq!(AmountRange::parse("Over $50,000,000"), Some(AmountRange { min: 50_000_001, max: None }));
        assert_eq!(AmountRange::parse("Spouse/DC Over $1,000,000"), Some(AmountRange { min: 1_000_001, max: None }));
        assert_eq!(AmountRange::parse("--"), None);
        let r = AmountRange::parse("$1,001 - $15,000").unwrap();
        assert_eq!(r.to_string(), "$1K-$15K");
        assert_eq!(AmountRange::parse("$1,000,001 - $5,000,000").unwrap().to_string(), "$1M-$5M");
        assert_eq!(AmountRange::parse("Over $1,000,000").unwrap().to_string(), ">$1M");
        assert_eq!(r.midpoint(), 8000.5);
    }

    #[test]
    fn tx_types() {
        assert_eq!(TxType::parse("S (partial)"), TxType::SalePartial);
        assert_eq!(TxType::parse("Sale (Partial)"), TxType::SalePartial);
        assert_eq!(TxType::parse("Sale (Full)"), TxType::Sale);
        assert_eq!(TxType::parse("P"), TxType::Purchase);
        assert_eq!(TxType::parse("Exchange"), TxType::Exchange);
        assert!(TxType::SalePartial.is_sell());
    }

    #[test]
    fn dates_and_tickers() {
        assert_eq!(parse_us_date(" 09/16/2026 "), NaiveDate::from_ymd_opt(2026, 9, 16));
        assert_eq!(parse_us_date("Filed 10/7/2026 @ 6:13 PM"), NaiveDate::from_ymd_opt(2026, 10, 7));
        assert_eq!(parse_us_date("13/40/2026"), None);
        assert_eq!(normalize_ticker("brk/b").as_deref(), Some("BRK.B"));
        assert_eq!(normalize_ticker("AAPL").as_deref(), Some("AAPL"));
        assert_eq!(normalize_ticker("91282CGH8"), None);
        assert_eq!(normalize_ticker("--"), None);
    }
}
