use std::time::Duration;

use chrono::{Days, NaiveDate};

use super::{ENDPOINT, INDEX_ENDPOINT, MAX_ROWS_PER_PAGE};
use crate::error::DataError;
use crate::http::{Http, RetryPolicy};
use crate::redact::ServiceKey;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub endpoint: String,
    pub index_endpoint: String,
    /// Minimum gap between any two requests (plan §5: request spacing).
    pub min_interval: Duration,
    /// Total tries per page for network errors and 5xx. 4xx is never retried.
    pub max_attempts: u32,
    /// First retry delay; doubles on each further retry.
    pub initial_backoff: Duration,
    pub timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        let policy = RetryPolicy::default();
        ClientConfig {
            endpoint: ENDPOINT.to_owned(),
            index_endpoint: INDEX_ENDPOINT.to_owned(),
            min_interval: policy.min_interval,
            max_attempts: policy.max_attempts,
            initial_backoff: policy.initial_backoff,
            timeout: policy.timeout,
        }
    }
}

/// Inclusive date range for one symbol, one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest<'a> {
    pub code: &'a str,
    pub begin: NaiveDate,
    pub end: NaiveDate,
    pub page_no: u32,
}

#[derive(Debug)]
pub struct DataGoKrClient {
    http: Http,
    key: ServiceKey,
    config: ClientConfig,
}

impl DataGoKrClient {
    pub fn new(key: ServiceKey, config: ClientConfig) -> Result<Self, DataError> {
        let policy = RetryPolicy {
            min_interval: config.min_interval,
            max_attempts: config.max_attempts,
            initial_backoff: config.initial_backoff,
            timeout: config.timeout,
        };
        let http = Http::new(key.expose(), policy)?;
        Ok(DataGoKrClient { http, key, config })
    }

    pub fn requests_sent(&self) -> u32 {
        self.http.requests_sent()
    }

    /// Returns the raw response body. Body-level API errors are left to
    /// [`super::parse_page`]; this only handles transport and HTTP status.
    pub async fn fetch_page(&mut self, req: &PageRequest<'_>) -> Result<String, DataError> {
        let (begin, end_exclusive) = range_params(req.begin, req.end);
        let endpoint = self.config.endpoint.clone();
        self.get_page(
            &endpoint,
            req.page_no,
            &[
                ("likeSrtnCd", req.code),
                ("beginBasDt", begin.as_str()),
                ("endBasDt", end_exclusive.as_str()),
            ],
        )
        .await
    }

    // Every listed stock on one trading date.
    pub async fn fetch_snapshot_page(
        &mut self,
        date: NaiveDate,
        page_no: u32,
    ) -> Result<String, DataError> {
        let date = date.format("%Y%m%d").to_string();
        let endpoint = self.config.endpoint.clone();
        self.get_page(&endpoint, page_no, &[("basDt", date.as_str())])
            .await
    }

    // `idxNm` is an exact match on the index name.
    pub async fn fetch_index_page(
        &mut self,
        index_name: &str,
        begin: NaiveDate,
        end: NaiveDate,
        page_no: u32,
    ) -> Result<String, DataError> {
        let (begin, end_exclusive) = range_params(begin, end);
        let endpoint = self.config.index_endpoint.clone();
        self.get_page(
            &endpoint,
            page_no,
            &[
                ("idxNm", index_name),
                ("beginBasDt", begin.as_str()),
                ("endBasDt", end_exclusive.as_str()),
            ],
        )
        .await
    }

    async fn get_page(
        &mut self,
        endpoint: &str,
        page_no: u32,
        filters: &[(&str, &str)],
    ) -> Result<String, DataError> {
        let rows = MAX_ROWS_PER_PAGE.to_string();
        let page = page_no.to_string();
        let mut query = vec![("serviceKey", self.key.expose()), ("resultType", "json")];
        query.extend_from_slice(filters);
        query.push(("numOfRows", rows.as_str()));
        query.push(("pageNo", page.as_str()));
        self.http.get(endpoint, &query).await
    }
}

// endBasDt is exclusive ("< endBasDt"), so send the day after `end`.
fn range_params(begin: NaiveDate, end: NaiveDate) -> (String, String) {
    let end_exclusive = end.checked_add_days(Days::new(1)).unwrap_or(end);
    (
        begin.format("%Y%m%d").to_string(),
        end_exclusive.format("%Y%m%d").to_string(),
    )
}
