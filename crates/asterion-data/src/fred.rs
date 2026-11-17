//! FRED (Federal Reserve Bank of St. Louis) daily series observations.

use std::{env, fmt};

use chrono::{NaiveDate, Utc};
use serde::Deserialize;

use crate::cache::{FetchedRange, RawCache};
use crate::data_go_kr::{CollectOutcome, missing_ranges};
use crate::error::DataError;
use crate::http::{Http, RetryPolicy};
use crate::redact::MASK;

pub const SOURCE_ID: &str = "fred";
pub const ENDPOINT: &str = "https://api.stlouisfed.org/fred/series/observations";
pub const DEFAULT_ROOT: &str = "data/raw/fred";
// Notice required by the FRED API terms of use.
pub const ATTRIBUTION: &str = "This product uses the FRED® API but is not endorsed or certified by the Federal Reserve Bank of St. Louis.";

pub const SP500: &str = "SP500";
pub const NASDAQ: &str = "NASDAQCOM";
pub const VIX: &str = "VIXCLS";
pub const US10Y: &str = "DGS10";
// South Korean won per US dollar.
pub const USDKRW: &str = "DEXKOUS";
pub const SERIES: [&str; 5] = [SP500, NASDAQ, VIX, US10Y, USDKRW];

pub const WTI: &str = "DCOILWTICO";
pub const DOLLAR_BROAD: &str = "DTWEXBGS";
pub const USDJPY: &str = "DEXJPUS";
pub const USDCNY: &str = "DEXCHUS";
pub const US2Y: &str = "DGS2";
pub const TERM_SPREAD: &str = "T10Y2Y";
pub const HY_SPREAD: &str = "BAMLH0A0HYM2";
pub const BAA_YIELD: &str = "DBAA";
pub const NASDAQ_VOL: &str = "VXNCLS";
pub const BREAKEVEN_10Y: &str = "T10YIE";

// Daily series beyond the core set. Every entry is one finite backfill, and the API has
// no daily quota (120 requests per minute), so a run over all of them always terminates.
pub const EXTRA_SERIES: [&str; 24] = [
    WTI,
    "DCOILBRENTEU",
    "DHHNGSP",
    DOLLAR_BROAD,
    USDJPY,
    USDCNY,
    "DEXUSEU",
    "DEXTAUS",
    "DGS3MO",
    US2Y,
    "DGS5",
    "DGS30",
    TERM_SPREAD,
    "T10Y3M",
    "DFF",
    BREAKEVEN_10Y,
    "T5YIE",
    HY_SPREAD,
    "BAMLC0A0CM",
    BAA_YIELD,
    "DJIA",
    "NASDAQ100",
    NASDAQ_VOL,
    "OVXCLS",
];

pub fn all_series() -> impl Iterator<Item = &'static str> {
    SERIES.into_iter().chain(EXTRA_SERIES)
}

#[derive(Clone, PartialEq, Eq)]
pub struct FredKey(String);

impl FredKey {
    pub const ENV_VAR: &'static str = "FRED_API_KEY";

    pub fn new(raw: &str) -> Self {
        FredKey(raw.trim().to_owned())
    }

    pub fn from_env() -> Result<Self, DataError> {
        match env::var(Self::ENV_VAR) {
            Ok(value) if !value.trim().is_empty() => Ok(Self::new(&value)),
            _ => Err(DataError::MissingServiceKey(Self::ENV_VAR)),
        }
    }
}

impl fmt::Debug for FredKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FredKey({MASK})")
    }
}

#[derive(Debug)]
pub struct FredClient {
    http: Http,
    key: FredKey,
    endpoint: String,
}

impl FredClient {
    pub fn new(key: FredKey, endpoint: &str, policy: RetryPolicy) -> Result<Self, DataError> {
        let http = Http::new(&key.0, policy)?;
        Ok(FredClient {
            http,
            key,
            endpoint: endpoint.to_owned(),
        })
    }

    pub fn requests_sent(&self) -> u32 {
        self.http.requests_sent()
    }

