//! Feature extraction and dataset construction for financial time-series forecasting.
//!
//! Enforces strict chronological order, zero future-information leakage,
//! and corporate-action split detection to preserve clean out-of-sample guarantees.

use asterion_core::Bar;
use asterion_data::adjusted_bars;
use asterion_data::data_go_kr::KrxDailyQuote;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::context::{DailySeries, MarketContext};
use crate::disclosures::NUM_DISCLOSURE_FEATURES;
use crate::fundamentals::NUM_FUNDAMENTAL_FEATURES;
use crate::metrics::HORIZONS;

// Index observations older than this many calendar days are treated as missing.
const INDEX_MAX_STALE_DAYS: i64 = 5;
// Covers long weekends and holiday runs in the overseas series.
const MACRO_MAX_STALE_DAYS: i64 = 10;
// Long-run VIX level, used when no observation is available.
const NEUTRAL_VIX: f64 = 20.0;
// Fewest names on one date for a cross-sectional median to exist.
const MIN_CROSS_SECTION: usize = 2;

pub const FEATURE_NAMES: [&str; NUM_FEATURES] = [
    "log_ret_1",
    "log_ret_2",
    "log_ret_3",
    "log_ret_5",
    "log_ret_10",
    "log_ret_20",
    "volatility_5",
    "volatility_20",
    "sma_5_ratio",
    "sma_20_ratio",
    "sma_60_ratio",
    "intraday_range",
    "close_to_open",
    "volume_ratio_20",
    "volume_momentum",
    "mkt_ret_1",
    "mkt_ret_5",
    "mkt_ret_20",
    "mkt_vol_20",
    "rel_ret_5",
    "rel_ret_20",
    "sp500_ret_1",
    "sp500_ret_5",
    "nasdaq_ret_1",
    "vix_log",
    "vix_chg_5",
    "us10y_chg_5",
    "usdkrw_ret_1",
    "usdkrw_ret_5",
    "dart_new",
    "dart_cnt_30",
    "dart_periodic_7",
    "dart_major_7",
    "dart_ownership_7",
    "dart_exchange_7",
    "dart_earnings_7",
    "dart_contract_7",
    "dart_capital_7",
    "dart_buyback_7",
    "log_ret_60",
    "high_52w_ratio",
    "low_52w_ratio",
    "max_ret_20",
    "log_mcap",
    "turnover_20",
    "amihud_20",
    "wti_ret_5",
    "dollar_ret_5",
    "usdjpy_ret_5",
    "usdcny_ret_5",
    "us2y_chg_5",
    "term_spread",
    "baa_spread",
    "baa_spread_chg_5",
    "vxn_log",
    "breakeven_chg_5",
    "fund_earnings_yield",
    "fund_book_to_market",
    "fund_sales_to_price",
    "fund_op_margin",
    "fund_roe",
    "fund_leverage",
    "fund_revenue_yoy",
    "fund_age",
    "fund_available",
];

const NUM_EXTRA_FEATURES: usize = 17;
pub const NUM_FEATURES: usize =
    29 + NUM_DISCLOSURE_FEATURES + NUM_EXTRA_FEATURES + NUM_FUNDAMENTAL_FEATURES;
// Bars of history needed before the first feature row (60-session moving average).
pub const WARMUP: usize = 60;
// Sessions in the 52-week high/low window.
const YEAR_SESSIONS: usize = 250;

// Unadjusted size and liquidity of one session, aligned with the adjusted bars.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BarExtra {
    pub market_cap: f64,
    pub trading_value: f64,
}

// Adjusted bars and their same-day size/liquidity, one entry per date (first row wins,
// as in `adjusted_bars`).
pub fn bars_and_extras(quotes: &[KrxDailyQuote]) -> (Vec<Bar>, Vec<BarExtra>) {
    let mut unique: Vec<KrxDailyQuote> = Vec::with_capacity(quotes.len());
    for q in quotes {
        if unique.last().is_none_or(|last| last.date != q.date) {
            unique.push(q.clone());
        }
    }
    let extras = unique
        .iter()
        .map(|q| BarExtra {
            market_cap: q.market_cap as f64,
            trading_value: q.trading_value as f64,
        })
        .collect();
    (adjusted_bars(&unique), extras)
}

