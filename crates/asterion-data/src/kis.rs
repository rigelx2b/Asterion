//! Korea Investment & Securities (KIS) Open API: per-stock daily net buying by investor
//! type (individual, foreign, institution). The endpoint returns only a recent window
//! with no start-date parameter, so history is built by fetching every day.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fmt, fs};

use chrono::{FixedOffset, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::error::DataError;
use crate::http::{Http, RetryPolicy};
use crate::redact::{MASK, redact_secret};

pub const SOURCE_ID: &str = "kis_investor";
pub const BASE_URL: &str = "https://openapi.koreainvestment.com:9443";
pub const DEFAULT_ROOT: &str = "data/raw/kis_investor";
const TOKEN_PATH: &str = "/oauth2/tokenP";
const INVESTOR_PATH: &str = "/uapi/domestic-stock/v1/quotations/inquire-investor";
const INVESTOR_TR_ID: &str = "FHKST01010900";
// Real accounts allow 20 requests per second; stay below it.
pub const MIN_INTERVAL: Duration = Duration::from_millis(70);
// Same-day figures are final only after the close; before this KST hour today's row is dropped.
const FINAL_HOUR_KST: u32 = 18;
const RATE_LIMIT_CODE: &str = "EGW00201";
// Backend relay error seen in bursts under sustained chart requests; single retries succeed.
const BACKEND_BUSY_CODE: &str = "OPSQ0008";
// Waits before each retry of a busy-backend response.
const BUSY_BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 16];
const CHART_PATH: &str = "/uapi/domestic-stock/v1/quotations/inquire-daily-itemchartprice";
const CHART_TR_ID: &str = "FHKST03010100";
const INVESTOR_DAILY_PATH: &str =
    "/uapi/domestic-stock/v1/quotations/investor-trade-by-stock-daily";
const INVESTOR_DAILY_TR_ID: &str = "FHPTJ04160001";
const SHORT_SALE_DAILY_PATH: &str = "/uapi/domestic-stock/v1/quotations/daily-short-sale";
const SHORT_SALE_DAILY_TR_ID: &str = "FHPST04830000";
const LENDING_DAILY_PATH: &str = "/uapi/domestic-stock/v1/quotations/daily-loan-trans";
const LENDING_DAILY_TR_ID: &str = "HHPST074500C0";
// Read-only market-data endpoints; order and account APIs live under other prefixes.
pub const QUOTATIONS_PREFIX: &str = "/uapi/domestic-stock/v1/quotations/";
const INDEX_CHART_PATH: &str = "/uapi/domestic-stock/v1/quotations/inquire-daily-indexchartprice";
const INDEX_CHART_TR_ID: &str = "FHKUP03500100";
pub const INVESTOR_DAILY_ROOT: &str = "data/raw/kis_investor_daily";
pub const INDEX_HISTORY_ROOT: &str = "data/raw/kis_index";
pub const SHORT_SALE_ROOT: &str = "data/raw/kis_short_sale";
// The short-sale endpoint returns at most this many sessions per call, newest first.
const SHORT_SALE_PAGE_CAP: usize = 100;
pub const LENDING_ROOT: &str = "data/raw/kis_lending";
// The lending endpoint returns at most this many sessions per call, newest first.
const LENDING_PAGE_CAP: usize = 100;
// The index chart returns at most this many rows per call, newest first.
const INDEX_PAGE_CAP: usize = 50;
// Calendar days per index request; at most 44 sessions, under the 50-row cap.
const INDEX_WINDOW_DAYS: u64 = 60;
pub const HISTORY_ROOT: &str = "data/raw/kis_daily";
const OVERSEAS_PATH: &str = "/uapi/overseas-price/v1/quotations/dailyprice";
const OVERSEAS_TR_ID: &str = "HHDFS76240000";
pub const OVERSEAS_ROOT: &str = "data/raw/kis_overseas";
// Calendar days per chart request; about 95 sessions, under the 100-row page limit.
const HISTORY_WINDOW_DAYS: u64 = 140;

/// App key and secret. `Debug` never prints either value.
#[derive(Clone, PartialEq, Eq)]
pub struct KisKeys {
    app_key: String,
    app_secret: String,
}

impl KisKeys {
    pub const KEY_VAR: &'static str = "KIS_APP_KEY";
    pub const SECRET_VAR: &'static str = "KIS_APP_SECRET";

    pub fn new(app_key: &str, app_secret: &str) -> Self {
        KisKeys {
            app_key: app_key.trim().to_owned(),
            app_secret: app_secret.trim().to_owned(),
        }
    }

    pub fn from_env() -> Result<Self, DataError> {
        let read = |name: &'static str| match env::var(name) {
            Ok(v) if !v.trim().is_empty() => Ok(v),
            _ => Err(DataError::MissingServiceKey(name)),
        };
        Ok(Self::new(&read(Self::KEY_VAR)?, &read(Self::SECRET_VAR)?))
    }
}

impl fmt::Debug for KisKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KisKeys")
            .field("app_key", &MASK)
            .field("app_secret", &MASK)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error_code: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChartEnvelope {
    rt_cd: String,
    #[serde(default)]
    msg_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output2: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    rt_cd: String,
    #[serde(default)]
    msg_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output: Vec<serde_json::Value>,
}

pub struct KisClient {
    http: Http,
    keys: KisKeys,
    base: String,
    token: Option<String>,
}

