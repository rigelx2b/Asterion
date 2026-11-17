//! Arctic Shift search API: one page of submissions, parsed and stripped of author fields.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::error::DataError;
use crate::http::{BodyVerdict, Http, RetryPolicy};

pub const SOURCE_ID: &str = "arctic_shift_reddit";
pub const PROVISIONAL_SOURCE_ID: &str = "arctic_shift_reddit_provisional";
pub const BASE_URL: &str = "https://arctic-shift.photon-reddit.com";
pub const DEFAULT_ROOT: &str = "data/raw/reddit";
// Whitelist accepted by the API's `fields` parameter; `author` is deliberately absent.
pub const FIELDS: &str =
    "id,created_utc,title,selftext,score,num_comments,link_flair_text,over_18,retrieved_on";
pub const PAGE_LIMIT: usize = 100;
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
const SEARCH_PATH: &str = "/api/posts/search";
const ERROR_CODE: &str = "arctic_shift";

/// One submission. `score`, `num_comments` and `selftext` are retrieval-time snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedditPost {
    pub id: String,
    #[serde(deserialize_with = "epoch_seconds")]
    pub created_utc: i64,
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub selftext: Option<String>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub score: Option<i64>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub num_comments: Option<i64>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub link_flair_text: Option<String>,
    #[serde(default, deserialize_with = "lenient_bool")]
    pub over_18: Option<bool>,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub retrieved_on: Option<i64>,
}

fn number_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }
}

// Older archive records store epoch seconds as floats or numeric strings.
fn epoch_seconds<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    number_of(&v).ok_or_else(|| serde::de::Error::custom(format!("invalid created_utc: {v}")))
}

// Unused snapshot fields: an odd value becomes `None` instead of failing the post.
fn lenient_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(number_of(&Value::deserialize(d)?))
}

fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        _ => None,
    })
}

fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Ok(Value::deserialize(d)?.as_bool())
}

#[derive(Debug, Deserialize)]
struct Envelope {
    data: Option<Vec<Value>>,
    #[serde(default)]
    error: Option<String>,
}

/// Posts of one page; a `data: null` body becomes `DataError::Api`. Posts without
/// a usable `id` or `created_utc` are skipped so one bad record cannot block a week.
pub fn parse_page(body: &str) -> Result<Vec<RedditPost>, DataError> {
    let envelope: Envelope = serde_json::from_str(body)?;
    let items = envelope.data.ok_or_else(|| DataError::Api {
        code: ERROR_CODE.to_owned(),
        message: envelope.error.unwrap_or_default(),
    })?;
    Ok(items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

// Request mistakes come back as `data: null` with a fixed message; anything else
// with `data: null` (timeouts, overload) is worth retrying.
pub(crate) fn classify(body: &str) -> BodyVerdict {
    let value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return BodyVerdict::Fatal(e.into()),
    };
    if value.get("data").is_some_and(Value::is_array) {
        return BodyVerdict::Accept;
    }
    let message = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !value.get("data").is_some_and(Value::is_null) {
        return BodyVerdict::Fatal(DataError::Api {
            code: ERROR_CODE.to_owned(),
            message,
        });
    }
    if message.contains("is not a valid field") || message.contains("must be between") {
        BodyVerdict::Fatal(DataError::Api {
            code: ERROR_CODE.to_owned(),
            message,
        })
    } else {
        BodyVerdict::Retry(message)
    }
}

/// `{"data":[...]}` with every key starting with `author` removed from each post.
pub fn sanitize_page(body: &str) -> Result<String, DataError> {
    let mut value: Value = serde_json::from_str(body)?;
    let mut posts = match value.get_mut("data").map(Value::take) {
        Some(Value::Array(posts)) => posts,
        _ => Vec::new(),
    };
    for post in &mut posts {
        if let Value::Object(map) = post {
            map.retain(|k, _| !k.starts_with("author"));
        }
    }
    Ok(serde_json::json!({ "data": posts }).to_string())
}

pub struct ArcticClient {
    http: Http,
    search_url: String,
    busy_rounds: u32,
    busy_pause: Duration,
}

impl fmt::Debug for ArcticClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArcticClient")
            .field("http", &self.http)
            .field("search_url", &self.search_url)
            .field("busy_rounds", &self.busy_rounds)
            .field("busy_pause", &self.busy_pause)
            .finish()
    }
}

