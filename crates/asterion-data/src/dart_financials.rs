//! OpenDART financial statements: key accounts for many companies per request, and
//! full statements per company. Values are keyed by receipt date, never period end.

use std::collections::BTreeMap;
use std::fs;

use chrono::{Days, NaiveDate, Utc};
use serde::Deserialize;

use crate::cache::{FetchedRange, RawCache};
use crate::data_go_kr::CollectOutcome;
use crate::error::DataError;
use crate::opendart::DartClient;

pub const SOURCE_ID: &str = "opendart_fin";
pub const DEFAULT_ROOT: &str = "data/raw/opendart_fin";
pub const MULTI_ENDPOINT: &str = "https://opendart.fss.or.kr/api/fnlttMultiAcnt.json";
pub const FULL_ENDPOINT: &str = "https://opendart.fss.or.kr/api/fnlttSinglAcntAll.json";
// The API serves business years from 2015 onward.
pub const FIRST_YEAR: i32 = 2015;
// fnlttMultiAcnt accepts at most this many corp codes per request (status 021 above it).
pub const MULTI_BATCH: usize = 100;

const STATUS_OK: &str = "000";
const STATUS_NO_DATA: &str = "013";
// Quarterly reports are due 45 days and annual reports 90 days after period end; past
// this margin an empty answer is final and is cached instead of being asked again.
const FINAL_AFTER_DAYS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReportCode {
    Q1,
    Half,
    Q3,
    Annual,
}

impl ReportCode {
    pub const ALL: [ReportCode; 4] = [
        ReportCode::Q1,
        ReportCode::Half,
        ReportCode::Q3,
        ReportCode::Annual,
    ];

