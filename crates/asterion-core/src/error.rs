use thiserror::Error;

use crate::market::Market;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("unknown market `{0}` (expected `us` or `kr`)")]
    UnknownMarket(String),

    #[error("invalid {market} symbol `{code}`: {reason}")]
    InvalidSymbol {
        market: Market,
        code: String,
        reason: &'static str,
    },
}
