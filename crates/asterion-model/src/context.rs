//! Market-wide series (KRX indices, US markets, rates, FX) aligned to stock dates.

use asterion_data::data_go_kr::{KOSDAQ, KOSPI};
use asterion_data::{DataError, RawCache, dart_financials, fred, opendart};
use chrono::NaiveDate;

use crate::disclosures::DisclosureIndex;
use crate::fundamentals::FundamentalIndex;

// Positive daily observations sorted by date, one per date.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailySeries {
    dates: Vec<NaiveDate>,
    values: Vec<f64>,
}

impl DailySeries {
    pub fn from_points(points: impl IntoIterator<Item = (NaiveDate, f64)>) -> Self {
        let mut points: Vec<(NaiveDate, f64)> = points
            .into_iter()
            .filter(|(_, v)| v.is_finite() && *v > 0.0)
            .collect();
        points.sort_by_key(|(d, _)| *d);
        points.dedup_by_key(|(d, _)| *d);
        DailySeries {
            dates: points.iter().map(|(d, _)| *d).collect(),
            values: points.iter().map(|(_, v)| *v).collect(),
        }
    }

    // Keeps zero and negative values too, for spreads and rates that cross zero; only
    // `value` and `difference` are meaningful on such a series.
    pub fn from_levels(points: impl IntoIterator<Item = (NaiveDate, f64)>) -> Self {
        let mut points: Vec<(NaiveDate, f64)> =
            points.into_iter().filter(|(_, v)| v.is_finite()).collect();
        points.sort_by_key(|(d, _)| *d);
        points.dedup_by_key(|(d, _)| *d);
        DailySeries {
            dates: points.iter().map(|(d, _)| *d).collect(),
            values: points.iter().map(|(_, v)| *v).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.dates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dates.is_empty()
    }

    // Index of the latest observation dated `<= date` (or `< date` when `strict`),
    // unless that observation is more than `max_stale_days` old.
    pub fn index_at(&self, date: NaiveDate, strict: bool, max_stale_days: i64) -> Option<usize> {
        let count = if strict {
            self.dates.partition_point(|d| *d < date)
        } else {
            self.dates.partition_point(|d| *d <= date)
        };
        let idx = count.checked_sub(1)?;
        ((date - self.dates[idx]).num_days() <= max_stale_days).then_some(idx)
    }

    pub fn date(&self, idx: usize) -> NaiveDate {
        self.dates[idx]
    }

    pub fn value(&self, idx: usize) -> f64 {
        self.values[idx]
    }

    // Log return over `lag` observations ending at `idx`; `None` without enough history.
    pub fn log_return(&self, idx: usize, lag: usize) -> Option<f64> {
        let start = idx.checked_sub(lag)?;
        Some((self.values[idx] / self.values[start]).ln())
    }

    pub fn difference(&self, idx: usize, lag: usize) -> Option<f64> {
        let start = idx.checked_sub(lag)?;
        Some(self.values[idx] - self.values[start])
    }

    // Sample standard deviation of the last `window` one-step log returns ending at `idx`.
    pub fn volatility(&self, idx: usize, window: usize) -> Option<f64> {
        if window < 2 || idx < window {
            return None;
        }
        let returns: Vec<f64> = ((idx + 1 - window)..=idx)
            .map(|i| (self.values[i] / self.values[i - 1]).ln())
            .collect();
        let mean = returns.iter().sum::<f64>() / window as f64;
        let variance =
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (window - 1) as f64;
        Some(variance.sqrt())
    }
}

// Overlapping sessions needed before an earlier source may be prepended.
const MIN_INDEX_OVERLAP: usize = 20;
// Both sources publish the official index level, so they must agree to rounding.
const INDEX_LEVEL_TOLERANCE: f64 = 1e-4;
// Longest calendar gap allowed inside the prepended part (long holidays reach ~11 days).
const MAX_INDEX_GAP_DAYS: i64 = 14;

/// Outcome of prepending an earlier source to one index series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexSplice {
    pub prefix: usize,
    pub overlap: usize,
    pub max_rel_diff: f64,
    pub max_gap_days: i64,
    pub first: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexSpliceError {
    ShortOverlap(usize),
    LevelMismatch {
        date: NaiveDate,
        current: f64,
        earlier: f64,
    },
    Gap {
        after: NaiveDate,
        days: i64,
    },
}

impl std::fmt::Display for IndexSpliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexSpliceError::ShortOverlap(n) => write!(
                f,
                "too few overlapping sessions ({n}) to verify index sources"
            ),
            IndexSpliceError::LevelMismatch {
                date,
                current,
                earlier,
            } => write!(
                f,
                "index mismatch on {date}: data.go.kr {current}, kis {earlier}"
            ),
            IndexSpliceError::Gap { after, days } => {
                write!(
                    f,
                    "kis index history gap of {days} calendar days after {after}"
                )
            }
        }
    }
}