impl fmt::Debug for KisClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KisClient")
            .field("http", &self.http)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl KisClient {
    pub fn new(keys: KisKeys, base: &str, policy: RetryPolicy) -> Result<Self, DataError> {
        // Errors are scrubbed of the secret; the app key travels in headers only.
        let http = Http::new(&keys.app_secret, policy)?;
        Ok(KisClient {
            http,
            keys,
            base: base.trim_end_matches('/').to_owned(),
            token: None,
        })
    }

    pub fn requests_sent(&self) -> u32 {
        self.http.requests_sent()
    }

    /// A second client on this client's token (issued now if needed), with its own
    /// throttle; KIS limits how often tokens may be issued, not how many share one.
    pub async fn fork(&mut self, policy: RetryPolicy) -> Result<Self, DataError> {
        let token = self.token().await?;
        let mut other = KisClient::new(self.keys.clone(), &self.base, policy)?;
        other.token = Some(token);
        Ok(other)
    }

    fn scrub(&self, text: &str) -> String {
        let text = redact_secret(text, &self.keys.app_secret);
        let text = redact_secret(&text, &self.keys.app_key);
        match &self.token {
            Some(t) => redact_secret(&text, t),
            None => text,
        }
    }

    // One token per run; KIS limits how often tokens may be issued.
    async fn token(&mut self) -> Result<String, DataError> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let url = format!("{}{TOKEN_PATH}", self.base);
        let body = serde_json::json!({
            "grant_type": "client_credentials",
            "appkey": self.keys.app_key,
            "appsecret": self.keys.app_secret,
        });
        let body = body.to_string();
        let text = self
            .http
            .send(|c| {
                c.post(&url)
                    .header("content-type", "application/json; charset=utf-8")
                    .body(body.clone())
            })
            .await?;
        let parsed: TokenResponse = serde_json::from_str(&text)?;
        match parsed.access_token {
            Some(t) if !t.is_empty() => {
                self.token = Some(t.clone());
                Ok(t)
            }
            _ => Err(DataError::Api {
                code: parsed.error_code.unwrap_or_default(),
                message: self.scrub(&parsed.error_description.unwrap_or_default()),
            }),
        }
    }

    /// Raw JSON body of the investor breakdown for one KRX short code.
    pub async fn fetch_investor(&mut self, code: &str) -> Result<String, DataError> {
        let token = self.token().await?;
        let url = format!("{}{INVESTOR_PATH}", self.base);
        let auth = format!("Bearer {token}");
        for attempt in 0..3 {
            let text = self
                .http
                .send(|c| {
                    c.get(&url)
                        .query(&[("FID_COND_MRKT_DIV_CODE", "J"), ("FID_INPUT_ISCD", code)])
                        .header("content-type", "application/json; charset=utf-8")
                        .header("authorization", &auth)
                        .header("appkey", &self.keys.app_key)
                        .header("appsecret", &self.keys.app_secret)
                        .header("tr_id", INVESTOR_TR_ID)
                        .header("custtype", "P")
                })
                .await?;
            let env: Envelope = serde_json::from_str(&text)?;
            if env.rt_cd == "0" {
                return Ok(text);
            }
            if env.msg_cd == RATE_LIMIT_CODE && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            return Err(DataError::Api {
                code: env.msg_cd,
                message: self.scrub(&env.msg1),
            });
        }
        unreachable!("the loop returns on its last attempt")
    }

    /// Raw JSON body of daily bars for one code between two dates, inclusive.
    /// `adjusted` is passed through as FID_ORG_ADJ_PRC.
    pub async fn fetch_chart(
        &mut self,
        code: &str,
        from: NaiveDate,
        to: NaiveDate,
        adjusted: &str,
    ) -> Result<String, DataError> {
        let d1 = from.format("%Y%m%d").to_string();
        let d2 = to.format("%Y%m%d").to_string();
        let query = [
            ("FID_COND_MRKT_DIV_CODE", "J"),
            ("FID_INPUT_ISCD", code),
            ("FID_INPUT_DATE_1", d1.as_str()),
            ("FID_INPUT_DATE_2", d2.as_str()),
            ("FID_PERIOD_DIV_CODE", "D"),
            ("FID_ORG_ADJ_PRC", adjusted),
        ];
        self.fetch_daily(CHART_PATH, CHART_TR_ID, &query).await
    }

    /// Raw JSON body of daily investor-type flows for one code, sessions up to `date`.
    pub async fn fetch_investor_daily(
        &mut self,
        code: &str,
        date: NaiveDate,
    ) -> Result<String, DataError> {
        let d = date.format("%Y%m%d").to_string();
        let query = [
            ("FID_COND_MRKT_DIV_CODE", "J"),
            ("FID_INPUT_ISCD", code),
            ("FID_INPUT_DATE_1", d.as_str()),
            ("FID_ORG_ADJ_PRC", ""),
            ("FID_ETC_CLS_CODE", ""),
        ];
        self.fetch_daily(INVESTOR_DAILY_PATH, INVESTOR_DAILY_TR_ID, &query)
            .await
    }

    /// Raw JSON body of daily short-sale trading (not balances) for one code between
    /// two dates, inclusive.
    pub async fn fetch_short_sale_daily(
        &mut self,
        code: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<String, DataError> {
        let d1 = from.format("%Y%m%d").to_string();
        let d2 = to.format("%Y%m%d").to_string();
        let query = [
            ("FID_COND_MRKT_DIV_CODE", "J"),
            ("FID_INPUT_ISCD", code),
            ("FID_INPUT_DATE_1", d1.as_str()),
            ("FID_INPUT_DATE_2", d2.as_str()),
        ];
        self.fetch_daily(SHORT_SALE_DAILY_PATH, SHORT_SALE_DAILY_TR_ID, &query)
            .await
    }

    /// Raw JSON body of daily index levels for one KIS sector code (`0001` KOSPI,
    /// `1001` KOSDAQ) between two dates, inclusive.
    pub async fn fetch_index_chart(
        &mut self,
        code: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<String, DataError> {
        let d1 = from.format("%Y%m%d").to_string();
        let d2 = to.format("%Y%m%d").to_string();
        let query = [
            ("FID_COND_MRKT_DIV_CODE", "U"),
            ("FID_INPUT_ISCD", code),
            ("FID_INPUT_DATE_1", d1.as_str()),
            ("FID_INPUT_DATE_2", d2.as_str()),
            ("FID_PERIOD_DIV_CODE", "D"),
        ];
        self.fetch_daily(INDEX_CHART_PATH, INDEX_CHART_TR_ID, &query)
            .await
    }

    /// Raw JSON body of daily stock-lending activity and balances for one code between
    /// two dates, inclusive (rows in `output1`).
    pub async fn fetch_lending_daily(
        &mut self,
        code: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<String, DataError> {
        let d1 = from.format("%Y%m%d").to_string();
        let d2 = to.format("%Y%m%d").to_string();
        let query = [
            ("MRKT_DIV_CLS_CODE", "3"),
            ("MKSC_SHRN_ISCD", code),
            ("START_DATE", d1.as_str()),
            ("END_DATE", d2.as_str()),
            ("CTS", ""),
        ];
        self.fetch_daily(LENDING_DAILY_PATH, LENDING_DAILY_TR_ID, &query)
            .await
    }

    /// Raw JSON body of any domestic-stock quotation endpoint (read-only market data).
    /// Paths outside the quotations prefix are refused, so no order or account API can be reached.
    pub async fn fetch_quotation(
        &mut self,
        path: &str,
        tr_id: &str,
        query: &[(&str, &str)],
    ) -> Result<String, DataError> {
        if !is_quotation_path(path) {
            return Err(DataError::InvalidField {
                field: "path",
                value: path.to_owned(),
            });
        }
        self.fetch_daily(path, tr_id, query).await
    }

    // Shared GET for the daily chart endpoints, retrying rate-limit and busy-backend replies.
    async fn fetch_daily(
        &mut self,
        path: &str,
        tr_id: &str,
        query: &[(&str, &str)],
    ) -> Result<String, DataError> {
        let token = self.token().await?;
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {token}");
        for attempt in 0..=BUSY_BACKOFF_SECS.len() {
            let text = self
                .http
                .send(|c| {
                    c.get(&url)
                        .query(query)
                        .header("content-type", "application/json; charset=utf-8")
                        .header("authorization", &auth)
                        .header("appkey", &self.keys.app_key)
                        .header("appsecret", &self.keys.app_secret)
                        .header("tr_id", tr_id)
                        .header("custtype", "P")
                })
                .await?;
            let env: ChartEnvelope = serde_json::from_str(&text)?;
            if env.rt_cd == "0" {
                return Ok(text);
            }
            if env.msg_cd == RATE_LIMIT_CODE && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            if env.msg_cd == BACKEND_BUSY_CODE
                && let Some(&wait) = BUSY_BACKOFF_SECS.get(attempt)
            {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            return Err(DataError::Api {
                code: env.msg_cd,
                message: self.scrub(&env.msg1),
            });
        }
        unreachable!("the loop returns on its last attempt")
    }
}

impl KisClient {
    /// Raw JSON body of up to 100 daily bars ending at `base` for one overseas listing.
    /// `exchange` is a KIS code such as NAS, NYS or AMS; `adjusted` is MODP ("0" or "1").
    pub async fn fetch_overseas(
        &mut self,
        exchange: &str,
        symbol: &str,
        base: NaiveDate,
        adjusted: &str,
    ) -> Result<String, DataError> {
        let token = self.token().await?;
        let url = format!("{}{OVERSEAS_PATH}", self.base);
        let auth = format!("Bearer {token}");
        let bymd = base.format("%Y%m%d").to_string();
        for attempt in 0..3 {
            let text = self
                .http
                .send(|c| {
                    c.get(&url)
                        .query(&[
                            ("AUTH", ""),
                            ("EXCD", exchange),
                            ("SYMB", symbol),
                            ("GUBN", "0"),
                            ("BYMD", bymd.as_str()),
                            ("MODP", adjusted),
                        ])
                        .header("content-type", "application/json; charset=utf-8")
                        .header("authorization", &auth)
                        .header("appkey", &self.keys.app_key)
                        .header("appsecret", &self.keys.app_secret)
                        .header("tr_id", OVERSEAS_TR_ID)
                        .header("custtype", "P")
                })
                .await?;
            let env: ChartEnvelope = serde_json::from_str(&text)?;
            if env.rt_cd == "0" {
                return Ok(text);
            }
            if env.msg_cd == RATE_LIMIT_CODE && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            return Err(DataError::Api {
                code: env.msg_cd,
                message: self.scrub(&env.msg1),
            });
        }
        unreachable!("the loop returns on its last attempt")
    }
}

/// Daily bars from the overseas endpoint; prices in the listing's currency.
pub fn parse_overseas(body: &str) -> Result<Vec<ChartBar>, DataError> {
    let env: ChartEnvelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output2 {
        let raw_date = row.get("xymd").and_then(|v| v.as_str()).unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "xymd",
                value: raw_date.to_owned(),
            })?;
        let close = number(row, "clos")?;
        if close <= 0.0 {
            continue;
        }
        out.push(ChartBar {
            date,
            open: number(row, "open")?,
            high: number(row, "high")?,
            low: number(row, "low")?,
            close,
            volume: number(row, "tvol")?,
            trading_value: number(row, "tamt")?,
        });
    }
    out.sort_by_key(|b| b.date);
    out.dedup_by_key(|b| b.date);
    Ok(out)
}

/// Pages back from `to` (each page ends the day before the previous page's first bar)
/// until `from` or an empty page. Bodies live at `<root>/<EXCD>_<SYMB>/<adjusted>/<base>.json`;
/// pages dated before `to` are reused. Returns (new requests, rows seen).
pub async fn collect_overseas(
    client: &mut KisClient,
    root: &Path,
    exchange: &str,
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
    adjusted: &str,
) -> Result<(usize, usize), DataError> {
    let dir = root.join(format!("{exchange}_{symbol}")).join(adjusted);
    let (mut requests, mut rows) = (0, 0);
    let mut base = to;
    loop {
        let path = dir.join(format!("{base}.json"));
        // The newest page is always refetched so a rerun picks up new sessions.
        let body = if path.exists() && base < to {
            fs::read_to_string(&path).map_err(|e| io(&path, e))?
        } else {
            let body = client
                .fetch_overseas(exchange, symbol, base, adjusted)
                .await?;
            requests += 1;
            fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
            body
        };
        let bars = parse_overseas(&body)?;
        rows += bars.len();
        let Some(first) = bars.first() else { break };
        if first.date <= from {
            break;
        }
        let Some(next) = first.date.pred_opt() else {
            break;
        };
        if next >= base {
            break;
        }
        base = next;
    }
    Ok((requests, rows))
}

/// Every cached overseas bar for one listing, sorted, one per date.
pub fn load_overseas(
    root: &Path,
    exchange: &str,
    symbol: &str,
    adjusted: &str,
) -> Result<Vec<ChartBar>, DataError> {
    let dir = root.join(format!("{exchange}_{symbol}")).join(adjusted);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.extend(parse_overseas(&text)?);
        }
    }
    out.sort_by_key(|b| b.date);
    out.dedup_by_key(|b| b.date);
    Ok(out)
}