/// Single time-step sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub symbol: String,
    pub date: NaiveDate,
    pub features: Vec<f64>,
    /// Forward return for each horizon in [`HORIZONS`] (1, 3, 5, 10, 20).
    pub forward_returns: Vec<f64>,
    /// Date of the furthest-horizon close the targets look at.
    pub target_end: NaiveDate,
    // Unadjusted same-day trading value in KRW; zero on halted sessions.
    #[serde(default)]
    pub trading_value: f64,
}

/// Replaces every forward return with its excess over the median of all names on
/// the same date, per horizon, so that up and down targets are balanced and the
/// market-wide move is removed. Dates with fewer than two names are dropped.
pub fn to_excess_returns(mut samples: Vec<Sample>) -> Vec<Sample> {
    samples.sort_by_key(|s| s.date);
    let mut out = Vec::with_capacity(samples.len());
    let mut start = 0;
    while start < samples.len() {
        let date = samples[start].date;
        let end = start + samples[start..].partition_point(|s| s.date == date);
        let group = &mut samples[start..end];
        if group.len() >= MIN_CROSS_SECTION {
            for h in 0..HORIZONS.len() {
                let mut values: Vec<f64> = group.iter().map(|s| s.forward_returns[h]).collect();
                values.sort_by(f64::total_cmp);
                let mid = values.len() / 2;
                let median = if values.len().is_multiple_of(2) {
                    (values[mid - 1] + values[mid]) / 2.0
                } else {
                    values[mid]
                };
                for s in group.iter_mut() {
                    s.forward_returns[h] -= median;
                }
            }
            out.extend(group.iter().cloned());
        }
        start = end;
    }
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StandardScaler {
    pub means: Vec<f64>,
    pub stds: Vec<f64>,
}

impl StandardScaler {
    pub fn fit(samples: &[Sample]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let num_feat = NUM_FEATURES;
        let mut sums = vec![0.0; num_feat];
        let mut sq_sums = vec![0.0; num_feat];
        let n = samples.len() as f64;

        for s in samples {
            for (i, &v) in s.features.iter().enumerate().take(num_feat) {
                sums[i] += v;
                sq_sums[i] += v * v;
            }
        }

        let mut means = vec![0.0; num_feat];
        let mut stds = vec![1.0; num_feat];
        for i in 0..num_feat {
            let mean = sums[i] / n;
            let variance = (sq_sums[i] / n) - (mean * mean);
            let std = if variance > 1e-12 {
                variance.sqrt()
            } else {
                1.0
            };
            means[i] = mean;
            stds[i] = std;
        }

        StandardScaler { means, stds }
    }

    pub fn transform(&self, sample: &mut Sample) {
        self.transform_row(&mut sample.features);
    }

    pub fn transform_row(&self, row: &mut [f64]) {
        for (i, v) in row.iter_mut().enumerate() {
            if i < self.means.len() {
                *v = (*v - self.means[i]) / self.stds[i];
                // Clip extreme outliers to [-5.0, 5.0] for stable neural training
                *v = v.clamp(-5.0, 5.0);
            }
        }
    }
}

// Own-market index features at `date`: returns over 1/5/20 sessions and 20-session volatility.
fn index_features(index: &DailySeries, date: NaiveDate) -> [f64; 4] {
    let Some(i) = index.index_at(date, false, INDEX_MAX_STALE_DAYS) else {
        return [0.0; 4];
    };
    [
        index.log_return(i, 1).unwrap_or(0.0),
        index.log_return(i, 5).unwrap_or(0.0),
        index.log_return(i, 20).unwrap_or(0.0),
        index.volatility(i, 20).unwrap_or(0.0),
    ]
}

// Overseas series are read strictly before `date`: the US session dated D closes
// after the Korean close of D, so only observations up to D-1 are known at that time.
fn macro_features(ctx: &MarketContext, date: NaiveDate) -> [f64; 8] {
    let at = |series: &DailySeries| series.index_at(date, true, MACRO_MAX_STALE_DAYS);
    let log_return = |series: &DailySeries, lag: usize| {
        at(series)
            .and_then(|i| series.log_return(i, lag))
            .unwrap_or(0.0)
    };
    let vix = at(&ctx.vix);
    [
        log_return(&ctx.sp500, 1),
        log_return(&ctx.sp500, 5),
        log_return(&ctx.nasdaq, 1),
        vix.map_or(NEUTRAL_VIX, |i| ctx.vix.value(i)).ln(),
        vix.and_then(|i| ctx.vix.log_return(i, 5)).unwrap_or(0.0),
        at(&ctx.us10y)
            .and_then(|i| ctx.us10y.difference(i, 5))
            .unwrap_or(0.0),
        log_return(&ctx.usdkrw, 1),
        log_return(&ctx.usdkrw, 5),
    ]
}

// Second set of overseas features, read strictly before `date` like `macro_features`.
fn extra_macro_features(ctx: &MarketContext, date: NaiveDate) -> [f64; 10] {
    let at = |series: &DailySeries| series.index_at(date, true, MACRO_MAX_STALE_DAYS);
    let ret5 = |series: &DailySeries| {
        at(series)
            .and_then(|i| series.log_return(i, 5))
            .unwrap_or(0.0)
    };
    let chg5 = |series: &DailySeries| {
        at(series)
            .and_then(|i| series.difference(i, 5))
            .unwrap_or(0.0)
    };
    let level = |series: &DailySeries| at(series).map(|i| series.value(i));
    // Baa corporate yield over the 10-year Treasury, on the latest date both exist.
    let spread_at = |back: usize| -> Option<f64> {
        let i = at(&ctx.baa_yield)?;
        let i = i.checked_sub(back)?;
        let day = ctx.baa_yield.date(i);
        let j = ctx.us10y.index_at(day, false, MACRO_MAX_STALE_DAYS)?;
        Some(ctx.baa_yield.value(i) - ctx.us10y.value(j))
    };
    let spread = spread_at(0);
    let vxn = level(&ctx.nasdaq_vol)
        .filter(|v| *v > 0.0)
        .unwrap_or(NEUTRAL_VIX);
    [
        ret5(&ctx.wti),
        ret5(&ctx.dollar),
        ret5(&ctx.usdjpy),
        ret5(&ctx.usdcny),
        chg5(&ctx.us2y),
        level(&ctx.term_spread).unwrap_or(0.0),
        spread.unwrap_or(0.0),
        spread
            .zip(spread_at(5))
            .map_or(0.0, |(now, before)| now - before),
        vxn.ln(),
        chg5(&ctx.breakeven),
    ]
}

/// Extract clean samples from chronological bars for a single symbol.
///
/// `bars` should be corporate-action adjusted; `market_category` selects the index.
/// Feature row for the close of `bars[t]`, computed only from `bars[..=t]` and context
/// known by then. `extras` may be empty (size and liquidity features stay neutral).
/// `None` before the warm-up or on a non-positive close.
pub fn features_at(
    symbol: &str,
    bars: &[Bar],
    extras: &[BarExtra],
    t: usize,
    index: &DailySeries,
    ctx: &MarketContext,
) -> Option<Vec<f64>> {
    if t < WARMUP || t >= bars.len() {
        return None;
    }
    let curr = &bars[t];
    let close_t = curr.close;
    if close_t <= 0.0 {
        return None;
    }

    let log_ret = |days: usize| -> f64 {
        let prev = bars[t - days].close;
        if prev > 0.0 {
            (close_t / prev).ln()
        } else {
            0.0
        }
    };
    let daily_ret = |i: usize| -> Option<f64> {
        let p1 = bars[i - 1].close;
        (p1 > 0.0).then(|| (bars[i].close / p1).ln())
    };

    // Volatility of 1-day returns
    let vol = |days: usize| -> f64 {
        let rets: Vec<f64> = ((t - days + 1)..=t).filter_map(daily_ret).collect();
        if rets.len() < 2 {
            return 0.0;
        }
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        var.sqrt()
    };

    let sma = |days: usize| -> f64 {
        bars[(t - days + 1)..=t]
            .iter()
            .map(|b| b.close)
            .sum::<f64>()
            / days as f64
    };
    let ratio_to = |level: f64| {
        if level > 0.0 {
            close_t / level - 1.0
        } else {
            0.0
        }
    };

    let log_ret_5 = log_ret(5);
    let log_ret_20 = log_ret(20);

    let intraday_range = (curr.high - curr.low) / close_t;
    let close_to_open = if curr.open > 0.0 {
        (curr.close - curr.open) / curr.open
    } else {
        0.0
    };

    let mean_volume = |days: usize| -> f64 {
        bars[(t + 1 - days)..=t]
            .iter()
            .map(|b| b.volume as f64)
            .sum::<f64>()
            / days as f64
    };
    let (vol_sma_20, vol_sma_5) = (mean_volume(20), mean_volume(5));
    let volume_ratio_20 = if vol_sma_20 > 0.0 {
        curr.volume as f64 / vol_sma_20 - 1.0
    } else {
        0.0
    };
    let volume_momentum = if vol_sma_20 > 0.0 {
        vol_sma_5 / vol_sma_20 - 1.0
    } else {
        0.0
    };

    let [mkt_ret_1, mkt_ret_5, mkt_ret_20, mkt_vol_20] = index_features(index, curr.date);

    let mut features = vec![
        log_ret(1),
        log_ret(2),
        log_ret(3),
        log_ret_5,
        log_ret(10),
        log_ret_20,
        vol(5),
        vol(20),
        ratio_to(sma(5)),
        ratio_to(sma(20)),
        ratio_to(sma(60)),
        intraday_range,
        close_to_open,
        volume_ratio_20,
        volume_momentum,
        mkt_ret_1,
        mkt_ret_5,
        mkt_ret_20,
        mkt_vol_20,
        log_ret_5 - mkt_ret_5,
        log_ret_20 - mkt_ret_20,
    ];
    features.extend_from_slice(&macro_features(ctx, curr.date));
    features.extend_from_slice(
        &ctx.disclosures
            .features(symbol, bars[t - 1].date, curr.date),
    );

    // Longer-range price position.
    let year = &bars[t.saturating_sub(YEAR_SESSIONS)..=t];
    let high = year
        .iter()
        .map(|b| b.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let low = year.iter().map(|b| b.low).fold(f64::INFINITY, f64::min);
    let max_ret_20 = ((t - 19)..=t).filter_map(daily_ret).fold(0.0, f64::max);
    features.extend_from_slice(&[log_ret(60), ratio_to(high), ratio_to(low), max_ret_20]);

    // Size and liquidity from unadjusted same-day values.
    let extra = extras.get(t).copied().unwrap_or_default();
    let window = |f: &dyn Fn(usize) -> Option<f64>| -> Option<f64> {
        let values: Vec<f64> = ((t - 19)..=t).filter_map(f).collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let turnover = window(&|i| {
        let e = extras.get(i)?;
        (e.market_cap > 0.0).then(|| e.trading_value / e.market_cap)
    });
    let amihud = window(&|i| {
        let e = extras.get(i)?;
        let r = daily_ret(i)?;
        (e.trading_value > 0.0).then(|| r.abs() / e.trading_value * 1e9)
    });
    features.extend_from_slice(&[
        if extra.market_cap > 0.0 {
            extra.market_cap.ln()
        } else {
            0.0
        },
        turnover.unwrap_or(0.0),
        amihud.map_or(0.0, f64::ln_1p),
    ]);

    features.extend_from_slice(&extra_macro_features(ctx, curr.date));
    features.extend_from_slice(
        &ctx.fundamentals
            .features(symbol, curr.date, extra.market_cap),
    );
    debug_assert_eq!(features.len(), NUM_FEATURES);
    Some(features)
}

/// Extract clean samples from chronological bars for a single symbol.
///
/// `bars` should be corporate-action adjusted; `market_category` selects the index.
pub fn extract_symbol_samples(
    symbol: &str,
    bars: &[Bar],
    extras: &[BarExtra],
    market_category: &str,
    ctx: &MarketContext,
) -> Vec<Sample> {
    let index = ctx.index_for(market_category);
    let max_horizon = *HORIZONS.iter().max().unwrap_or(&20);
    if bars.len() < WARMUP + max_horizon {
        return Vec::new();
    }

    let mut samples = Vec::new();
    for t in WARMUP..(bars.len() - max_horizon) {
        // Safety net for jumps the base-price adjustment could not explain
        let split_distorted = ((t + 1)..=(t + max_horizon)).any(|k| {
            let ratio = bars[k].close / bars[k - 1].close;
            !(0.6..=1.6).contains(&ratio)
        });
        if split_distorted {
            continue;
        }
        let Some(features) = features_at(symbol, bars, extras, t, index, ctx) else {
            continue;
        };
        let close_t = bars[t].close;
        let forward_returns: Vec<f64> = HORIZONS
            .iter()
            .map(|&h| (bars[t + h].close - close_t) / close_t)
            .collect();

        samples.push(Sample {
            symbol: symbol.to_owned(),
            date: bars[t].date,
            features,
            forward_returns,
            target_end: bars[t + max_horizon].date,
            trading_value: extras.get(t).map_or(0.0, |e| e.trading_value),
        });
    }

    samples
}

/// Chronological dataset split into Train, Validation, and Test partitions.
#[derive(Debug, Clone)]
pub struct DatasetSplits {
    pub train: Vec<Sample>,
    pub val: Vec<Sample>,
    pub test: Vec<Sample>,
    pub scaler: StandardScaler,
}

impl DatasetSplits {
    pub fn split_and_scale(
        mut all_samples: Vec<Sample>,
        val_start: NaiveDate,
        test_start: NaiveDate,
    ) -> Self {
        all_samples.sort_by_key(|s| s.date);

        let mut train = Vec::new();
        let mut val = Vec::new();
        let mut test = Vec::new();

        // Purge rows whose targets reach into the next split, so no training label
        // overlaps the returns that validation (or test) rows are scored on.
        for sample in all_samples {
            if sample.target_end < val_start {
                train.push(sample);
            } else if sample.date >= val_start && sample.target_end < test_start {
                val.push(sample);
            } else if sample.date >= test_start {
                test.push(sample);
            }
        }

        // Fit scaler ONLY on train split
        let scaler = StandardScaler::fit(&train);

        for s in &mut train {
            scaler.transform(s);
        }
        for s in &mut val {
            scaler.transform(s);
        }
        for s in &mut test {
            scaler.transform(s);
        }

        DatasetSplits {
            train,
            val,
            test,
            scaler,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Days;

    use super::*;

    fn day(offset: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 1, 1)
            .and_then(|d| d.checked_add_days(Days::new(offset)))
            .expect("valid date")
    }

    fn bars(closes: &[f64]) -> Vec<Bar> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &close)| Bar {
                date: day(i as u64),
                open: close,
                high: close * 1.01,
                low: close * 0.99,
                close,
                adj_close: Some(close),
                volume: 1_000,
            })
            .collect()
    }

    fn feature(sample: &Sample, name: &str) -> f64 {
        let idx = FEATURE_NAMES
            .iter()
            .position(|n| *n == name)
            .expect("known feature");
        sample.features[idx]
    }

    #[test]
    fn every_sample_has_all_features_even_without_context() {
        let closes: Vec<f64> = (0..100).map(|i| 100.0 + i as f64).collect();
        let samples = extract_symbol_samples(
            "005930",
            &bars(&closes),
            &[],
            "KOSPI",
            &MarketContext::default(),
        );
        assert_eq!(samples.len(), 100 - 60 - 20);
        for s in &samples {
            assert_eq!(s.features.len(), NUM_FEATURES);
            assert!(s.features.iter().all(|v| v.is_finite()));
            assert_eq!(feature(s, "mkt_ret_1"), 0.0);
            assert_eq!(feature(s, "vix_log"), NEUTRAL_VIX.ln());
            assert!((feature(s, "rel_ret_5") - feature(s, "log_ret_5")).abs() < 1e-12);
        }
    }

    #[test]
    fn index_is_same_day_and_overseas_series_lag_one_day() {
        let closes: Vec<f64> = (0..100).map(|i| 100.0 + i as f64).collect();
        let level = |i: u64| 1000.0 * 1.001f64.powi(i as i32);
        let points = |n: u64| (0..n).map(move |i| (day(i), level(i)));
        let mut ctx = MarketContext {
            kospi: DailySeries::from_points(points(100)),
            sp500: DailySeries::from_points(points(100)),
            ..MarketContext::default()
        };
        // A jump dated on the first sample day must not reach that day's features.
        let jump_day = day(60);
        ctx.sp500 = DailySeries::from_points(
            points(100).map(|(d, v)| if d >= jump_day { (d, v * 2.0) } else { (d, v) }),
        );

        let samples = extract_symbol_samples("005930", &bars(&closes), &[], "KOSPI", &ctx);
        let first = &samples[0];
        assert_eq!(first.date, jump_day);
        assert!((feature(first, "mkt_ret_1") - 1.001f64.ln()).abs() < 1e-12);
        assert!((feature(first, "sp500_ret_1") - 1.001f64.ln()).abs() < 1e-12);
        let second = &samples[1];
        assert!((feature(second, "sp500_ret_1") - (2.0f64 * 1.001).ln()).abs() < 1e-12);
        assert_eq!(
            feature(first, "nasdaq_ret_1"),
            0.0,
            "empty series is neutral"
        );
    }

    #[test]
    fn excess_returns_subtract_the_same_date_median_per_horizon() {
        let sample = |symbol: &str, offset: u64, ret: f64| Sample {
            symbol: symbol.to_owned(),
            date: day(offset),
            features: vec![0.0; NUM_FEATURES],
            forward_returns: HORIZONS.iter().map(|&h| ret * h as f64).collect(),
            target_end: day(offset + 20),
            trading_value: 0.0,
        };
        let samples = vec![
            sample("A", 1, 0.01),
            sample("B", 0, 0.05),
            sample("C", 1, 0.04),
            sample("D", 1, -0.02),
            sample("E", 2, 0.03),
            sample("F", 2, 0.01),
        ];
        let excess = to_excess_returns(samples);
        // Day 0 has a single name and is dropped; day 1 median is A, day 2 is the midpoint.
        let got: Vec<(&str, f64)> = excess
            .iter()
            .map(|s| {
                (
                    s.symbol.as_str(),
                    (s.forward_returns[1] * 1e6).round() / 1e6,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("A", 0.0),
                ("C", 0.09),
                ("D", -0.09),
                ("E", 0.03),
                ("F", -0.03)
            ]
        );
    }

    #[test]
    fn features_do_not_depend_on_the_price_basis() {
        // Back-adjustment rescales all earlier bars by one factor; no feature may see it.
        let closes: Vec<f64> = (0..120)
            .map(|i| 100.0 + 10.0 * (i as f64 * 0.7).sin() + i as f64)
            .collect();
        let base = bars(&closes);
        let scaled: Vec<Bar> = base
            .iter()
            .map(|b| Bar {
                open: b.open * 0.2,
                high: b.high * 0.2,
                low: b.low * 0.2,
                close: b.close * 0.2,
                adj_close: b.adj_close.map(|c| c * 0.2),
                volume: b.volume * 5,
                ..*b
            })
            .collect();
        let ctx = MarketContext::default();
        let a = extract_symbol_samples("005930", &base, &[], "KOSPI", &ctx);
        let b = extract_symbol_samples("005930", &scaled, &[], "KOSPI", &ctx);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            for (u, v) in x.features.iter().zip(&y.features) {
                assert!((u - v).abs() < 1e-9);
            }
            for (u, v) in x.forward_returns.iter().zip(&y.forward_returns) {
                assert!((u - v).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn unexplained_jump_drops_only_the_samples_whose_targets_span_it() {
        let mut closes: Vec<f64> = vec![100.0; 120];
        for close in closes.iter_mut().skip(90) {
            *close = 20.0;
        }
        let samples = extract_symbol_samples(
            "005930",
            &bars(&closes),
            &[],
            "KOSPI",
            &MarketContext::default(),
        );
        // t in 60..100 are candidates; t in 70..=89 look across the jump at index 90.
        assert_eq!(samples.len(), 40 - 20);
        assert!(
            samples
                .iter()
                .all(|s| s.date < day(70) || s.date >= day(90))
        );
    }
}
