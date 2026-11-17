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

impl MarketContext {
    // Missing caches load as empty series, which turn into neutral feature values.
    pub fn load(
        index_cache: &RawCache,
        macro_cache: &RawCache,
        disclosure_cache: &RawCache,
        financial_cache: &RawCache,
    ) -> Result<Self, DataError> {
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
        Ok(MarketContext {
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
            disclosures: DisclosureIndex::from_filings(opendart::load_filings(disclosure_cache)?),
            fundamentals: FundamentalIndex::from_reports(dart_financials::load_reports(
                financial_cache,
            )?),
        })
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
