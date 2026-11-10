use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// A stock market whose daily bars Asterion collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Market {
    /// US equities (NYSE, NASDAQ, ...).
    Us,
    /// Korean equities listed on KRX (KOSPI, KOSDAQ).
    Kr,
}

impl Market {
    pub const ALL: [Market; 2] = [Market::Us, Market::Kr];

    /// ISO 4217 currency code that prices in this market are quoted in.
    pub fn currency(self) -> &'static str {
        match self {
            Market::Us => "USD",
            Market::Kr => "KRW",
        }
    }

    /// IANA time zone of the exchange. Bar dates are local trading dates in
    /// this zone, never UTC dates.
    pub fn timezone(self) -> &'static str {
        match self {
            Market::Us => "America/New_York",
            Market::Kr => "Asia/Seoul",
        }
    }

    /// Absolute single-day move beyond which a close-to-close return is
    /// suspicious unless a corporate action explains it.
    ///
    /// KRX enforces a ±30% daily price limit, so a larger move there is almost
    /// always a split, merger, or data error. The US has no such limit; 30% is
    /// a review threshold, not a hard bound.
    pub fn extreme_move_threshold(self) -> f64 {
        0.30
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Market::Us => "us",
            Market::Kr => "kr",
        }
    }
}

impl fmt::Display for Market {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Market {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "us" => Ok(Market::Us),
            "kr" => Ok(Market::Kr),
            _ => Err(CoreError::UnknownMarket(s.to_owned())),
        }
    }
}

/// A validated ticker within one market.
///
/// - US: 1-10 characters, uppercase letters, digits, `.` or `-` (e.g. `BRK.B`).
/// - KR: the 6-character KRX short code (e.g. `005930`). Newer listings may
///   contain uppercase letters (e.g. `0001A0`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Symbol {
    market: Market,
    code: String,
}

impl Symbol {
    pub fn new(market: Market, code: &str) -> Result<Self, CoreError> {
        let code = code.trim().to_ascii_uppercase();
        let invalid = |reason| CoreError::InvalidSymbol {
            market,
            code: code.clone(),
            reason,
        };

        match market {
            Market::Us => {
                if code.is_empty() || code.len() > 10 {
                    return Err(invalid("must be 1-10 characters"));
                }
                if !code
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '.' || c == '-')
                {
                    return Err(invalid("only letters, digits, `.` and `-` are allowed"));
                }
            }
            Market::Kr => {
                if code.len() != 6 {
                    return Err(invalid("KRX short codes are exactly 6 characters"));
                }
                if !code
                    .chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                {
                    return Err(invalid("only digits and uppercase letters are allowed"));
                }
            }
        }

        Ok(Symbol { market, code })
    }

    pub fn market(&self) -> Market {
        self.market
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.market, self.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_parses_case_insensitively() {
        assert_eq!("US".parse::<Market>(), Ok(Market::Us));
        assert_eq!("kr".parse::<Market>(), Ok(Market::Kr));
        assert!(matches!(
            "jp".parse::<Market>(),
            Err(CoreError::UnknownMarket(_))
        ));
    }

    #[test]
    fn market_serde_uses_lowercase() {
        assert_eq!(
            serde_json::to_string(&Market::Kr).expect("serialize"),
            "\"kr\""
        );
    }

    #[test]
    fn us_symbols_are_normalized_and_validated() {
        let s = Symbol::new(Market::Us, " brk.b ").expect("valid");
        assert_eq!(s.code(), "BRK.B");
        assert_eq!(s.to_string(), "us:BRK.B");
        assert!(Symbol::new(Market::Us, "").is_err());
        assert!(Symbol::new(Market::Us, "AAPL$").is_err());
        assert!(Symbol::new(Market::Us, "TOOLONGTICKER").is_err());
    }

    #[test]
    fn kr_symbols_require_six_character_codes() {
        assert!(Symbol::new(Market::Kr, "005930").is_ok());
        assert!(Symbol::new(Market::Kr, "0001a0").is_ok());
        assert!(Symbol::new(Market::Kr, "5930").is_err());
        assert!(Symbol::new(Market::Kr, "00593-").is_err());
    }
}
