//! OpenDART (FSS electronic disclosure system) filing lists for listed companies.

use std::collections::BTreeMap;
use std::{env, fmt};

use chrono::{Days, NaiveDate, Utc};
use serde::Deserialize;

use crate::cache::{FetchedRange, RawCache};
use crate::data_go_kr::{CollectOutcome, missing_ranges};
use crate::error::DataError;
use crate::http::{Http, RetryPolicy};
use crate::redact::MASK;

pub const SOURCE_ID: &str = "opendart";
pub const ENDPOINT: &str = "https://opendart.fss.or.kr/api/list.json";
pub const DEFAULT_ROOT: &str = "data/raw/opendart";
pub const ATTRIBUTION: &str = "출처: 금융감독원 전자공시시스템(DART), OpenDART(opendart.fss.or.kr)";

// Without a corp_code the API rejects searches longer than three months.
const MAX_WINDOW_DAYS: u64 = 85;
const PAGE_SIZE: &str = "100";
const STATUS_OK: &str = "000";
const STATUS_NO_DATA: &str = "013";

// Disclosure categories (`pblntf_ty`) that are collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilingKind {
    // A: annual, half-year and quarterly reports.
    Periodic,
    // B: material event reports.
    Major,
    // D: ownership and insider holding reports.
    Ownership,
    // I: exchange disclosures (contracts, preliminary earnings, ...).
    Exchange,
}

impl FilingKind {
    pub const ALL: [FilingKind; 4] = [
        FilingKind::Periodic,
        FilingKind::Major,
        FilingKind::Ownership,
        FilingKind::Exchange,
    ];

    pub fn code(self) -> &'static str {
        match self {
            FilingKind::Periodic => "A",
            FilingKind::Major => "B",
            FilingKind::Ownership => "D",
            FilingKind::Exchange => "I",
        }
    }
}

// Listing board (`corp_cls`); the API takes one value per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Board {
    Kospi,
    Kosdaq,
}

impl Board {
    pub const ALL: [Board; 2] = [Board::Kospi, Board::Kosdaq];

    pub fn code(self) -> &'static str {
        match self {
            Board::Kospi => "Y",
            Board::Kosdaq => "K",
        }
    }
}

// One category on one board: the unit that is requested and cached together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Feed {
    pub kind: FilingKind,
    pub board: Board,
}

impl Feed {
    pub fn all() -> Vec<Feed> {
        FilingKind::ALL
            .into_iter()
            .flat_map(|kind| {
                Board::ALL
                    .into_iter()
                    .map(move |board| Feed { kind, board })
            })
            .collect()
    }

    pub fn key(self) -> String {
        format!("{}_{}", self.kind.code(), self.board.code())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DartKey(String);

impl DartKey {
    pub const ENV_VAR: &'static str = "OPENDART_API_KEY";

    pub fn new(raw: &str) -> Self {
        DartKey(raw.trim().to_owned())
    }

    pub fn from_env() -> Result<Self, DataError> {
        match env::var(Self::ENV_VAR) {
            Ok(value) if !value.trim().is_empty() => Ok(Self::new(&value)),
            _ => Err(DataError::MissingServiceKey(Self::ENV_VAR)),
        }
    }
}

impl fmt::Debug for DartKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DartKey({MASK})")
    }
}

#[derive(Debug)]
pub struct DartClient {
    http: Http,
    key: DartKey,
    endpoint: String,
}

impl DartClient {
    pub fn new(key: DartKey, endpoint: &str, policy: RetryPolicy) -> Result<Self, DataError> {
        let http = Http::with_native_tls(&key.0, policy)?;
        Ok(DartClient {
            http,
            key,
            endpoint: endpoint.to_owned(),
        })
    }

    pub fn requests_sent(&self) -> u32 {
        self.http.requests_sent()
    }

