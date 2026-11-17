use std::collections::{BTreeMap, BTreeSet};

use asterion_core::{Bar, Market};
use chrono::NaiveDate;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Issue {
    /// Prices non-positive/non-finite or outside `low..=high`.
    OhlcInconsistent {
        date: NaiveDate,
    },
    /// KRX trading halt: open/high/low are 0 and volume is 0, close carries over.
    Halted {
        date: NaiveDate,
    },
    Duplicate {
        date: NaiveDate,
    },
    OutOfOrder {
        date: NaiveDate,
    },
    /// Close-to-close move beyond [`Market::extreme_move_threshold`]; usually
    /// a split or other corporate action, since KRX caps daily moves at 30%.
    ExtremeMove {
        date: NaiveDate,
        ret: f64,
    },
    /// Date traded by other symbols in the set but absent for this one.
    MissingDate {
        date: NaiveDate,
    },
}

impl Issue {
    pub fn kind(&self) -> &'static str {
        match self {
            Issue::OhlcInconsistent { .. } => "ohlc_inconsistent",
            Issue::Halted { .. } => "halted",
            Issue::Duplicate { .. } => "duplicate",
            Issue::OutOfOrder { .. } => "out_of_order",
            Issue::ExtremeMove { .. } => "extreme_move",
            Issue::MissingDate { .. } => "missing_date",
        }
    }

    pub fn date(&self) -> NaiveDate {
        match self {
            Issue::OhlcInconsistent { date }
            | Issue::Halted { date }
            | Issue::Duplicate { date }
            | Issue::OutOfOrder { date }
            | Issue::ExtremeMove { date, .. }
            | Issue::MissingDate { date } => *date,
        }
    }

    /// Errors make the data unusable as-is; the rest need review.
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Issue::OhlcInconsistent { .. } | Issue::Duplicate { .. } | Issue::OutOfOrder { .. }
        )
    }
}

/// Per-series checks. `bars` should be in the order they will be used.
pub fn validate_bars(market: Market, bars: &[Bar]) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut prev_date: Option<NaiveDate> = None;
    let mut prev_close: Option<f64> = None;

    for bar in bars {
        if let Some(prev) = prev_date {
            if bar.date == prev {
                issues.push(Issue::Duplicate { date: bar.date });
            } else if bar.date < prev {
                issues.push(Issue::OutOfOrder { date: bar.date });
            }
        }
        prev_date = Some(prev_date.map_or(bar.date, |p| p.max(bar.date)));

        if is_halted(bar) {
            issues.push(Issue::Halted { date: bar.date });
            continue;
        }
        if !bar.is_ohlc_consistent() {
            issues.push(Issue::OhlcInconsistent { date: bar.date });
            continue;
        }
        // Halted and broken bars are skipped, so this compares valid closes only.
        if let Some(prev) = prev_close {
            let ret = bar.close / prev - 1.0;
            if ret.abs() > market.extreme_move_threshold() {
                issues.push(Issue::ExtremeMove {
                    date: bar.date,
                    ret,
                });
            }
        }
        prev_close = Some(bar.close);
    }
    issues
}

fn is_halted(bar: &Bar) -> bool {
    bar.volume == 0
        && bar.open == 0.0
        && bar.high == 0.0
        && bar.low == 0.0
        && bar.close.is_finite()
        && bar.close > 0.0
}

/// Cross-sectional gap check: the union of all symbols' dates stands in for
/// the trading calendar, and each symbol is checked within its own first..last
/// span. Needs several symbols to be meaningful.
pub fn missing_dates(
    series: &BTreeMap<String, Vec<NaiveDate>>,
) -> BTreeMap<String, Vec<NaiveDate>> {
    let calendar: BTreeSet<NaiveDate> = series.values().flatten().copied().collect();
    series
        .iter()
        .filter_map(|(code, dates)| {
            let own: BTreeSet<NaiveDate> = dates.iter().copied().collect();
            let (first, last) = (*own.first()?, *own.last()?);
            let missing: Vec<NaiveDate> = calendar
                .range(first..=last)
                .filter(|d| !own.contains(d))
                .copied()
                .collect();
            (!missing.is_empty()).then(|| (code.clone(), missing))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 1, day).expect("valid date")
    }

    fn bar(day: u32, close: f64) -> Bar {
        Bar {
            date: d(day),
            open: close,
            high: close,
            low: close,
            close,
            adj_close: None,
            volume: 100,
        }
    }

    #[test]
    fn clean_series_has_no_issues() {
        let bars = [bar(2, 100.0), bar(3, 110.0), bar(4, 105.0)];
        assert!(validate_bars(Market::Kr, &bars).is_empty());
    }

    #[test]
    fn detects_duplicates_order_and_ohlc() {
        let mut broken = bar(5, 100.0);
        broken.low = 101.0;
        let bars = [
            bar(2, 100.0),
            bar(3, 100.0),
            bar(3, 100.0),
            bar(1, 100.0),
            broken,
        ];
        let kinds: Vec<_> = validate_bars(Market::Kr, &bars)
            .iter()
            .map(|i| (i.kind(), i.date()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("duplicate", d(3)),
                ("out_of_order", d(1)),
                ("ohlc_inconsistent", d(5))
            ]
        );
    }

    #[test]
    fn halted_day_is_skipped_for_returns() {
        let mut halted = bar(3, 100.0);
        halted.open = 0.0;
        halted.high = 0.0;
        halted.low = 0.0;
        halted.volume = 0;
        let bars = [bar(2, 100.0), halted, bar(4, 120.0)];
        let issues = validate_bars(Market::Kr, &bars);
        assert_eq!(issues, vec![Issue::Halted { date: d(3) }]);
        assert!(!issues[0].is_error());
    }

    #[test]
    fn extreme_move_is_flagged() {
        let bars = [bar(2, 100.0), bar(3, 50.0)];
        match validate_bars(Market::Kr, &bars).as_slice() {
            [Issue::ExtremeMove { date, ret }] => {
                assert_eq!(*date, d(3));
                assert!((ret + 0.5).abs() < 1e-12);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_dates_use_union_calendar_within_each_span() {
        let mut series = BTreeMap::new();
        series.insert("A".to_owned(), vec![d(2), d(3), d(4), d(5)]);
        series.insert("B".to_owned(), vec![d(2), d(5)]);
        // C listed later: dates before its first row are not missing.
        series.insert("C".to_owned(), vec![d(4), d(5)]);
        let missing = missing_dates(&series);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing["B"], vec![d(3), d(4)]);
    }
}