/// One daily bar from the chart endpoint, prices as the request's adjustment flag gave them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChartBar {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub trading_value: f64,
}

pub fn parse_chart(body: &str) -> Result<Vec<ChartBar>, DataError> {
    let env: ChartEnvelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output2 {
        let raw_date = row
            .get("stck_bsop_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "stck_bsop_date",
                value: raw_date.to_owned(),
            })?;
        out.push(ChartBar {
            date,
            open: number(row, "stck_oprc")?,
            high: number(row, "stck_hgpr")?,
            low: number(row, "stck_lwpr")?,
            close: number(row, "stck_clpr")?,
            volume: number(row, "acml_vol")?,
            trading_value: number(row, "acml_tr_pbmn")?,
        });
    }
    out.sort_by_key(|b| b.date);
    out.dedup_by_key(|b| b.date);
    Ok(out)
}

/// Raw chart bodies live at `<root>/<code>/<adjusted>/<from>_<to>.json`.
pub fn chart_path(
    root: &Path,
    code: &str,
    adjusted: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> PathBuf {
    root.join(code)
        .join(adjusted)
        .join(format!("{from}_{to}.json"))
}

/// Walks back from `to` in fixed windows, storing each body once, until `from` or the
/// first empty window (before listing). With `skip_leading_empty`, empty windows before
/// the first bar is found (after a delisting) do not stop the walk.
/// Returns (new requests, total rows seen).
pub async fn collect_history(
    client: &mut KisClient,
    root: &Path,
    code: &str,
    from: NaiveDate,
    to: NaiveDate,
    adjusted: &str,
    skip_leading_empty: bool,
) -> Result<(usize, usize), DataError> {
    let (mut requests, mut rows) = (0, 0);
    let mut end = to;
    while end >= from {
        let start = end
            .checked_sub_days(chrono::Days::new(HISTORY_WINDOW_DAYS - 1))
            .map_or(from, |d| d.max(from));
        let path = chart_path(root, code, adjusted, start, end);
        let body = if path.exists() {
            fs::read_to_string(&path).map_err(|e| io(&path, e))?
        } else {
            let body = client.fetch_chart(code, start, end, adjusted).await?;
            requests += 1;
            let dir = path.parent().expect("chart path has a parent");
            fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
            body
        };
        let n = parse_chart(&body)?.len();
        rows += n;
        if n == 0 && !(skip_leading_empty && rows == 0) {
            break;
        }
        let Some(next) = start.pred_opt() else { break };
        end = next;
    }
    Ok((requests, rows))
}

pub fn parse_index_chart(body: &str) -> Result<Vec<ChartBar>, DataError> {
    let env: ChartEnvelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output2 {
        let raw_date = row
            .get("stck_bsop_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "stck_bsop_date",
                value: raw_date.to_owned(),
            })?;
        out.push(ChartBar {
            date,
            open: number(row, "bstp_nmix_oprc")?,
            high: number(row, "bstp_nmix_hgpr")?,
            low: number(row, "bstp_nmix_lwpr")?,
            close: number(row, "bstp_nmix_prpr")?,
            volume: number(row, "acml_vol")?,
            trading_value: number(row, "acml_tr_pbmn")?,
        });
    }
    out.sort_by_key(|b| b.date);
    out.dedup_by_key(|b| b.date);
    Ok(out)
}

