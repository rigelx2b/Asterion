use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a batch of bars came from, so every stored row can be traced back to
/// the raw response it was parsed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Source identifier, e.g. `tiingo` or `data_go_kr`.
    pub source: String,
    /// When the raw response was fetched.
    pub fetched_at: DateTime<Utc>,
    /// SHA-256 of the cached raw response body, hex-encoded.
    pub raw_sha256: String,
}