    pub fn code(self) -> &'static str {
        match self {
            ReportCode::Q1 => "11013",
            ReportCode::Half => "11012",
            ReportCode::Q3 => "11014",
            ReportCode::Annual => "11011",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.code() == code.trim())
    }

    // Months of the fiscal year covered by cumulative income-statement amounts.
    pub fn months(self) -> u32 {
        match self {
            ReportCode::Q1 => 3,
            ReportCode::Half => 6,
            ReportCode::Q3 => 9,
            ReportCode::Annual => 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Period {
    pub year: i32,
    pub report: ReportCode,
}

impl Period {
    pub fn end(self) -> NaiveDate {
        let (m, d) = match self.report {
            ReportCode::Q1 => (3, 31),
            ReportCode::Half => (6, 30),
            ReportCode::Q3 => (9, 30),
            ReportCode::Annual => (12, 31),
        };
        NaiveDate::from_ymd_opt(self.year, m, d).unwrap_or(NaiveDate::MIN)
    }

    pub fn is_final(self, today: NaiveDate) -> bool {
        self.end()
            .checked_add_days(Days::new(FINAL_AFTER_DAYS))
            .is_some_and(|d| d < today)
    }

    fn multi_key(self) -> String {
        format!("M{}_{}", self.year, self.report.code())
    }
}

// Every period from `first_year` whose period has ended before `today`, oldest first.
pub fn periods(first_year: i32, today: NaiveDate) -> Vec<Period> {
    let mut out = Vec::new();
    for year in first_year..=today.format("%Y").to_string().parse().unwrap_or(first_year) {
        for report in ReportCode::ALL {
            let p = Period { year, report };
            if p.end() < today {
                out.push(p);
            }
        }
    }
    out
}

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
    stock_code: String,
    #[serde(default)]
    fs_div: String,
    #[serde(default)]
    sj_div: String,
    #[serde(default)]
    account_nm: String,
    #[serde(default)]
    thstrm_amount: String,
    #[serde(default)]
    thstrm_add_amount: String,
    #[serde(default)]
    frmtrm_amount: String,
    #[serde(default)]
    frmtrm_add_amount: String,
}

// `Ok(None)` for "no data"; quota, key and parameter problems are API errors.
fn parse_body(body: &str) -> Result<Option<RawBody>, DataError> {
    let raw: RawBody = serde_json::from_str(body.trim_start_matches('\u{feff}'))?;
    match raw.status.as_str() {
        STATUS_OK => Ok(Some(raw)),
        STATUS_NO_DATA => Ok(None),
        _ => Err(DataError::Api {
            code: raw.status,
            message: raw.message,
        }),
    }
}

fn amount(text: &str) -> Option<f64> {
    let cleaned: String = text
        .chars()
        .filter(|c| *c != ',' && !c.is_whitespace())
        .collect();
    cleaned.parse::<f64>().ok().filter(|v| v.is_finite())
}

// Receipt date encoded in the first eight digits of the receipt number.
fn receipt_date(rcept_no: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(rcept_no.get(..8)?, "%Y%m%d").ok()
}

// Key accounts of one report as filed. Income amounts are year-to-date cumulative.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FinancialReport {
    pub stock_code: String,
    pub filed: NaiveDate,
    pub period: Option<Period>,
    pub consolidated: bool,
    pub revenue: Option<f64>,
    pub operating_income: Option<f64>,
    pub net_income: Option<f64>,
    pub total_assets: Option<f64>,
    pub total_liabilities: Option<f64>,
    pub total_equity: Option<f64>,
    // Same accounts for the same span one year earlier, as restated in this report.
    pub prior_revenue: Option<f64>,
    pub prior_net_income: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Account {
    Revenue,
    OperatingIncome,
    NetIncome,
    Assets,
    Liabilities,
    Equity,
}

// DART account names as the API spells them, compared with spaces removed.
fn account(name: &str) -> Option<Account> {
    let n: String = name.chars().filter(|c| !c.is_whitespace()).collect();
    if n.starts_with("매출액") || n == "영업수익" || n == "수익(매출액)" {
        Some(Account::Revenue)
    } else if n.starts_with("영업이익") {
        Some(Account::OperatingIncome)
    } else if n.starts_with("당기순이익")
        || n.starts_with("분기순이익")
        || n.starts_with("반기순이익")
    {
        Some(Account::NetIncome)
    } else if n == "자산총계" {
        Some(Account::Assets)
    } else if n == "부채총계" {
        Some(Account::Liabilities)
    } else if n == "자본총계" {
        Some(Account::Equity)
    } else {
        None
    }
}

// Rows of one key-accounts body grouped into reports, one per company and statement basis.
pub fn parse_multi(body: &str, period: Period) -> Result<Vec<FinancialReport>, DataError> {
    let Some(raw) = parse_body(body)? else {
        return Ok(Vec::new());
    };
    let mut reports: BTreeMap<(String, bool), FinancialReport> = BTreeMap::new();
    for row in raw.list {
        let code = row.stock_code.trim();
        let Some(filed) = receipt_date(row.rcept_no.trim()) else {
            continue;
        };
        if code.is_empty() {
            continue;
        }
        let consolidated = row.fs_div.trim() == "CFS";
        let report = reports
            .entry((code.to_owned(), consolidated))
            .or_insert_with(|| FinancialReport {
                stock_code: code.to_owned(),
                filed,
                period: Some(period),
                consolidated,
                ..FinancialReport::default()
            });
        report.filed = report.filed.max(filed);
        let flow = row.sj_div.trim() == "IS";
        let current = if flow {
            amount(&row.thstrm_add_amount).or_else(|| amount(&row.thstrm_amount))
        } else {
            amount(&row.thstrm_amount)
        };
        let prior = amount(&row.frmtrm_add_amount).or_else(|| amount(&row.frmtrm_amount));
        match account(&row.account_nm) {
            Some(Account::Revenue) if flow => {
                report.revenue = report.revenue.or(current);
                report.prior_revenue = report.prior_revenue.or(prior);
            }
            Some(Account::OperatingIncome) if flow => {
                report.operating_income = report.operating_income.or(current);
            }
            Some(Account::NetIncome) if flow => {
                report.net_income = report.net_income.or(current);
                report.prior_net_income = report.prior_net_income.or(prior);
            }
            Some(Account::Assets) if !flow => report.total_assets = report.total_assets.or(current),
            Some(Account::Liabilities) if !flow => {
                report.total_liabilities = report.total_liabilities.or(current);
            }
            Some(Account::Equity) if !flow => report.total_equity = report.total_equity.or(current),
            _ => {}
        }
    }
    Ok(reports.into_values().collect())
}

fn now_kst_date() -> NaiveDate {
    (Utc::now() + chrono::Duration::hours(9)).date_naive()
}

pub fn multi_is_pending(cache: &RawCache, period: Period) -> Result<bool, DataError> {
    Ok(cache
        .load_manifest_for(&period.multi_key())?
        .ranges
        .is_empty())
}

// Key accounts of every company in `corp_codes` for one period, `MULTI_BATCH` per request.
// The period is recorded as done only when all batches succeeded and the period is final.
pub async fn collect_multi(
    client: &mut DartClient,
    cache: &RawCache,
    period: Period,
    corp_codes: &[String],
) -> Result<CollectOutcome, DataError> {
    let key = period.multi_key();
    let year = period.year.to_string();
    let mut pages = Vec::new();
    let mut rows = 0;
    for (i, batch) in corp_codes.chunks(MULTI_BATCH).enumerate() {
        let joined = batch.join(",");
        let body = client
            .get_endpoint(
                MULTI_ENDPOINT,
                &[
                    ("corp_code", joined.as_str()),
                    ("bsns_year", year.as_str()),
                    ("reprt_code", period.report.code()),
                ],
            )
            .await?;
        let count = parse_multi(&body, period)?.len();
        rows += count;
        let page_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        pages.push(cache.store_page_for(
            &key,
            period.end(),
            period.end(),
            page_no,
            &body,
            count,
            Utc::now(),
        )?);
    }
    let outcome = CollectOutcome {
        ranges_fetched: 1,
        pages: pages.len(),
        rows,
    };
    if period.is_final(now_kst_date()) {
        let mut manifest = cache.load_manifest_for(&key)?;
        manifest.ranges.push(FetchedRange {
            begin: period.end(),
            end: period.end(),
            pages,
        });
        cache.save_manifest_for(&key, &manifest)?;
    }
    Ok(outcome)
}

fn full_key(corp_code: &str) -> String {
    format!("F{corp_code}")
}

pub fn full_is_pending(
    cache: &RawCache,
    corp_code: &str,
    period: Period,
) -> Result<bool, DataError> {
    let manifest = cache.load_manifest_for(&full_key(corp_code))?;
    Ok(!manifest.ranges.iter().any(|r| r.begin == period.end()))
}

// Full statements of one company for one period: consolidated, falling back to separate
// when the company publishes no consolidated statements. Returns line items stored.
pub async fn collect_full(
    client: &mut DartClient,
    cache: &RawCache,
    corp_code: &str,
    period: Period,
) -> Result<CollectOutcome, DataError> {
    let key = full_key(corp_code);
    let year = period.year.to_string();
    let mut pages = Vec::new();
    let mut rows = 0;
    for (page_no, fs_div) in [(1, "CFS"), (2, "OFS")] {
        let body = client
            .get_endpoint(
                FULL_ENDPOINT,
                &[
                    ("corp_code", corp_code),
                    ("bsns_year", year.as_str()),
                    ("reprt_code", period.report.code()),
                    ("fs_div", fs_div),
                ],
            )
            .await?;
        let count = parse_body(&body)?.map_or(0, |raw| raw.list.len());
        rows += count;
        pages.push(cache.store_page_for(
            &key,
            period.end(),
            period.end(),
            page_no,
            &body,
            count,
            Utc::now(),
        )?);
        if count > 0 {
            break;
        }
    }
    let outcome = CollectOutcome {
        ranges_fetched: 1,
        pages: pages.len(),
        rows,
    };
    if rows > 0 || period.is_final(now_kst_date()) {
        let mut manifest = cache.load_manifest_for(&key)?;
        manifest.ranges.push(FetchedRange {
            begin: period.end(),
            end: period.end(),
            pages,
        });
        cache.save_manifest_for(&key, &manifest)?;
    }
    Ok(outcome)
}

// Every cached key-accounts report, preferring consolidated over separate statements
// for the same company and period.
pub fn load_reports(cache: &RawCache) -> Result<Vec<FinancialReport>, DataError> {
    let entries = match fs::read_dir(cache.root()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(DataError::io(cache.root(), e)),
    };
    let mut best: BTreeMap<(String, Period), FinancialReport> = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| DataError::io(cache.root(), e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(period) = parse_multi_key(&name) else {
            continue;
        };
        for cached in cache.load_bodies(&name)? {
            for report in parse_multi(&cached.body, period)? {
                let slot = best.entry((report.stock_code.clone(), period));
                match slot {
                    std::collections::btree_map::Entry::Vacant(v) => {
                        v.insert(report);
                    }
                    std::collections::btree_map::Entry::Occupied(mut o) => {
                        if report.consolidated && !o.get().consolidated {
                            o.insert(report);
                        }
                    }
                }
            }
        }
    }
    let mut reports: Vec<FinancialReport> = best.into_values().collect();
    reports.sort_by(|a, b| (&a.stock_code, a.filed).cmp(&(&b.stock_code, b.filed)));
    Ok(reports)
}

fn parse_multi_key(name: &str) -> Option<Period> {
    let rest = name.strip_prefix('M')?;
    let (year, code) = rest.split_once('_')?;
    Some(Period {
        year: year.parse().ok()?,
        report: ReportCode::from_code(code)?,
    })
}

// Corp codes in a stable order, deduplicated.
pub fn sorted_corp_codes(map: &BTreeMap<String, String>) -> Vec<String> {
    let mut codes: Vec<String> = map.values().cloned().collect();
    codes.sort();
    codes.dedup();
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
    }

    #[test]
    fn periods_stop_before_today_and_finality_needs_a_margin() {
        let ps = periods(2025, date(2025, 7, 1));
        assert_eq!(
            ps.iter().map(|p| p.report).collect::<Vec<_>>(),
            vec![ReportCode::Q1, ReportCode::Half]
        );
        let q1 = ps[0];
        assert!(!q1.is_final(date(2025, 7, 1)));
        assert!(q1.is_final(date(2025, 8, 1)));
    }

    #[test]
    fn multi_rows_become_reports_keyed_by_receipt_date() {
        let body = r#"{"status":"000","message":"정상","list":[
          {"rcept_no":"20240514000682","bsns_year":"2024","stock_code":"005930","reprt_code":"11013","account_nm":"매출액","fs_div":"CFS","sj_div":"IS","thstrm_amount":"71,915,601,000,000","thstrm_add_amount":"","frmtrm_amount":"63,745,371,000,000"},
          {"rcept_no":"20240514000682","bsns_year":"2024","stock_code":"005930","reprt_code":"11013","account_nm":"자본총계","fs_div":"CFS","sj_div":"BS","thstrm_amount":"376,000,000,000,000"},
          {"rcept_no":"20240514000682","bsns_year":"2024","stock_code":"005930","reprt_code":"11013","account_nm":"당기순이익(손실)","fs_div":"OFS","sj_div":"IS","thstrm_amount":"-1,000"}
        ]}"#;
        let p = Period {
            year: 2024,
            report: ReportCode::Q1,
        };
        let reports = parse_multi(body, p).expect("parse");
        assert_eq!(reports.len(), 2);
        let cfs = reports.iter().find(|r| r.consolidated).expect("cfs");
        assert_eq!(cfs.filed, date(2024, 5, 14));
        assert_eq!(cfs.revenue, Some(71_915_601_000_000.0));
        assert_eq!(cfs.prior_revenue, Some(63_745_371_000_000.0));
        assert_eq!(cfs.total_equity, Some(376_000_000_000_000.0));
        let ofs = reports.iter().find(|r| !r.consolidated).expect("ofs");
        assert_eq!(ofs.net_income, Some(-1000.0));
    }

    #[test]
    fn no_data_is_empty_and_quota_is_fatal() {
        let p = Period {
            year: 2024,
            report: ReportCode::Q1,
        };
        assert!(
            parse_multi(r#"{"status":"013","message":"x"}"#, p)
                .expect("ok")
                .is_empty()
        );
        let err = parse_multi(r#"{"status":"020","message":"limit"}"#, p).expect_err("quota");
        assert!(err.is_fatal_for_batch());
        assert_eq!(parse_multi_key("M2024_11013"), Some(p));
    }
}
