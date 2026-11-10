use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// One trading day of OHLCV data for a single symbol.
///
/// `open`..`close` are the prices as traded (unadjusted). `adj_close` is the
/// split- and dividend-adjusted close when the source provides one; sources
/// that do not (e.g. KRX public data) leave it `None` and adjustment factors
/// are derived from [`CorporateAction`] records instead.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    /// Local trading date in the market's time zone.
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub adj_close: Option<f64>,
    pub volume: u64,
}

impl Bar {
    /// True when every price is finite and positive and
    /// `low <= min(open, close)` and `max(open, close) <= high`.
    pub fn is_ohlc_consistent(&self) -> bool {
        let prices = [self.open, self.high, self.low, self.close];
        prices.iter().all(|p| p.is_finite() && *p > 0.0)
            && self.low <= self.open.min(self.close)
            && self.open.max(self.close) <= self.high
            && self.adj_close.is_none_or(|a| a.is_finite() && a > 0.0)
    }
}

/// An event that changes how historical prices compare to current ones.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CorporateAction {
    /// Ex-date: the first trading date on which the new basis applies.
    pub date: NaiveDate,
    pub kind: CorporateActionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CorporateActionKind {
    /// New shares per old share: 2.0 for a 2-for-1 split, 0.1 for a 1-for-10
    /// reverse split.
    Split { ratio: f64 },
    /// Cash dividend per share, in the market's currency.
    CashDividend { amount: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(open: f64, high: f64, low: f64, close: f64) -> Bar {
        Bar {
            date: NaiveDate::from_ymd_opt(2026, 1, 2).expect("valid date"),
            open,
            high,
            low,
            close,
            adj_close: None,
            volume: 1_000,
        }
    }

    #[test]
    fn consistent_bar_passes() {
        assert!(bar(10.0, 12.0, 9.0, 11.0).is_ohlc_consistent());
        assert!(bar(10.0, 10.0, 10.0, 10.0).is_ohlc_consistent());
    }

    #[test]
    fn inconsistent_bars_fail() {
        assert!(
            !bar(10.0, 9.5, 9.0, 11.0).is_ohlc_consistent(),
            "close > high"
        );
        assert!(
            !bar(10.0, 12.0, 10.5, 11.0).is_ohlc_consistent(),
            "low > open"
        );
        assert!(
            !bar(0.0, 12.0, 0.0, 11.0).is_ohlc_consistent(),
            "zero price"
        );
        assert!(!bar(f64::NAN, 12.0, 9.0, 11.0).is_ohlc_consistent(), "NaN");
        let mut b = bar(10.0, 12.0, 9.0, 11.0);
        b.adj_close = Some(-1.0);
        assert!(!b.is_ohlc_consistent(), "negative adj_close");
    }

    #[test]
    fn corporate_action_serde_is_tagged() {
        let action = CorporateAction {
            date: NaiveDate::from_ymd_opt(2024, 6, 10).expect("valid date"),
            kind: CorporateActionKind::Split { ratio: 10.0 },
        };
        let json = serde_json::to_string(&action).expect("serialize");
        assert_eq!(
            json,
            r#"{"date":"2024-06-10","kind":{"type":"split","ratio":10.0}}"#
        );
        let back: CorporateAction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, action);
    }
}