impl ArcticClient {
    pub fn new(base: &str, policy: RetryPolicy) -> Result<Self, DataError> {
        Ok(ArcticClient {
            http: Http::new("", policy)?,
            search_url: format!("{}{SEARCH_PATH}", base.trim_end_matches('/')),
            busy_rounds: 0,
            busy_pause: Duration::ZERO,
        })
    }

    /// After a page exhausts its retry policy on overload replies, pause and run
    /// the whole policy again, up to `rounds` more times.
    pub fn with_busy_rounds(mut self, rounds: u32, pause: Duration) -> Self {
        self.busy_rounds = rounds;
        self.busy_pause = pause;
        self
    }

    pub fn requests_sent(&self) -> u32 {
        self.http.requests_sent()
    }

    /// Sanitized body of posts with `after < created_utc` (both bounds epoch seconds).
    pub async fn fetch_page(
        &mut self,
        subreddit: &str,
        after: i64,
        before: i64,
    ) -> Result<String, DataError> {
        let after = after.to_string();
        let before = before.to_string();
        let limit = PAGE_LIMIT.to_string();
        let query = [
            ("subreddit", subreddit),
            ("after", after.as_str()),
            ("before", before.as_str()),
            ("limit", limit.as_str()),
            ("sort", "asc"),
            ("fields", FIELDS),
        ];
        let url = self.search_url.clone();
        let mut round = 0;
        let body = loop {
            match self
                .http
                .send_with(|c| c.get(&url).query(&query), classify)
                .await
            {
                Err(DataError::Busy { .. }) if round < self.busy_rounds => {
                    round += 1;
                    tokio::time::sleep(self.busy_pause).await;
                }
                other => break other?,
            }
        };
        sanitize_page(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_utc_accepts_int_float_and_string() {
        let body = r#"{"data":[
            {"id":"a","created_utc":1334162440,"title":"x"},
            {"id":"b","created_utc":1334162440.0,"title":"y"},
            {"id":"c","created_utc":"1334162440","title":"z"}]}"#;
        let posts = parse_page(body).expect("parsed");
        assert_eq!(posts.len(), 3);
        assert!(posts.iter().all(|p| p.created_utc == 1_334_162_440));
    }

    #[test]
    fn missing_optional_fields_default_to_none() {
        let posts = parse_page(r#"{"data":[{"id":"a","created_utc":1}]}"#).expect("parsed");
        assert_eq!(posts[0].title, None);
        assert_eq!(posts[0].retrieved_on, None);
    }

    #[test]
    fn malformed_fields_do_not_fail_page() {
        let body = r#"{"data":[
            {"id":"a","created_utc":1,"score":1.0,"retrieved_on":"123"},
            {"id":"b","created_utc":null},
            {"id":"c","created_utc":2,"num_comments":"[deleted]"}]}"#;
        let posts = parse_page(body).expect("parsed");
        assert_eq!(
            posts.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(
            (posts[0].score, posts[0].retrieved_on),
            (Some(1), Some(123))
        );
        assert_eq!(posts[1].num_comments, None);
    }

    #[test]
    fn null_data_is_api_error() {
        let err = parse_page(r#"{"data":null,"error":"boom"}"#).expect_err("error");
        assert!(matches!(err, DataError::Api { .. }));
    }

    #[test]
    fn sanitize_strips_author_keys() {
        let body = r#"{"data":[{"id":"a","created_utc":1,"author":"u","author_fullname":"t2_x","author_flair_text":"f","title":"hello"}]}"#;
        let out = sanitize_page(body).expect("sanitized");
        assert!(!out.contains("author"), "{out}");
        assert!(out.contains("\"title\""));
        assert_eq!(
            parse_page(&out).expect("parsed")[0].title.as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn classify_verdicts() {
        assert!(matches!(classify(r#"{"data":[]}"#), BodyVerdict::Accept));
        assert!(matches!(
            classify(r#"{"data":null,"error":"Timeout. Maybe slow down a bit"}"#),
            BodyVerdict::Retry(_)
        ));
        assert!(matches!(
            classify(r#"{"data":null,"error":"'stickied' is not a valid field"}"#),
            BodyVerdict::Fatal(_)
        ));
        assert!(matches!(
            classify(r#"{"data":null,"error":"'limit' must be between 1 and 100"}"#),
            BodyVerdict::Fatal(_)
        ));
        assert!(matches!(classify("<html>"), BodyVerdict::Fatal(_)));
    }
}