/// Raw index chart bodies live at `<root>/<code>/<from>_<to>.json`.
pub fn index_chart_path(root: &Path, code: &str, from: NaiveDate, to: NaiveDate) -> PathBuf {
    root.join(code).join(format!("{from}_{to}.json"))
}

/// Walks back from `to` to `from` in fixed windows, storing each body once. A window that
/// fills the page cap may be truncated, so it is rejected instead of cached.
/// Returns (new requests, total rows seen).
pub async fn collect_index_history(
    client: &mut KisClient,
    root: &Path,
    code: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(usize, usize), DataError> {
    let (mut requests, mut rows) = (0, 0);
    let mut end = to;
    while end >= from {
        let start = end
            .checked_sub_days(chrono::Days::new(INDEX_WINDOW_DAYS - 1))
            .map_or(from, |d| d.max(from));
        let path = index_chart_path(root, code, start, end);
        let n = if path.exists() {
            let body = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            parse_index_chart(&body)?.len()
        } else {
            let body = client.fetch_index_chart(code, start, end).await?;
            requests += 1;
            let n = parse_index_chart(&body)?.len();
            if n >= INDEX_PAGE_CAP {
                return Err(DataError::InvalidField {
                    field: "output2",
                    value: format!(
                        "index {code} {start}..{end}: {n} rows hit cap, may truncate, uncached"
                    ),
                });
            }
            let dir = path.parent().expect("index path has a parent");
            fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
            n
        };
        rows += n;
        let Some(next) = start.pred_opt() else { break };
        end = next;
    }
    Ok((requests, rows))
}

/// One session of investor-type flows from the per-stock daily endpoint.
/// Net-buy values are in million KRW; `trading_value` is in KRW; quantities in shares.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InvestorDay {
    pub date: NaiveDate,
    pub foreign_net_qty: f64,
    pub institution_net_qty: f64,
    pub individual_net_qty: f64,
    pub foreign_net_value: f64,
    pub institution_net_value: f64,
    pub individual_net_value: f64,
    pub volume: f64,
    pub trading_value: f64,
}

pub fn parse_investor_daily(body: &str) -> Result<Vec<InvestorDay>, DataError> {
    let env: ChartEnvelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output2 {
        let raw_date = row
            .get("stck_bsop_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "stck_bsop_date",
                value: raw_date.to_owned(),
            })?;
        out.push(InvestorDay {
            date,
            foreign_net_qty: number(row, "frgn_ntby_qty")?,
            institution_net_qty: number(row, "orgn_ntby_qty")?,
            individual_net_qty: number(row, "prsn_ntby_qty")?,
            foreign_net_value: number(row, "frgn_ntby_tr_pbmn")?,
            institution_net_value: number(row, "orgn_ntby_tr_pbmn")?,
            individual_net_value: number(row, "prsn_ntby_tr_pbmn")?,
            volume: number(row, "acml_vol")?,
            trading_value: number(row, "acml_tr_pbmn")?,
        });
    }
    out.sort_by_key(|d| d.date);
    out.dedup_by_key(|d| d.date);
    Ok(out)
}