impl std::error::Error for IndexSpliceError {}

impl DailySeries {
    // Prepends the points of `earlier` dated before this series starts. Both must report
    // the same level on every shared date; existing points are never changed.
    pub fn prepend_checked(
        &mut self,
        earlier: &DailySeries,
    ) -> Result<IndexSplice, IndexSpliceError> {
        let Some(&start) = self.dates.first() else {
            return Err(IndexSpliceError::ShortOverlap(0));
        };
        let (mut overlap, mut max_rel_diff) = (0usize, 0.0f64);
        for (date, &earlier_value) in earlier.dates.iter().zip(&earlier.values) {
            let Ok(i) = self.dates.binary_search(date) else {
                continue;
            };
            let current = self.values[i];
            let diff = (earlier_value / current - 1.0).abs();
            if diff > INDEX_LEVEL_TOLERANCE {
                return Err(IndexSpliceError::LevelMismatch {
                    date: *date,
                    current,
                    earlier: earlier_value,
                });
            }
            overlap += 1;
            max_rel_diff = max_rel_diff.max(diff);
        }
        if overlap < MIN_INDEX_OVERLAP {
            return Err(IndexSpliceError::ShortOverlap(overlap));
        }
        let prefix = earlier.dates.partition_point(|d| *d < start);
        let mut max_gap_days = 0;
        for pair in earlier.dates[..prefix].windows(2) {
            let days = (pair[1] - pair[0]).num_days();
            if days > MAX_INDEX_GAP_DAYS {
                return Err(IndexSpliceError::Gap {
                    after: pair[0],
                    days,
                });
            }
            max_gap_days = max_gap_days.max(days);
        }
        let first = earlier.dates[..prefix].first().copied();
        self.dates
            .splice(0..0, earlier.dates[..prefix].iter().copied());
        self.values
            .splice(0..0, earlier.values[..prefix].iter().copied());
        Ok(IndexSplice {
            prefix,
            overlap,
            max_rel_diff,
            max_gap_days,
            first,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarketContext {
    pub kospi: DailySeries,
    pub kosdaq: DailySeries,
    pub sp500: DailySeries,
    pub nasdaq: DailySeries,
    pub vix: DailySeries,
    pub us10y: DailySeries,
    pub usdkrw: DailySeries,
    pub wti: DailySeries,
    pub dollar: DailySeries,
    pub usdjpy: DailySeries,
    pub usdcny: DailySeries,
    pub us2y: DailySeries,
    pub term_spread: DailySeries,
    pub baa_yield: DailySeries,
    pub nasdaq_vol: DailySeries,
    pub breakeven: DailySeries,
    pub disclosures: DisclosureIndex,
    pub fundamentals: FundamentalIndex,
}

/// Per-company OpenDART caches for filers the board feeds miss (delisted companies).
#[derive(Debug, Clone, Copy)]
pub struct DartBackfill<'a> {
    pub filings: &'a RawCache,
    pub financials: &'a RawCache,
}

/// Stock codes contributed by a `DartBackfill`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillCounts {
    pub filing_codes: usize,
    pub filings: usize,
    pub report_codes: usize,
    pub reports: usize,
}

// Backfill rows may only cover codes the main caches lack, so main codes stay bit-identical.
fn disjoint_codes<'a>(
    main: impl Iterator<Item = &'a str>,
    extra: impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<usize, DataError> {
    let main: std::collections::BTreeSet<&str> = main.collect();
    let extra: std::collections::BTreeSet<&str> = extra.collect();
    let shared: Vec<&str> = extra.intersection(&main).copied().collect();
    if shared.is_empty() {
        Ok(extra.len())
    } else {
        Err(DataError::InvalidField {
            field,
            value: shared.join(","),
        })
    }
}

impl MarketContext {
    // Missing caches load as empty series, which turn into neutral feature values.
    pub fn load(
        index_cache: &RawCache,
        macro_cache: &RawCache,
        disclosure_cache: &RawCache,
        financial_cache: &RawCache,
    ) -> Result<Self, DataError> {
        Ok(Self::load_with_backfill(
            index_cache,
            macro_cache,
            disclosure_cache,
            financial_cache,
            &[],
        )?
        .0)
    }

    // Like `load`, adding filings and key accounts from `backfill` for codes the main
    // caches do not contain; a code present in both is an error.
    pub fn load_with_backfill(
        index_cache: &RawCache,
        macro_cache: &RawCache,
        disclosure_cache: &RawCache,
        financial_cache: &RawCache,
        backfill: &[DartBackfill<'_>],
    ) -> Result<(Self, BackfillCounts), DataError> {
        let mut filings = opendart::load_filings(disclosure_cache)?;
        let mut reports = dart_financials::load_reports(financial_cache)?;
        let mut counts = BackfillCounts::default();
        for extra in backfill {
            let extra_filings = opendart::load_corp_filings(extra.filings)?;
            let extra_reports = dart_financials::load_reports(extra.financials)?;
            counts.filing_codes += disjoint_codes(
                filings.iter().map(|f| f.stock_code.as_str()),
                extra_filings.iter().map(|f| f.stock_code.as_str()),
                "backfill_filing_codes",
            )?;
            counts.report_codes += disjoint_codes(
                reports.iter().map(|r| r.stock_code.as_str()),
                extra_reports.iter().map(|r| r.stock_code.as_str()),
                "backfill_report_codes",
            )?;
            counts.filings += extra_filings.len();
            counts.reports += extra_reports.len();
            filings.extend(extra_filings);
            reports.extend(extra_reports);
        }
        let index = |spec| -> Result<DailySeries, DataError> {
            let quotes = index_cache.load_index_quotes(spec)?;
            Ok(DailySeries::from_points(
                quotes.iter().map(|q| (q.date, q.close)),
            ))
        };
        let series = |id| -> Result<DailySeries, DataError> {
            let observations = fred::load_series(macro_cache, id)?;
            Ok(DailySeries::from_points(
                observations.iter().map(|o| (o.date, o.value)),
            ))
        };
        let levels = |id| -> Result<DailySeries, DataError> {
            let observations = fred::load_series(macro_cache, id)?;
            Ok(DailySeries::from_levels(
                observations.iter().map(|o| (o.date, o.value)),
            ))
        };
        let context = MarketContext {
            kospi: index(&KOSPI)?,
            kosdaq: index(&KOSDAQ)?,
            sp500: series(fred::SP500)?,
            nasdaq: series(fred::NASDAQ)?,
            vix: series(fred::VIX)?,
            us10y: series(fred::US10Y)?,
            usdkrw: series(fred::USDKRW)?,
            wti: series(fred::WTI)?,
            dollar: series(fred::DOLLAR_BROAD)?,
            usdjpy: series(fred::USDJPY)?,
            usdcny: series(fred::USDCNY)?,
            us2y: levels(fred::US2Y)?,
            term_spread: levels(fred::TERM_SPREAD)?,
            baa_yield: levels(fred::BAA_YIELD)?,
            nasdaq_vol: series(fred::NASDAQ_VOL)?,
            breakeven: levels(fred::BREAKEVEN_10Y)?,
            disclosures: DisclosureIndex::from_filings(filings),
            fundamentals: FundamentalIndex::from_reports(reports),
        };
        Ok((context, counts))
    }

    // The broad index of the board a stock is listed on.
    pub fn index_for(&self, market_category: &str) -> &DailySeries {
        if market_category.eq_ignore_ascii_case("KOSDAQ") {
            &self.kosdaq
        } else {
            &self.kospi
        }
    }

    pub fn summary(&self) -> [(&'static str, usize); 17] {
        [
            ("kospi", self.kospi.len()),
            ("kosdaq", self.kosdaq.len()),
            ("sp500", self.sp500.len()),
            ("nasdaq", self.nasdaq.len()),
            ("vix", self.vix.len()),
            ("us10y", self.us10y.len()),
            ("usdkrw", self.usdkrw.len()),
            ("wti", self.wti.len()),
            ("dollar", self.dollar.len()),
            ("usdjpy", self.usdjpy.len()),
            ("usdcny", self.usdcny.len()),
            ("us2y", self.us2y.len()),
            ("term_spread", self.term_spread.len()),
            ("baa_yield", self.baa_yield.len()),
            ("nasdaq_vol", self.nasdaq_vol.len()),
            ("breakeven", self.breakeven.len()),
            ("fundamental_reports", self.fundamentals.len()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 1, day).expect("valid date")
    }

    #[test]
    fn index_at_respects_strictness_and_staleness() {
        let series =
            DailySeries::from_points([(d(3), 11.0), (d(2), 10.0), (d(2), 99.0), (d(5), -1.0)]);
        assert_eq!(series.len(), 2);
        assert_eq!(series.index_at(d(3), false, 5), Some(1));
        assert_eq!(series.index_at(d(3), true, 5), Some(0));
        assert_eq!(series.index_at(d(2), true, 5), None);
        assert_eq!(series.index_at(d(20), false, 5), None, "stale");
        assert_eq!(series.value(0), 10.0);
    }

    fn days_from(start: NaiveDate, n: u64, level: impl Fn(u64) -> f64) -> DailySeries {
        DailySeries::from_points((0..n).map(|i| (start + chrono::Days::new(i), level(i))))
    }

    #[test]
    fn prepend_adds_only_earlier_dates_after_overlap_check() {
        let jan = NaiveDate::from_ymd_opt(2020, 1, 1).expect("date");
        let dec = NaiveDate::from_ymd_opt(2019, 12, 1).expect("date");
        let mut current = days_from(jan, 40, |i| 2000.0 + i as f64);
        let before = current.clone();
        // Earlier source spans 2019-12-01..2020-01-30 and matches on the 30 shared days.
        let earlier = days_from(dec, 61, |i| 2000.0 + i as f64 - 31.0);
        let out = current.prepend_checked(&earlier).expect("splices");
        assert_eq!(out.prefix, 31);
        assert_eq!(out.overlap, 30);
        assert_eq!(out.first, Some(dec));
        assert_eq!(current.len(), before.len() + 31);
        assert_eq!(current.dates[31..], before.dates[..]);
        assert_eq!(current.values[31..], before.values[..]);
    }

    #[test]
    fn prepend_rejects_mismatch_short_overlap_and_gaps() {
        let jan = NaiveDate::from_ymd_opt(2020, 1, 1).expect("date");
        let dec = NaiveDate::from_ymd_opt(2019, 12, 1).expect("date");
        let current = days_from(jan, 40, |_| 2000.0);
        let scaled = days_from(dec, 61, |_| 2001.0);
        assert!(matches!(
            current.clone().prepend_checked(&scaled),
            Err(IndexSpliceError::LevelMismatch { .. })
        ));
        let short = days_from(dec, 40, |_| 2000.0);
        assert!(matches!(
            current.clone().prepend_checked(&short),
            Err(IndexSpliceError::ShortOverlap(9))
        ));
        let mut points: Vec<(NaiveDate, f64)> = (0..61)
            .map(|i| (dec + chrono::Days::new(i), 2000.0))
            .collect();
        points.retain(|(d, _)| {
            *d < NaiveDate::from_ymd_opt(2019, 12, 5).expect("date")
                || *d > NaiveDate::from_ymd_opt(2019, 12, 25).expect("date")
        });
        let gapped = DailySeries::from_points(points);
        assert!(matches!(
            current.clone().prepend_checked(&gapped),
            Err(IndexSpliceError::Gap { days: 22, .. })
        ));
    }

    #[test]
    fn backfill_codes_must_not_overlap_main_codes() {
        let main = ["000020", "005930"];
        let added = disjoint_codes(main.into_iter(), ["117930", "117930"].into_iter(), "t");
        assert_eq!(added.expect("disjoint"), 1);
        match disjoint_codes(main.into_iter(), ["005930", "117930"].into_iter(), "t") {
            Err(DataError::InvalidField { value, .. }) => assert_eq!(value, "005930"),
            other => panic!("expected an overlap error, got {other:?}"),
        }
    }

    #[test]
    fn returns_need_enough_history() {
        let series = DailySeries::from_points([(d(1), 100.0), (d(2), 110.0), (d(3), 121.0)]);
        assert!((series.log_return(2, 2).expect("ret") - 1.21f64.ln()).abs() < 1e-12);
        assert_eq!(series.log_return(1, 2), None);
        assert_eq!(series.difference(2, 1), Some(11.0));
        let vol = series.volatility(2, 2).expect("vol");
        assert!(vol.abs() < 1e-12, "constant growth has zero volatility");
        assert_eq!(series.volatility(1, 2), None);
    }
}
