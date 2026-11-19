//! OpenDART issued-share counts (stockTotqySttus) per company and report period, for
//! names without a data.go.kr market cap. Counts are keyed by receipt date.

use std::collections::BTreeMap;
use std::fs;

use chrono::{NaiveDate, Utc};
use serde::Deserialize;

use crate::cache::{FetchedRange, RawCache};
use crate::dart_financials::{Period, ReportCode};
use crate::data_go_kr::CollectOutcome;
use crate::error::DataError;
use crate::opendart::DartClient;

pub const SOURCE_ID: &str = "opendart_shares";
pub const ENDPOINT: &str = "https://opendart.fss.or.kr/api/stockTotqySttus.json";

const STATUS_OK: &str = "000";
const STATUS_NO_DATA: &str = "013";
// Row label of common shares in the `se` column.
const COMMON_SHARES: &str = "보통주";

#[derive(Deserialize)]
struct RawBody {
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    list: Vec<RawRow>,
}

#[derive(Deserialize)]
struct RawRow {
    #[serde(default)]
    rcept_no: String,
    #[serde(default)]
    se: String,
    #[serde(default)]
    istc_totqy: String,
}

/// Issued common shares reported in one filing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareCount {
    pub filed: NaiveDate,
    pub shares: u64,
}

fn key(stock_code: &str) -> String {
    format!("S{stock_code}")
}

fn count(text: &str) -> Option<u64> {
    let digits: String = text
        .chars()
        .filter(|c| *c != ',' && !c.is_whitespace())
        .collect();
    digits.parse::<u64>().ok().filter(|v| *v > 0)
}

/// Issued common shares in one response; `Ok(None)` for "no data" or no common row.
pub fn parse(body: &str) -> Result<Option<ShareCount>, DataError> {
    let raw: RawBody = serde_json::from_str(body.trim_start_matches('\u{feff}'))?;
    match raw.status.as_str() {
        STATUS_OK => {}
        STATUS_NO_DATA => return Ok(None),
        _ => {
            return Err(DataError::Api {
                code: raw.status,
                message: raw.message,
            });
        }
    }
    Ok(raw.list.iter().find_map(|row| {
        if row.se.replace(' ', "") != COMMON_SHARES {
            return None;
        }
        let filed = NaiveDate::parse_from_str(row.rcept_no.get(..8)?, "%Y%m%d").ok()?;
        Some(ShareCount {
            filed,
            shares: count(&row.istc_totqy)?,
        })
    }))
}

pub fn is_pending(cache: &RawCache, stock_code: &str, period: Period) -> Result<bool, DataError> {
    let manifest = cache.load_manifest_for(&key(stock_code))?;
    Ok(!manifest.ranges.iter().any(|r| r.begin == period.end()))
}

// One company's share counts for one period; recorded as done once found or final.
pub async fn collect(
    client: &mut DartClient,
    cache: &RawCache,
    stock_code: &str,
    corp_code: &str,
    period: Period,
) -> Result<CollectOutcome, DataError> {
    let key = key(stock_code);
    let year = period.year.to_string();
    let body = client
        .get_endpoint(
            ENDPOINT,
            &[
                ("corp_code", corp_code),
                ("bsns_year", year.as_str()),
                ("reprt_code", period.report.code()),
            ],
        )
        .await?;
    let rows = usize::from(parse(&body)?.is_some());
    let page_no = match period.report {
        ReportCode::Q1 => 1,
        ReportCode::Half => 2,
        ReportCode::Q3 => 3,
        ReportCode::Annual => 4,
    };
    let page = cache.store_page_for(
        &key,
        period.end(),
        period.end(),
        page_no,
        &body,
        rows,
        Utc::now(),
    )?;
    if rows > 0 || period.is_final(Utc::now().date_naive()) {
        let mut manifest = cache.load_manifest_for(&key)?;
        manifest.ranges.push(FetchedRange {
            begin: period.end(),
            end: period.end(),
            pages: vec![page],
        });
        cache.save_manifest_for(&key, &manifest)?;
    }
    Ok(CollectOutcome {
        ranges_fetched: 1,
        pages: 1,
        rows,
    })
}

/// Every cached share count per stock code, sorted by receipt date (one per date).
pub fn load(cache: &RawCache) -> Result<BTreeMap<String, Vec<ShareCount>>, DataError> {
    let entries = match fs::read_dir(cache.root()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(DataError::io(cache.root(), e)),
    };
    let mut out = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| DataError::io(cache.root(), e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stock_code) = name.strip_prefix('S') else {
            continue;
        };
        let mut counts: Vec<ShareCount> = Vec::new();
        for cached in cache.load_bodies(&name)? {
            counts.extend(parse(&cached.body)?);
        }
        counts.sort_by_key(|c| c.filed);
        counts.dedup_by_key(|c| c.filed);
        if !counts.is_empty() {
            out.insert(stock_code.to_owned(), counts);
        }
    }
    Ok(out)
}

/// Share count known on `date`: the latest filing on or before it, else the earliest
/// filing (later information; acceptable for training rows only).
pub fn shares_on(counts: &[ShareCount], date: NaiveDate) -> Option<u64> {
    let known = counts.partition_point(|c| c.filed <= date);
    match known {
        0 => counts.first().map(|c| c.shares),
        n => Some(counts[n - 1].shares),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_row_and_receipt_date_are_parsed() {
        let body = r#"{"status":"000","message":"정상","list":[
            {"rcept_no":"20180402000123","se":"보통주","istc_totqy":"12,345,678"},
            {"rcept_no":"20180402000123","se":"우선주","istc_totqy":"1,000"},
            {"rcept_no":"20180402000123","se":"합계","istc_totqy":"12,346,678"}]}"#;
        let got = parse(body).expect("parse").expect("common row");
        assert_eq!(got.shares, 12_345_678);
        assert_eq!(
            got.filed,
            NaiveDate::from_ymd_opt(2018, 4, 2).expect("date")
        );
        assert_eq!(
            parse(r#"{"status":"013","message":"없음"}"#).expect("no data"),
            None
        );
        assert!(parse(r#"{"status":"020","message":"limit"}"#).is_err());
        let dash = r#"{"status":"000","list":[{"rcept_no":"20180402000123","se":"보통주","istc_totqy":"-"}]}"#;
        assert_eq!(parse(dash).expect("parse"), None);
    }

    #[test]
    fn shares_on_uses_latest_known_else_earliest() {
        let d = |m, day| NaiveDate::from_ymd_opt(2018, m, day).expect("date");
        let counts = [
            ShareCount {
                filed: d(3, 30),
                shares: 100,
            },
            ShareCount {
                filed: d(8, 14),
                shares: 200,
            },
        ];
        assert_eq!(shares_on(&counts, d(1, 2)), Some(100));
        assert_eq!(shares_on(&counts, d(3, 30)), Some(100));
        assert_eq!(shares_on(&counts, d(9, 1)), Some(200));
        assert_eq!(shares_on(&[], d(9, 1)), None);
    }
}
