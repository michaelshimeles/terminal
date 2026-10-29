//! US Treasury par yields from Treasury.gov's daily yield-curve CSV (one file per year),
//! with FRED (`fredgraph.csv`) as a fallback for single-tenor history.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, NaiveDate};
use tokio::sync::Mutex;
use tr_core::{Bar, Security, Timeframe, catalog};

use crate::{CurvePoint, YieldCurve, YieldCurves, pick_curves};

const TREASURY: &str =
    "https://home.treasury.gov/resource-center/data-chart-center/interest-rates/daily-treasury-rates.csv";
const FRED: &str = "https://fred.stlouisfed.org/graph/fredgraph.csv";
/// Treasury.gov publishes this file back to 1990.
const FIRST_YEAR: i32 = 1990;

/// One day of the par curve: column label (e.g. `10 Yr`) -> yield in percent.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveRow {
    pub date: NaiveDate,
    pub values: HashMap<String, f64>,
}

fn midnight(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp()).unwrap_or_default()
}

/// Parse a Treasury.gov daily par yield curve CSV (`Date,"1 Mo",…,"30 Yr"`).
pub fn parse_treasury_csv(csv: &str) -> Result<Vec<CurveRow>> {
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header: Vec<String> = lines
        .next()
        .ok_or_else(|| anyhow!("empty Treasury CSV"))?
        .split(',')
        .map(|h| h.trim().trim_matches('"').to_string())
        .collect();
    if header.first().map(String::as_str) != Some("Date") {
        bail!("unexpected Treasury CSV header: {header:?}");
    }
    let mut rows: Vec<CurveRow> = lines
        .filter_map(|l| {
            let cells: Vec<&str> = l.split(',').map(|c| c.trim().trim_matches('"')).collect();
            let date = NaiveDate::parse_from_str(cells.first()?, "%m/%d/%Y").ok()?;
            let values = header
                .iter()
                .zip(&cells)
                .skip(1)
                .filter_map(|(h, c)| c.parse::<f64>().ok().map(|v| (h.clone(), v)))
                .collect();
            Some(CurveRow { date, values })
        })
        .collect();
    rows.sort_by_key(|r| r.date);
    Ok(rows)
}

/// Parse FRED's `fredgraph.csv` (`observation_date,DGS10` or `DATE,DGS10`; missing values
/// are empty or `.`). Returns daily bars with O=H=L=C=yield.
pub fn parse_fred_csv(csv: &str) -> Result<Vec<Bar>> {
    let mut lines = csv.lines();
    let header = lines.next().ok_or_else(|| anyhow!("empty FRED CSV"))?;
    if !header.to_ascii_lowercase().contains("date") {
        bail!("unexpected FRED header: {header}");
    }
    Ok(lines
        .filter_map(|l| {
            let (d, v) = l.split_once(',')?;
            let date = NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok()?;
            let y: f64 = v.trim().parse().ok()?;
            Some(Bar::new(midnight(date), y, y, y, y, 0.0))
        })
        .collect())
}

/// Daily bars for one curve column.
pub fn column_bars(rows: &[CurveRow], column: &str) -> Vec<Bar> {
    rows.iter().filter_map(|r| r.values.get(column).map(|&y| Bar::new(midnight(r.date), y, y, y, y, 0.0))).collect()
}

/// Curve snapshots (now, ~1M, ~1Y ago) from parsed rows.
pub fn curves_from_rows(rows: &[CurveRow]) -> Option<YieldCurves> {
    let tenors = catalog::treasury_curve();
    let curves = rows
        .iter()
        .map(|r| YieldCurve {
            ts: midnight(r.date),
            points: tenors
                .iter()
                .filter_map(|l| {
                    Some(CurvePoint {
                        ticker: l.ticker.into(),
                        tenor_years: l.tenor_years?,
                        yield_pct: *r.values.get(l.treasury?)?,
                    })
                })
                .collect(),
        })
        .collect();
    pick_curves(curves)
}

/// Parsed year files with the time they were fetched.
type YearCache = HashMap<i32, (Instant, std::sync::Arc<Vec<CurveRow>>)>;

pub struct TreasuryProvider {
    client: reqwest::Client,
    years: Mutex<YearCache>,
}

