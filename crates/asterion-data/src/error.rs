use asterion_core::CoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("environment variable {0} is unset; set it in .env or environment")]
    MissingServiceKey(&'static str),

    #[error("HTTP request failed with status {status}; not retried further")]
    HttpStatus { status: u16 },

    #[error("API error {code}: {message}")]
    Api { code: String, message: String },

    #[error("request failed after {attempts} attempts: {detail}")]
    Network { attempts: u32, detail: String },

    #[error("upstream archive busy after {attempts} attempts: {message}")]
    Busy { attempts: u32, message: String },

    #[error("failed to parse response JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("failed to convert field {field}: {value:?}")]
    InvalidField { field: &'static str, value: String },

    #[error("unexpected page number: expected {expected}, got {got}")]
    UnexpectedPage { expected: u32, got: u32 },

    #[error("cache IO error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cached file {file} checksum mismatch; file modified or corrupted")]
    CacheIntegrity { file: String },

    #[error(transparent)]
    Core(#[from] CoreError),
}

impl DataError {
    /// Errors that will repeat for every symbol (bad key, quota, bad request),
    /// so a batch run should stop instead of sending more requests.
    pub fn is_fatal_for_batch(&self) -> bool {
        match self {
            DataError::MissingServiceKey(_) | DataError::Api { .. } => true,
            DataError::HttpStatus { status } => (400..500).contains(status),
            _ => false,
        }
    }

    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        DataError::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