/// Walks back from `to` until `from` in pages of the 30 sessions ending at a request
/// date, storing each body once at `<root>/<code>/<end>.json`. The next end is the day
/// before the oldest session returned, so a resumed walk replays the cached chain.
pub async fn collect_investor_daily(
    client: &mut KisClient,
    root: &Path,
    code: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(usize, usize), DataError> {
    let (mut requests, mut rows) = (0, 0);
    let mut end = to;
    while end >= from {
        let path = root.join(code).join(format!("{end}.json"));
        let body = if path.exists() {
            fs::read_to_string(&path).map_err(|e| io(&path, e))?
        } else {
            let body = client.fetch_investor_daily(code, end).await?;
            requests += 1;
            let dir = path.parent().expect("investor path has a parent");
            fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
            body
        };
        let days = parse_investor_daily(&body)?;
        rows += days.len();
        let Some(oldest) = days.first().map(|d| d.date).filter(|d| *d <= end) else {
            break;
        };
        let Some(next) = oldest.pred_opt() else { break };
        end = next;
    }
    Ok((requests, rows))
}

/// One session of stock lending from the per-stock daily endpoint. A proxy for short
/// interest, not the short-sale balance. `balance_amount` is in million KRW.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LendingDay {
    pub date: NaiveDate,
    pub balance_shares: f64,
    pub balance_amount: f64,
    pub new_shares: f64,
    pub repaid_shares: f64,
    pub volume: f64,
    pub close: f64,
}

#[derive(Debug, Deserialize)]
struct LendingEnvelope {
    #[serde(default)]
    output1: Vec<serde_json::Value>,
}

pub fn parse_lending_daily(body: &str) -> Result<Vec<LendingDay>, DataError> {
    let env: LendingEnvelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output1 {
        let raw_date = row.get("bsop_date").and_then(|v| v.as_str()).unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "bsop_date",
                value: raw_date.to_owned(),
            })?;
        out.push(LendingDay {
            date,
            balance_shares: number(row, "rmnd_stcn")?,
            balance_amount: number(row, "rmnd_amt")?,
            new_shares: number(row, "new_stcn")?,
            repaid_shares: number(row, "rdmp_stcn")?,
            volume: number(row, "acml_vol")?,
            close: number(row, "stck_prpr")?,
        });
    }
    out.sort_by_key(|d| d.date);
    out.dedup_by_key(|d| d.date);
    Ok(out)
}

/// Walks back from `to` to `from` in pages of at most 100 sessions within
/// `[from, end]`, storing each body once at `<root>/<code>/<end>.json`. A full page
/// continues from the day before its oldest session; a short page ends the walk.
pub async fn collect_lending_daily(
    client: &mut KisClient,
    root: &Path,
    code: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(usize, usize), DataError> {
    let (mut requests, mut rows) = (0, 0);
    let mut end = to;
    while end >= from {
        let path = root.join(code).join(format!("{end}.json"));
        let body = if path.exists() {
            fs::read_to_string(&path).map_err(|e| io(&path, e))?
        } else {
            let body = client.fetch_lending_daily(code, from, end).await?;
            requests += 1;
            let dir = path.parent().expect("lending path has a parent");
            fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
            body
        };
        let days = parse_lending_daily(&body)?;
        rows += days.len();
        if days.len() < LENDING_PAGE_CAP {
            break;
        }
        let Some(oldest) = days.first().map(|d| d.date).filter(|d| *d <= end) else {
            break;
        };
        let Some(next) = oldest.pred_opt() else { break };
        end = next;
    }
    Ok((requests, rows))
}

/// Every cached lending day for `code`, sorted, one per date.
pub fn load_lending_daily(root: &Path, code: &str) -> Result<Vec<LendingDay>, DataError> {
    let dir = root.join(code);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.extend(parse_lending_daily(&text)?);
        }
    }
    out.sort_by_key(|d| d.date);
    out.dedup_by_key(|d| d.date);
    Ok(out)
}

/// True only for paths under the read-only quotations prefix.
pub fn is_quotation_path(path: &str) -> bool {
    path.starts_with(QUOTATIONS_PREFIX) && !path.contains("..") && !path.contains('?')
}

/// One session of short-sale trading (not balances) from the per-stock daily endpoint.
/// `short_value` and `trading_value` are in KRW; quantities in shares.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShortSaleDay {
    pub date: NaiveDate,
    pub short_qty: f64,
    pub short_value: f64,
    pub volume: f64,
    pub trading_value: f64,
}

pub fn parse_short_sale_daily(body: &str) -> Result<Vec<ShortSaleDay>, DataError> {
    let env: ChartEnvelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output2 {
        let raw_date = row
            .get("stck_bsop_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "stck_bsop_date",
                value: raw_date.to_owned(),
            })?;
        out.push(ShortSaleDay {
            date,
            short_qty: number(row, "ssts_cntg_qty")?,
            short_value: number(row, "ssts_tr_pbmn")?,
            volume: number(row, "acml_vol")?,
            trading_value: number(row, "acml_tr_pbmn")?,
        });
    }
    out.sort_by_key(|d| d.date);
    out.dedup_by_key(|d| d.date);
    Ok(out)
}

/// Walks back from `to` to `from` in pages of at most 100 sessions within
/// `[from, end]`, storing each body once at `<root>/<code>/<end>.json`. A full page
/// continues from the day before its oldest session; a short page ends the walk.
pub async fn collect_short_sale_daily(
    client: &mut KisClient,
    root: &Path,
    code: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(usize, usize), DataError> {
    let (mut requests, mut rows) = (0, 0);
    let mut end = to;
    while end >= from {
        let path = root.join(code).join(format!("{end}.json"));
        let body = if path.exists() {
            fs::read_to_string(&path).map_err(|e| io(&path, e))?
        } else {
            let body = client.fetch_short_sale_daily(code, from, end).await?;
            requests += 1;
            let dir = path.parent().expect("short-sale path has a parent");
            fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
            body
        };
        let days = parse_short_sale_daily(&body)?;
        rows += days.len();
        if days.len() < SHORT_SALE_PAGE_CAP {
            break;
        }
        let Some(oldest) = days.first().map(|d| d.date).filter(|d| *d <= end) else {
            break;
        };
        let Some(next) = oldest.pred_opt() else { break };
        end = next;
    }
    Ok((requests, rows))
}

/// Every cached short-sale day for `code`, sorted, one per date.
pub fn load_short_sale_daily(root: &Path, code: &str) -> Result<Vec<ShortSaleDay>, DataError> {
    let dir = root.join(code);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.extend(parse_short_sale_daily(&text)?);
        }
    }
    out.sort_by_key(|d| d.date);
    out.dedup_by_key(|d| d.date);
    Ok(out)
}

/// Every cached investor day for `code`, sorted, one per date.
pub fn load_investor_daily(root: &Path, code: &str) -> Result<Vec<InvestorDay>, DataError> {
    let dir = root.join(code);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.extend(parse_investor_daily(&text)?);
        }
    }
    out.sort_by_key(|d| d.date);
    out.dedup_by_key(|d| d.date);
    Ok(out)
}