impl TreasuryProvider {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; terminal)")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client, years: Mutex::new(HashMap::new()) })
    }

    /// One year of the curve, cached (the current year refreshes every 15 minutes).
    async fn year(&self, y: i32) -> Result<std::sync::Arc<Vec<CurveRow>>> {
        let current = chrono::Utc::now().year();
        if let Some((at, rows)) = self.years.lock().await.get(&y)
            && (y < current || at.elapsed() < Duration::from_secs(900))
        {
            return Ok(rows.clone());
        }
        let url = format!("{TREASURY}/{y}/all");
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("type", "daily_treasury_yield_curve"),
                ("field_tdr_date_value", &y.to_string()),
                ("_format", "csv"),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("Treasury.gov {y}: HTTP {}", resp.status());
        }
        let rows =
            std::sync::Arc::new(parse_treasury_csv(&resp.text().await?).with_context(|| format!("Treasury.gov {y}"))?);
        self.years.lock().await.insert(y, (Instant::now(), rows.clone()));
        Ok(rows)
    }

    /// Rows for every year in `[from, to]`, oldest first.
    async fn rows(&self, from: i32, to: i32) -> Result<Vec<CurveRow>> {
        let mut out = Vec::new();
        for y in from.max(FIRST_YEAR)..=to {
            match self.year(y).await {
                Ok(r) => out.extend(r.iter().cloned()),
                // Early in January the new year's file can be empty or missing.
                Err(e) if y == to && !out.is_empty() => tracing::debug!("{e:#}"),
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub async fn history(&self, sec: &Security, tf: Timeframe, end: Option<i64>, limit: usize) -> Result<Vec<Bar>> {
        let listing = catalog::by_security(sec).ok_or_else(|| anyhow!("unknown Treasury tenor {}", sec.ticker))?;
        let column = listing.treasury.ok_or_else(|| anyhow!("{} has no Treasury.gov column", sec.ticker))?;
        let end_year = end
            .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
            .map(|d| d.year())
            .unwrap_or_else(|| chrono::Utc::now().year());
        // ~250 business days a year; cap a single request at 12 years of files.
        let days_needed = (limit as i64 * tf.seconds().max(86_400) / 86_400) * 5 / 7 + 10;
        let years = ((days_needed / 250) as i32 + 1).clamp(1, 12);
        let result = match self.rows(end_year - years + 1, end_year).await {
            Ok(rows) => Ok(column_bars(&rows, column)),
            Err(e) => match listing.fred {
                Some(id) => self.fred(id, end_year - years + 1).await.map_err(|f| e.context(format!("FRED: {f:#}"))),
                None => Err(e),
            },
        };
        let mut bars = result?;
        if tf > Timeframe::D1 {
            bars = tf.resample(&bars);
        }
        if bars.is_empty() && end.is_some() {
            bail!("no older Treasury data");
        }
        Ok(crate::clip(bars, end, limit))
    }

    async fn fred(&self, series: &str, from_year: i32) -> Result<Vec<Bar>> {
        let resp = self
            .client
            .get(FRED)
            .query(&[("id", series.to_string()), ("cosd", format!("{from_year}-01-01"))])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("FRED {series}: HTTP {}", resp.status());
        }
        parse_fred_csv(&resp.text().await?)
    }

    pub async fn curves(&self) -> Result<YieldCurves> {
        let y = chrono::Utc::now().year();
        let rows = self.rows(y - 1, y).await?;
        curves_from_rows(&rows).ok_or_else(|| anyhow!("empty Treasury curve"))
    }

    /// Latest and previous close for each requested tenor.
    pub async fn quotes(&self, secs: &[(String, Security)]) -> Result<Vec<crate::Quote>> {
        let y = chrono::Utc::now().year();
        let rows = self.rows(y - 1, y).await?;
        Ok(secs
            .iter()
            .filter_map(|(key, sec)| {
                let col = catalog::by_security(sec)?.treasury?;
                let bars = column_bars(&rows, col);
                let last = bars.last()?;
                let prev = bars.get(bars.len().checked_sub(2)?).map(|b| b.close).unwrap_or(last.close);
                let spark = bars.iter().rev().take(8).rev().map(|b| b.close).collect();
                Some(crate::Quote { symbol: key.clone(), last: last.close, prev_close: prev, ts: last.ts, spark })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    #[test]
    fn treasury_csv() {
        let csv = std::fs::read_to_string(format!("{FIX}/treasury_2026.csv")).unwrap();
        let rows = parse_treasury_csv(&csv).unwrap();
        assert!(rows.len() > 150);
        assert!(rows.windows(2).all(|w| w[0].date < w[1].date));
        let last = rows.last().unwrap();
        assert_eq!(last.date, NaiveDate::from_ymd_opt(2026, 10, 9).unwrap());
        assert_eq!(last.values["10 Yr"], 5.24);
        assert_eq!(last.values["30 Yr"], 5.60);
        let bars = column_bars(&rows, "10 Yr");
        assert_eq!(bars.len(), rows.len());
        let c = curves_from_rows(&rows).unwrap();
        assert_eq!(c.current.points.len(), 11);
        assert_eq!(c.current.get("US2Y"), Some(4.80));
        let m = c.month_ago.unwrap();
        assert!(c.current.ts - m.ts >= 30 * 86_400 && c.current.ts - m.ts < 40 * 86_400);
        // The fixture only covers 2026, so there is no year-ago curve.
        assert!(c.year_ago.is_none());
        assert!(parse_treasury_csv("nonsense\n1,2").is_err());
    }

    #[test]
    fn fred_csv() {
        let csv = std::fs::read_to_string(format!("{FIX}/fred_dgs10.csv")).unwrap();
        let bars = parse_fred_csv(&csv).unwrap();
        assert_eq!(bars.len(), 5); // the empty latest observation is skipped
        assert_eq!(bars[4].close, 5.24);
        assert_eq!(parse_fred_csv("DATE,DGS10\n2020-01-02,.\n2020-01-03,1.8\n").unwrap().len(), 1);
    }
}