    // Raw body from any OpenDART endpoint; the key is added to `params`.
    pub async fn get_endpoint(
        &mut self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<String, DataError> {
        let mut query = Vec::with_capacity(params.len() + 1);
        query.push(("crtfc_key", self.key.0.as_str()));
        query.extend_from_slice(params);
        self.http.get(url, &query).await
    }

    // Raw bytes from an OpenDART endpoint that answers with a file; the key is added to `params`.
    pub async fn get_endpoint_bytes(
        &mut self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<u8>, DataError> {
        let mut query = Vec::with_capacity(params.len() + 1);
        query.push(("crtfc_key", self.key.0.as_str()));
        query.extend_from_slice(params);
        self.http.get_bytes(url, &query).await
    }

    // Raw JSON body of one result page for an inclusive receipt-date range.
    pub async fn fetch_page(
        &mut self,
        feed: Feed,
        begin: NaiveDate,
        end: NaiveDate,
        page_no: u32,
    ) -> Result<String, DataError> {
        let begin = begin.format("%Y%m%d").to_string();
        let end = end.format("%Y%m%d").to_string();
        let page_no = page_no.to_string();
        let endpoint = self.endpoint.clone();
        self.http
            .get(
                &endpoint,
                &[
                    ("crtfc_key", self.key.0.as_str()),
                    ("bgn_de", begin.as_str()),
                    ("end_de", end.as_str()),
                    ("pblntf_ty", feed.kind.code()),
                    ("corp_cls", feed.board.code()),
                    ("page_no", page_no.as_str()),
                    ("page_count", PAGE_SIZE),
                ],
            )
            .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filing {
    pub receipt_no: String,
    pub stock_code: String,
    // Receipt date; the API does not expose the time of day.
    pub date: NaiveDate,
    pub kind: FilingKind,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilingPage {
    pub total_pages: u32,
    pub filings: Vec<Filing>,
}

#[derive(Deserialize)]
struct RawBody {
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    total_page: serde_json::Value,
    #[serde(default)]
    list: Vec<RawFiling>,
}

#[derive(Deserialize)]
struct RawFiling {
    #[serde(default)]
    corp_code: String,
    #[serde(default)]
    stock_code: String,
    report_nm: String,
    rcept_no: String,
    rcept_dt: String,
}

// Rows without a stock code (unlisted filers) are skipped; "no data" is an empty page.
pub fn parse_page(body: &str, kind: FilingKind) -> Result<FilingPage, DataError> {
    let raw: RawBody = serde_json::from_str(body.trim_start_matches('\u{feff}'))?;
    if raw.status == STATUS_NO_DATA {
        return Ok(FilingPage {
            total_pages: 0,
            filings: Vec::new(),
        });
    }
    if raw.status != STATUS_OK {
        return Err(DataError::Api {
            code: raw.status,
            message: raw.message,
        });
    }

    // The count arrives as a JSON number, but tolerate a numeric string.
    let total_pages = raw
        .total_page
        .as_u64()
        .or_else(|| raw.total_page.as_str().and_then(|s| s.trim().parse().ok()))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| DataError::InvalidField {
            field: "total_page",
            value: raw.total_page.to_string(),
        })?;

    let mut filings = Vec::with_capacity(raw.list.len());
    for row in raw.list {
        let stock_code = row.stock_code.trim();
        if stock_code.is_empty() {
            continue;
        }
        let date = NaiveDate::parse_from_str(row.rcept_dt.trim(), "%Y%m%d").map_err(|_| {
            DataError::InvalidField {
                field: "rcept_dt",
                value: row.rcept_dt.clone(),
            }
        })?;
        filings.push(Filing {
            receipt_no: row.rcept_no.trim().to_owned(),
            stock_code: stock_code.to_owned(),
            date,
            kind,
            title: row.report_nm.trim().to_owned(),
        });
    }
    Ok(FilingPage {
        total_pages,
        filings,
    })
}

// Uncached parts of `[begin, end]` for `feed`, cut into windows the API accepts.
pub fn pending_windows(
    cache: &RawCache,
    feed: Feed,
    begin: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<(NaiveDate, NaiveDate)>, DataError> {
    let manifest = cache.load_manifest_for(&feed.key())?;
    let mut windows = Vec::new();
    for (range_begin, range_end) in missing_ranges(&manifest.covered(), begin, end) {
        let mut window_begin = range_begin;
        while window_begin <= range_end {
            let window_end = window_begin
                .checked_add_days(Days::new(MAX_WINDOW_DAYS - 1))
                .map_or(range_end, |d| d.min(range_end));
            windows.push((window_begin, window_end));
            let Some(next) = window_end.succ_opt() else {
                break;
            };
            window_begin = next;
        }
    }
    Ok(windows)
}

// Fetches every page of one window. The window is recorded only once complete, so
// an interrupted run re-fetches it; `end` must be a past date whose filings are final.
pub async fn collect_window(
    client: &mut DartClient,
    cache: &RawCache,
    feed: Feed,
    begin: NaiveDate,
    end: NaiveDate,
) -> Result<CollectOutcome, DataError> {
    let key = feed.key();
    let mut pages = Vec::new();
    let mut rows = 0;
    let mut page_no = 1;
    loop {
        let body = client.fetch_page(feed, begin, end, page_no).await?;
        let page = parse_page(&body, feed.kind)?;
        rows += page.filings.len();
        pages.push(cache.store_page_for(
            &key,
            begin,
            end,
            page_no,
            &body,
            page.filings.len(),
            Utc::now(),
        )?);
        if page_no >= page.total_pages {
            break;
        }
        page_no += 1;
    }

    let outcome = CollectOutcome {
        ranges_fetched: 1,
        pages: pages.len(),
        rows,
    };
    let mut manifest = cache.load_manifest_for(&key)?;
    manifest.ranges.push(FetchedRange { begin, end, pages });
    cache.save_manifest_for(&key, &manifest)?;
    Ok(outcome)
}

// Every cached filing across all feeds, one per receipt number, sorted by date.
pub fn load_filings(cache: &RawCache) -> Result<Vec<Filing>, DataError> {
    let mut filings = Vec::new();
    for feed in Feed::all() {
        for cached in cache.load_bodies(&feed.key())? {
            filings.extend(
                parse_page(&cached.body, feed.kind)?
                    .filings
                    .into_iter()
                    .filter(|f| f.date >= cached.begin && f.date <= cached.end),
            );
        }
    }
    filings.sort_by(|a, b| a.receipt_no.cmp(&b.receipt_no));
    filings.dedup_by(|a, b| a.receipt_no == b.receipt_no);
    filings.sort_by(|a, b| (a.date, &a.receipt_no).cmp(&(b.date, &b.receipt_no)));
    Ok(filings)
}

// Stock code to DART corp_code for every listed filer seen in the cached filing lists.
pub fn load_corp_codes(cache: &RawCache) -> Result<BTreeMap<String, String>, DataError> {
    let mut map = BTreeMap::new();
    for feed in Feed::all() {
        for cached in cache.load_bodies(&feed.key())? {
            let raw: RawBody = serde_json::from_str(cached.body.trim_start_matches('\u{feff}'))?;
            for row in raw.list {
                let (stock, corp) = (row.stock_code.trim(), row.corp_code.trim());
                if !stock.is_empty() && corp.len() == 8 {
                    map.insert(stock.to_owned(), corp.to_owned());
                }
            }
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, m, day).expect("valid date")
    }

    #[test]
    fn parses_listed_filings_and_skips_unlisted_rows() {
        let body = r#"{"status":"000","message":"정상","page_no":1,"page_count":100,"total_count":2,"total_page":3,"list":[
            {"corp_code":"00126380","corp_name":"삼성전자","stock_code":"005930","corp_cls":"Y","report_nm":"분기보고서 (2024.03)","rcept_no":"20240516001421","flr_nm":"삼성전자","rcept_dt":"20240516","rm":""},
            {"corp_code":"00000001","corp_name":"비상장","stock_code":" ","corp_cls":"E","report_nm":"감사보고서","rcept_no":"20240516000001","flr_nm":"x","rcept_dt":"20240516","rm":""}
        ]}"#;
        let page = parse_page(body, FilingKind::Periodic).expect("parse");
        assert_eq!(page.total_pages, 3);
        assert_eq!(
            page.filings,
            vec![Filing {
                receipt_no: "20240516001421".to_owned(),
                stock_code: "005930".to_owned(),
                date: d(5, 16),
                kind: FilingKind::Periodic,
                title: "분기보고서 (2024.03)".to_owned(),
            }]
        );
    }

    #[test]
    fn no_data_is_empty_and_other_statuses_are_api_errors() {
        let empty = parse_page(
            r#"{"status":"013","message":"조회된 데이타가 없습니다."}"#,
            FilingKind::Major,
        )
        .expect("empty");
        assert_eq!((empty.total_pages, empty.filings.len()), (0, 0));

        match parse_page(
            r#"{"status":"020","message":"요청 제한을 초과하였습니다."}"#,
            FilingKind::Major,
        ) {
            Err(e @ DataError::Api { .. }) => assert!(e.is_fatal_for_batch()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pending_windows_respect_the_three_month_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = RawCache::with_source(dir.path(), SOURCE_ID);
        let feed = Feed {
            kind: FilingKind::Exchange,
            board: Board::Kosdaq,
        };
        assert_eq!(feed.key(), "I_K");
        let windows = pending_windows(&cache, feed, d(1, 1), d(6, 30)).expect("windows");
        assert_eq!(
            windows,
            vec![
                (d(1, 1), d(3, 25)),
                (d(3, 26), d(6, 18)),
                (d(6, 19), d(6, 30))
            ]
        );
        assert_eq!(Feed::all().len(), 8);
        assert_eq!(format!("{:?}", DartKey::new("secret")), "DartKey(****)");
    }
}