/// Every cached index bar for `code`, sorted, one per date.
pub fn load_index_history(root: &Path, code: &str) -> Result<Vec<ChartBar>, DataError> {
    let dir = root.join(code);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.extend(parse_index_chart(&text)?);
        }
    }
    out.sort_by_key(|b| b.date);
    out.dedup_by_key(|b| b.date);
    Ok(out)
}

/// True once `collect_history` has stored at least one window for `code`, even an empty one.
pub fn history_cached(root: &Path, code: &str, adjusted: &str) -> bool {
    root.join(code).join(adjusted).is_dir()
}

/// Every cached bar for `code` under one adjustment flag, sorted, one per date.
pub fn load_history(root: &Path, code: &str, adjusted: &str) -> Result<Vec<ChartBar>, DataError> {
    let dir = root.join(code).join(adjusted);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.extend(parse_chart(&text)?);
        }
    }
    out.sort_by_key(|b| b.date);
    out.dedup_by_key(|b| b.date);
    Ok(out)
}

/// One session of investor-type flows. Values are as the API reports them
/// (shares for `*_qty`, the API's KRW unit for `*_value`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InvestorFlow {
    pub date: NaiveDate,
    pub close: f64,
    pub individual_net_qty: f64,
    pub foreign_net_qty: f64,
    pub institution_net_qty: f64,
    pub individual_net_value: f64,
    pub foreign_net_value: f64,
    pub institution_net_value: f64,
}

fn number(row: &serde_json::Value, field: &'static str) -> Result<f64, DataError> {
    let raw = row.get(field).and_then(|v| v.as_str()).unwrap_or("");
    if raw.trim().is_empty() {
        return Ok(0.0);
    }
    raw.trim()
        .parse::<f64>()
        .map_err(|_| DataError::InvalidField {
            field,
            value: raw.to_owned(),
        })
}

pub fn parse_investor(body: &str) -> Result<Vec<InvestorFlow>, DataError> {
    let env: Envelope = serde_json::from_str(body)?;
    let mut out = Vec::new();
    for row in &env.output {
        let raw_date = row
            .get("stck_bsop_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if raw_date.is_empty() {
            continue;
        }
        let date =
            NaiveDate::parse_from_str(raw_date, "%Y%m%d").map_err(|_| DataError::InvalidField {
                field: "stck_bsop_date",
                value: raw_date.to_owned(),
            })?;
        out.push(InvestorFlow {
            date,
            close: number(row, "stck_clpr")?,
            individual_net_qty: number(row, "prsn_ntby_qty")?,
            foreign_net_qty: number(row, "frgn_ntby_qty")?,
            institution_net_qty: number(row, "orgn_ntby_qty")?,
            individual_net_value: number(row, "prsn_ntby_tr_pbmn")?,
            foreign_net_value: number(row, "frgn_ntby_tr_pbmn")?,
            institution_net_value: number(row, "orgn_ntby_tr_pbmn")?,
        });
    }
    out.sort_by_key(|f| f.date);
    Ok(out)
}