    // Raw JSON body for one series over an inclusive date range.
    pub async fn fetch_observations(
        &mut self,
        series_id: &str,
        begin: NaiveDate,
        end: NaiveDate,
    ) -> Result<String, DataError> {
        let begin = begin.format("%Y-%m-%d").to_string();
        let end = end.format("%Y-%m-%d").to_string();
        let endpoint = self.endpoint.clone();
        self.http
            .get(
                &endpoint,
                &[
                    ("series_id", series_id),
                    ("api_key", self.key.0.as_str()),
                    ("file_type", "json"),
                    ("observation_start", begin.as_str()),
                    ("observation_end", end.as_str()),
                ],
            )
            .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub date: NaiveDate,
    pub value: f64,
}

#[derive(Deserialize)]
struct RawBody {
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    observations: Vec<RawObservation>,
}

#[derive(Deserialize)]
struct RawObservation {
    date: String,
    value: String,
}

// FRED marks days without a value (holidays) with ".", which are skipped.
pub fn parse_observations(body: &str) -> Result<Vec<Observation>, DataError> {
    let raw: RawBody = serde_json::from_str(body.trim_start_matches('\u{feff}'))?;
    if raw.error_code.is_some() || raw.error_message.is_some() {
        return Err(DataError::Api {
            code: raw
                .error_code
                .map_or_else(|| "fred".to_owned(), |c| c.to_string()),
            message: raw.error_message.unwrap_or_default(),
        });
    }

    let mut observations = Vec::with_capacity(raw.observations.len());
    for obs in raw.observations {
        let value = obs.value.trim();
        if value == "." || value.is_empty() {
            continue;
        }
        let date = NaiveDate::parse_from_str(obs.date.trim(), "%Y-%m-%d").map_err(|_| {
            DataError::InvalidField {
                field: "date",
                value: obs.date.clone(),
            }
        })?;
        let value = value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| DataError::InvalidField {
                field: "value",
                value: obs.value.clone(),
            })?;
        observations.push(Observation { date, value });
    }
    Ok(observations)
}

// Fetches the uncached part of `[begin, end]`. A range is recorded only up to its
// last real observation, so not-yet-published days are requested again next run.
pub async fn collect_series(
    client: &mut FredClient,
    cache: &RawCache,
    series_id: &str,
    begin: NaiveDate,
    end: NaiveDate,
) -> Result<CollectOutcome, DataError> {
    let mut outcome = CollectOutcome::default();
    if begin > end {
        return Ok(outcome);
    }

    let mut manifest = cache.load_manifest_for(series_id)?;
    for (range_begin, range_end) in missing_ranges(&manifest.covered(), begin, end) {
        let body = client
            .fetch_observations(series_id, range_begin, range_end)
            .await?;
        let dates: Vec<NaiveDate> = parse_observations(&body)?
            .iter()
            .map(|o| o.date)
            .filter(|d| *d >= range_begin && *d <= range_end)
            .collect();
        let Some(covered_end) = dates.iter().copied().max() else {
            continue;
        };

        let page = cache.store_page_for(
            series_id,
            range_begin,
            range_end,
            1,
            &body,
            dates.len(),
            Utc::now(),
        )?;
        manifest.ranges.push(FetchedRange {
            begin: range_begin,
            end: covered_end,
            pages: vec![page],
        });
        cache.save_manifest_for(series_id, &manifest)?;
        outcome.ranges_fetched += 1;
        outcome.pages += 1;
        outcome.rows += dates.len();
    }
    Ok(outcome)
}

// Cached observations of one series, sorted by date with duplicate dates removed.
pub fn load_series(cache: &RawCache, series_id: &str) -> Result<Vec<Observation>, DataError> {
    let mut observations = Vec::new();
    for cached in cache.load_bodies(series_id)? {
        observations.extend(
            parse_observations(&cached.body)?
                .into_iter()
                .filter(|o| o.date >= cached.begin && o.date <= cached.end),
        );
    }
    observations.sort_by_key(|o| o.date);
    observations.dedup_by_key(|o| o.date);
    Ok(observations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_missing_values_and_parses_the_rest() {
        let body = r#"{"realtime_start":"2026-11-11","count":3,"observations":[
            {"realtime_start":"2026-11-11","realtime_end":"2026-11-11","date":"2024-01-01","value":"."},
            {"realtime_start":"2026-11-11","realtime_end":"2026-11-11","date":"2024-01-02","value":"4742.83"},
            {"realtime_start":"2026-11-11","realtime_end":"2026-11-11","date":"2024-01-03","value":"4704.81"}
        ]}"#;
        let observations = parse_observations(body).expect("parse");
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations[0].date,
            NaiveDate::from_ymd_opt(2024, 1, 2).expect("date")
        );
        assert_eq!(observations[1].value, 4704.81);
    }

    #[test]
    fn error_body_maps_to_api_error() {
        let body =
            r#"{"error_code":400,"error_message":"Bad Request.  The series does not exist."}"#;
        match parse_observations(body) {
            Err(DataError::Api { code, message }) => {
                assert_eq!(code, "400");
                assert!(message.contains("series does not exist"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn debug_hides_key() {
        assert_eq!(format!("{:?}", FredKey::new("abc123")), "FredKey(****)");
    }
}