fn io(path: &Path, source: std::io::Error) -> DataError {
    DataError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn kst_now() -> chrono::DateTime<FixedOffset> {
    Utc::now().with_timezone(&FixedOffset::east_opt(9 * 3600).expect("valid offset"))
}

/// Raw bodies live at `<root>/<code>/<fetch date>.json`, one per day.
pub fn body_path(root: &Path, code: &str, fetched: NaiveDate) -> PathBuf {
    root.join(code).join(format!("{fetched}.json"))
}

pub fn fetched_today(root: &Path, code: &str) -> bool {
    body_path(root, code, kst_now().date_naive()).exists()
}

/// Fetches one code and stores the raw body unless today's body already exists.
/// Returns the number of final sessions in the body.
pub async fn collect_code(
    client: &mut KisClient,
    root: &Path,
    code: &str,
) -> Result<Option<usize>, DataError> {
    let now = kst_now();
    let path = body_path(root, code, now.date_naive());
    if path.exists() {
        return Ok(None);
    }
    let body = client.fetch_investor(code).await?;
    let rows = parse_investor(&body)?;
    let dir = path.parent().expect("body path has a parent");
    fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
    fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
    let today = now.date_naive();
    let final_rows = rows
        .iter()
        .filter(|r| r.date < today || now.hour() >= FINAL_HOUR_KST)
        .count();
    Ok(Some(final_rows))
}

/// Every final session cached for `code`, newest fetch winning on a date. A row dated on
/// its own fetch day counts only when fetched after the close.
pub fn load_flows(root: &Path, code: &str) -> Result<Vec<InvestorFlow>, DataError> {
    let dir = root.join(code);
    let Ok(read) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut bodies: Vec<(NaiveDate, PathBuf)> = Vec::new();
    for item in read {
        let path = item.map_err(|e| io(&dir, e))?.path();
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        let Some(Ok(day)) = stem.map(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d")) else {
            continue;
        };
        if path.extension().is_some_and(|e| e == "json") {
            bodies.push((day, path));
        }
    }
    bodies.sort();
    let mut by_date = std::collections::BTreeMap::new();
    for (fetched, path) in bodies {
        let body = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let modified_after_close = fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|t| {
                chrono::DateTime::<Utc>::from(t)
                    .with_timezone(&FixedOffset::east_opt(9 * 3600).expect("valid offset"))
                    .hour()
                    >= FINAL_HOUR_KST
            })
            .unwrap_or(false);
        for row in parse_investor(&body)? {
            if row.date < fetched || (row.date == fetched && modified_after_close) {
                by_date.insert(row.date, row);
            }
        }
    }
    Ok(by_date.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output":[
        {"stck_bsop_date":"20261111","stck_clpr":"205900","prsn_ntby_qty":"-1200","frgn_ntby_qty":"900","orgn_ntby_qty":"300","prsn_ntby_tr_pbmn":"-247","frgn_ntby_tr_pbmn":"185","orgn_ntby_tr_pbmn":"62"},
        {"stck_bsop_date":"20261110","stck_clpr":"204000","prsn_ntby_qty":"","frgn_ntby_qty":"-5","orgn_ntby_qty":"5","prsn_ntby_tr_pbmn":"0","frgn_ntby_tr_pbmn":"-1","orgn_ntby_tr_pbmn":"1"}
    ]}"#;

    #[test]
    fn parses_daily_investor_pages_oldest_first() {
        let body = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[
            {"stck_bsop_date":"20150130","frgn_ntby_qty":"-24649","orgn_ntby_qty":"28450","prsn_ntby_qty":"-3801","frgn_ntby_tr_pbmn":"-33681","orgn_ntby_tr_pbmn":"38900","prsn_ntby_tr_pbmn":"-5219","acml_vol":"16110","acml_tr_pbmn":"440683045910"},
            {"stck_bsop_date":"20150129","frgn_ntby_qty":"1","orgn_ntby_qty":"","prsn_ntby_qty":"-1","frgn_ntby_tr_pbmn":"2","orgn_ntby_tr_pbmn":"0","prsn_ntby_tr_pbmn":"-2","acml_vol":"10","acml_tr_pbmn":"1000"}
        ]}"#;
        let days = parse_investor_daily(body).expect("parses");
        assert_eq!(days.len(), 2);
        assert_eq!(
            days[0].date,
            NaiveDate::from_ymd_opt(2015, 1, 29).expect("date")
        );
        assert_eq!(days[0].institution_net_qty, 0.0);
        assert_eq!(days[1].foreign_net_value, -33681.0);
        assert_eq!(days[1].trading_value, 440_683_045_910.0);
    }

    #[test]
    fn quotation_probe_refuses_order_and_account_paths() {
        assert!(is_quotation_path(
            "/uapi/domestic-stock/v1/quotations/daily-loan-trans"
        ));
        assert!(!is_quotation_path(
            "/uapi/domestic-stock/v1/trading/order-cash"
        ));
        assert!(!is_quotation_path(
            "/uapi/domestic-stock/v1/trading/inquire-balance"
        ));
        assert!(!is_quotation_path(
            "/uapi/domestic-stock/v1/quotations/../trading/order-cash"
        ));
    }

    #[test]
    fn parses_daily_lending_pages_oldest_first() {
        let body = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":[
            {"bsop_date":"20241230","stck_prpr":"53200.00","acml_vol":"12624702","new_stcn":"482242","rdmp_stcn":"2360106","rmnd_stcn":"121308503","rmnd_amt":"6453612"},
            {"bsop_date":"20241227","stck_prpr":"53700.00","acml_vol":"0","new_stcn":"0","rdmp_stcn":"","rmnd_stcn":"3750","rmnd_amt":"0"}
        ],"output2":[]}"#;
        let days = parse_lending_daily(body).expect("parses");
        assert_eq!(days.len(), 2);
        assert_eq!(
            days[0].date,
            NaiveDate::from_ymd_opt(2024, 12, 27).expect("date")
        );
        assert_eq!(days[0].repaid_shares, 0.0);
        assert_eq!(days[1].balance_amount, 6_453_612.0);
        assert_eq!(days[1].close, 53_200.0);
        assert_eq!(days[1].new_shares, 482_242.0);
    }

    #[test]
    fn parses_daily_short_sale_pages_oldest_first() {
        let body = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[
            {"stck_bsop_date":"20221229","ssts_cntg_qty":"54834","ssts_tr_pbmn":"3061797800","ssts_vol_rlim":"0.49","acml_vol":"11295935","acml_tr_pbmn":"628653753150"},
            {"stck_bsop_date":"20221228","ssts_cntg_qty":"0","ssts_tr_pbmn":"","ssts_vol_rlim":"0.00","acml_vol":"0","acml_tr_pbmn":"0"}
        ]}"#;
        let days = parse_short_sale_daily(body).expect("parses");
        assert_eq!(days.len(), 2);
        assert_eq!(
            days[0].date,
            NaiveDate::from_ymd_opt(2022, 12, 28).expect("date")
        );
        assert_eq!(days[0].short_value, 0.0);
        assert_eq!(days[1].short_qty, 54_834.0);
        assert_eq!(days[1].short_value, 3_061_797_800.0);
        assert_eq!(days[1].trading_value, 628_653_753_150.0);
    }

    #[test]
    fn parses_rows_oldest_first_and_blank_as_zero() {
        let rows = parse_investor(BODY).expect("parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].date,
            NaiveDate::from_ymd_opt(2026, 11, 10).expect("date")
        );
        assert_eq!(rows[0].individual_net_qty, 0.0);
        assert_eq!(rows[1].foreign_net_qty, 900.0);
        assert_eq!(rows[1].individual_net_value, -247.0);
    }

    #[test]
    fn error_envelope_has_no_rows() {
        let body = r#"{"rt_cd":"1","msg_cd":"EGW00123","msg1":"expired"}"#;
        assert!(parse_investor(body).expect("parses").is_empty());
    }

    #[test]
    fn keys_debug_is_masked() {
        let k = KisKeys::new("abc", "def");
        let shown = format!("{k:?}");
        assert!(!shown.contains("abc") && !shown.contains("def"));
    }

    #[test]
    fn load_keeps_only_sessions_before_the_fetch_day() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = body_path(
            dir.path(),
            "005930",
            NaiveDate::from_ymd_opt(2026, 11, 11).expect("date"),
        );
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, BODY).expect("write");
        let rows = load_flows(dir.path(), "005930").expect("loads");
        // The 11-11 row was fetched on 11-11; it stays only if the file was written after 18:00 KST.
        assert!(
            rows.iter()
                .any(|r| r.date == NaiveDate::from_ymd_opt(2026, 11, 10).expect("date"))
        );
    }

    #[tokio::test]
    async fn busy_backend_chart_responses_are_retried() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"t"}"#))
            .mount(&server)
            .await;
        let busy = r#"{"rt_cd":"1","msg_cd":"OPSQ0008","msg1":"busy","output2":[]}"#;
        Mock::given(method("GET"))
            .and(path(CHART_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(busy))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let ok = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[]}"#;
        Mock::given(method("GET"))
            .and(path(CHART_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(ok))
            .mount(&server)
            .await;
        let keys = KisKeys {
            app_key: "k".to_owned(),
            app_secret: "s".to_owned(),
        };
        let mut client =
            KisClient::new(keys, &server.uri(), RetryPolicy::default()).expect("client");
        let day = NaiveDate::from_ymd_opt(2019, 1, 2).expect("date");
        let body = client
            .fetch_chart("000001", day, day, "0")
            .await
            .expect("retried after a busy response");
        assert_eq!(body, ok);
    }

    #[test]
    fn index_chart_body_parses_levels() {
        let body = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{"bstp_nmix_prpr":"1600.18"},"output2":[
            {"stck_bsop_date":"20100520","bstp_nmix_prpr":"1600.18","bstp_nmix_oprc":"1624.17","bstp_nmix_hgpr":"1640.47","bstp_nmix_lwpr":"1591.93","acml_vol":"358176","acml_tr_pbmn":"6560697","mod_yn":"N"},
            {"stck_bsop_date":"20100519","bstp_nmix_prpr":"1630.50","bstp_nmix_oprc":"1640.00","bstp_nmix_hgpr":"1641.00","bstp_nmix_lwpr":"1620.00","acml_vol":"1","acml_tr_pbmn":"2","mod_yn":"N"},
            {}]}"#;
        let bars = parse_index_chart(body).expect("parses");
        assert_eq!(bars.len(), 2);
        assert!(bars[0].date < bars[1].date);
        assert!((bars[1].close - 1600.18).abs() < 1e-9);
        assert!((bars[0].open - 1640.0).abs() < 1e-9);
    }

    fn index_rows(n: usize) -> String {
        let start = NaiveDate::from_ymd_opt(2010, 1, 1).expect("date");
        let rows: Vec<String> = (0..n)
            .map(|i| {
                let d = start + chrono::Days::new(i as u64);
                format!(
                    r#"{{"stck_bsop_date":"{}","bstp_nmix_prpr":"1000.0"}}"#,
                    d.format("%Y%m%d")
                )
            })
            .collect();
        format!(
            r#"{{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{{}},"output2":[{}]}}"#,
            rows.join(",")
        )
    }

    async fn index_server(body: String) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"t"}"#))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(INDEX_CHART_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    fn test_client(server: &wiremock::MockServer) -> KisClient {
        let keys = KisKeys {
            app_key: "k".to_owned(),
            app_secret: "s".to_owned(),
        };
        KisClient::new(keys, &server.uri(), RetryPolicy::default()).expect("client")
    }

    #[tokio::test]
    async fn index_window_at_page_cap_is_rejected_and_not_cached() {
        let server = index_server(index_rows(INDEX_PAGE_CAP)).await;
        let mut client = test_client(&server);
        let dir = tempfile::tempdir().expect("tempdir");
        let day = NaiveDate::from_ymd_opt(2010, 3, 1).expect("date");
        let err = collect_index_history(&mut client, dir.path(), "0001", day, day)
            .await
            .expect_err("a full page may be truncated");
        assert!(matches!(
            err,
            DataError::InvalidField {
                field: "output2",
                ..
            }
        ));
        assert!(!dir.path().join("0001").exists());
    }

    #[tokio::test]
    async fn index_windows_are_cached_and_reused() {
        let server = index_server(index_rows(3)).await;
        let mut client = test_client(&server);
        let dir = tempfile::tempdir().expect("tempdir");
        let from = NaiveDate::from_ymd_opt(2010, 1, 1).expect("date");
        let to = NaiveDate::from_ymd_opt(2010, 4, 30).expect("date");
        let (requests, _) = collect_index_history(&mut client, dir.path(), "0001", from, to)
            .await
            .expect("collects");
        assert_eq!(requests, 2, "120 days in 60-day windows");
        let (again, _) = collect_index_history(&mut client, dir.path(), "0001", from, to)
            .await
            .expect("reuses");
        assert_eq!(again, 0);
        assert_eq!(
            load_index_history(dir.path(), "0001").expect("loads").len(),
            3
        );
    }

    #[tokio::test]
    async fn leading_empty_windows_are_skipped_only_when_asked() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"t"}"#))
            .mount(&server)
            .await;
        // Only the window ending 2019-08-13 (second from the end) holds a bar.
        let bar = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[{"stck_bsop_date":"20190801","stck_clpr":"100","stck_oprc":"100","stck_hgpr":"100","stck_lwpr":"100","acml_vol":"1","acml_tr_pbmn":"100"}]}"#;
        Mock::given(method("GET"))
            .and(path(CHART_PATH))
            .and(query_param("FID_INPUT_DATE_2", "20190813"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bar))
            .mount(&server)
            .await;
        let empty = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[]}"#;
        Mock::given(method("GET"))
            .and(path(CHART_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(empty))
            .mount(&server)
            .await;
        let from = NaiveDate::from_ymd_opt(2019, 1, 1).expect("date");
        let to = NaiveDate::from_ymd_opt(2019, 12, 31).expect("date");

        let mut client = test_client(&server);
        let dir = tempfile::tempdir().expect("tempdir");
        let (requests, rows) =
            collect_history(&mut client, dir.path(), "000001", from, to, "0", false)
                .await
                .expect("collects");
        assert_eq!((requests, rows), (1, 0), "stops at the first empty window");

        let dir = tempfile::tempdir().expect("tempdir");
        let (requests, rows) =
            collect_history(&mut client, dir.path(), "000001", from, to, "0", true)
                .await
                .expect("collects");
        assert_eq!(
            (requests, rows),
            (3, 1),
            "skips the leading empty window, stops after data"
        );
    }

    #[test]
    fn chart_body_parses_and_sorts() {
        let body = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[
            {"stck_bsop_date":"20190103","stck_clpr":"100","stck_oprc":"98","stck_hgpr":"101","stck_lwpr":"97","acml_vol":"10","acml_tr_pbmn":"1000"},
            {"stck_bsop_date":"20190102","stck_clpr":"98","stck_oprc":"99","stck_hgpr":"99","stck_lwpr":"96","acml_vol":"5","acml_tr_pbmn":"490"},
            {}]}"#;
        let bars = parse_chart(body).expect("parses");
        assert_eq!(bars.len(), 2);
        assert!(bars[0].date < bars[1].date);
        assert!((bars[1].close - 100.0).abs() < 1e-12);
        assert!((bars[0].trading_value - 490.0).abs() < 1e-12);
    }

    #[test]
    fn overseas_body_parses_and_skips_blank_rows() {
        let body = r#"{"rt_cd":"0","msg_cd":"MCA00000","msg1":"ok","output1":{},"output2":[
            {"xymd":"20240103","clos":"481.68","open":"490.0","high":"492.1","low":"475.1","tvol":"100","tamt":"48000"},
            {"xymd":"20240102","clos":"481.68","open":"","high":"","low":"","tvol":"","tamt":""},
            {"xymd":"","clos":"0"}]}"#;
        let bars = parse_overseas(body).expect("parses");
        assert_eq!(bars.len(), 2);
        assert_eq!(
            bars[0].date,
            NaiveDate::from_ymd_opt(2024, 1, 2).expect("date")
        );
        assert!((bars[1].close - 481.68).abs() < 1e-9);
    }
}
