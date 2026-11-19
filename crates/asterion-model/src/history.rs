//! Splices cached KIS daily bars in front of the data.go.kr series of one symbol.
//!
//! The two sources share an overlap (2020-01..2020-06). The price basis is estimated
//! from that overlap and the KIS bars are rescaled onto the data.go.kr basis; symbols
//! whose basis is not stable across the overlap keep their data.go.kr bars only.
//!
//! Index levels start in 2020 unless `--with-index-history` prepends KIS levels; without it
//! `mkt_*` are 0 before 2020 and `rel_ret_*` equal `log_ret_*`.

use asterion_core::Bar;
use asterion_data::kis::ChartBar;
use chrono::{Datelike, NaiveDate};

use crate::features::{BarExtra, FEATURE_NAMES, Sample};

// Overlapping sessions needed to estimate the price basis between the sources.
const MIN_OVERLAP: usize = 20;
// An overlap day agrees when its close ratio is within this of the median ratio
// (loose enough for integer rounding of adjusted low-priced stocks).
const AGREE_TOLERANCE: f64 = 0.01;
// Share of overlap days that must agree; a mid-overlap basis step fails this.
const MIN_AGREE_SHARE: f64 = 0.9;
// Largest calendar gap between the last KIS bar and the first data.go.kr bar.
const MAX_SPLICE_GAP_DAYS: i64 = 7;

/// What happened to one symbol's history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Splice {
    Spliced {
        prefix: usize,
        price_ratio: f64,
        volume_ratio: f64,
        value_ratio: f64,
    },
    NoHistory,
    ShortOverlap(usize),
    UnstableBasis {
        agree_share: f64,
    },
    Gap(i64),
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite() && *v > 0.0);
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

/// Prepends `history` (sorted, one bar per date) to the aligned `bars`/`extras`.
///
/// Market cap before the first data.go.kr session is approximated as the rescaled
/// close times the share count at that session. That share count is later information,
/// so spliced rows are fit for training windows only, never for scored test windows.
pub fn splice_history(
    history: &[ChartBar],
    bars: &mut Vec<Bar>,
    extras: &mut Vec<BarExtra>,
) -> Splice {
    debug_assert_eq!(bars.len(), extras.len());
    let Some(first) = bars.first().copied() else {
        return Splice::NoHistory;
    };
    let prefix_len = history.partition_point(|h| h.date < first.date);
    if prefix_len == 0 {
        return Splice::NoHistory;
    }

    let (mut price, mut volume, mut value) = (Vec::new(), Vec::new(), Vec::new());
    for h in &history[prefix_len..] {
        let Ok(i) = bars.binary_search_by_key(&h.date, |b| b.date) else {
            continue;
        };
        if h.close > 0.0 && bars[i].close > 0.0 {
            price.push(bars[i].close / h.close);
        }
        if h.volume > 0.0 {
            volume.push(bars[i].volume as f64 / h.volume);
        }
        if h.trading_value > 0.0 {
            value.push(extras[i].trading_value / h.trading_value);
        }
    }
    if price.len() < MIN_OVERLAP {
        return Splice::ShortOverlap(price.len());
    }
    let Some(price_ratio) = median(price.clone()) else {
        return Splice::ShortOverlap(0);
    };
    let agree = price
        .iter()
        .filter(|r| (*r / price_ratio - 1.0).abs() <= AGREE_TOLERANCE)
        .count();
    let agree_share = agree as f64 / price.len() as f64;
    if agree_share < MIN_AGREE_SHARE {
        return Splice::UnstableBasis { agree_share };
    }
    let volume_ratio = median(volume).unwrap_or(1.0 / price_ratio);
    let value_ratio = median(value).unwrap_or(1.0);

    let shares = (extras[0].market_cap > 0.0 && first.close > 0.0)
        .then(|| extras[0].market_cap / first.close);
    let mut prefix_bars = Vec::with_capacity(prefix_len);
    let mut prefix_extras = Vec::with_capacity(prefix_len);
    for h in &history[..prefix_len] {
        let close = h.close * price_ratio;
        let bar = Bar {
            date: h.date,
            open: h.open * price_ratio,
            high: h.high * price_ratio,
            low: h.low * price_ratio,
            close,
            adj_close: Some(close),
            volume: (h.volume.max(0.0) * volume_ratio).round() as u64,
        };
        if !bar.is_ohlc_consistent() {
            continue;
        }
        prefix_bars.push(bar);
        prefix_extras.push(BarExtra {
            market_cap: shares.map_or(0.0, |s| s * close),
            trading_value: h.trading_value.max(0.0) * value_ratio,
        });
    }
    let Some(last) = prefix_bars.last() else {
        return Splice::NoHistory;
    };
    let gap = (first.date - last.date).num_days();
    if gap > MAX_SPLICE_GAP_DAYS {
        return Splice::Gap(gap);
    }

    let prefix = prefix_bars.len();
    prefix_bars.append(bars);
    *bars = prefix_bars;
    prefix_extras.append(extras);
    *extras = prefix_extras;
    Splice::Spliced {
        prefix,
        price_ratio,
        volume_ratio,
        value_ratio,
    }
}

/// Per-symbol splice outcomes, summed over a load.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SpliceCounts {
    pub spliced: usize,
    pub prefix_bars: usize,
    pub no_history: usize,
    pub short_overlap: usize,
    pub unstable_basis: usize,
    pub gap: usize,
    // Symbols never visited by fetch-history, as opposed to visited with no older bars.
    pub missing_cache: usize,
}

impl SpliceCounts {
    pub fn add(&mut self, splice: Splice) {
        match splice {
            Splice::Spliced { prefix, .. } => {
                self.spliced += 1;
                self.prefix_bars += prefix;
            }
            Splice::NoHistory => self.no_history += 1,
            Splice::ShortOverlap(_) => self.short_overlap += 1,
            Splice::UnstableBasis { .. } => self.unstable_basis += 1,
            Splice::Gap(_) => self.gap += 1,
        }
    }
}

/// Feature groups whose pre-2020 coverage is checked by [`coverage_by_year`].
pub const COVERAGE_FEATURES: [&str; 5] = [
    "log_mcap",
    "turnover_20",
    "fund_available",
    "dart_cnt_30",
    "mkt_ret_20",
];

/// Per calendar year: sample count and the share of samples with a non-zero value
/// for each of [`COVERAGE_FEATURES`].
pub fn coverage_by_year(samples: &[Sample]) -> Vec<(i32, usize, Vec<f64>)> {
    let columns: Vec<Option<usize>> = COVERAGE_FEATURES
        .iter()
        .map(|name| FEATURE_NAMES.iter().position(|n| n == name))
        .collect();
    let mut years: std::collections::BTreeMap<i32, (usize, Vec<usize>)> =
        std::collections::BTreeMap::new();
    for s in samples {
        let entry = years
            .entry(s.date.year())
            .or_insert_with(|| (0, vec![0; columns.len()]));
        entry.0 += 1;
        for (k, column) in columns.iter().enumerate() {
            if column.is_some_and(|j| s.features[j] != 0.0) {
                entry.1[k] += 1;
            }
        }
    }
    years
        .into_iter()
        .map(|(year, (n, nonzero))| {
            let shares = nonzero.iter().map(|&c| c as f64 / n as f64).collect();
            (year, n, shares)
        })
        .collect()
}

/// Drops samples dated before `from`.
pub fn samples_from(samples: Vec<Sample>, from: NaiveDate) -> Vec<Sample> {
    samples.into_iter().filter(|s| s.date >= from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(i: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2020, 1, 1).expect("date") + chrono::Days::new(i as u64)
    }

    fn chart(i: i64, close: f64) -> ChartBar {
        ChartBar {
            date: day(i),
            open: close,
            high: close * 1.01,
            low: close * 0.99,
            close,
            volume: 1000.0,
            trading_value: 1000.0 * close,
        }
    }

    // data.go.kr side from day `start`: closes on a basis twice the KIS one.
    fn recent(start: i64, end: i64) -> (Vec<Bar>, Vec<BarExtra>) {
        let bars: Vec<Bar> = (start..end)
            .map(|i| {
                let close = 2.0 * (100.0 + i as f64);
                Bar {
                    date: day(i),
                    open: close,
                    high: close * 1.01,
                    low: close * 0.99,
                    close,
                    adj_close: Some(close),
                    volume: 500,
                }
            })
            .collect();
        let extras = bars
            .iter()
            .map(|b| BarExtra {
                market_cap: b.close * 10.0,
                trading_value: 500.0 * b.close,
            })
            .collect();
        (bars, extras)
    }

    #[test]
    fn prefix_is_rescaled_onto_the_recent_basis() {
        let history: Vec<ChartBar> = (0..60).map(|i| chart(i, 100.0 + i as f64)).collect();
        let (mut bars, mut extras) = recent(30, 90);
        let splice = splice_history(&history, &mut bars, &mut extras);
        let Splice::Spliced {
            prefix,
            price_ratio,
            volume_ratio,
            value_ratio,
        } = splice
        else {
            panic!("expected a splice, got {splice:?}");
        };
        assert_eq!(prefix, 30);
        assert!((price_ratio - 2.0).abs() < 1e-12);
        assert!((volume_ratio - 0.5).abs() < 1e-12);
        assert!((value_ratio - 1.0).abs() < 1e-12);
        assert_eq!(bars.len(), 90);
        assert_eq!(extras.len(), 90);
        assert_eq!(bars[0].date, day(0));
        assert!((bars[0].close - 200.0).abs() < 1e-9);
        assert_eq!(bars[0].volume, 500);
        // Share count 10 at the first data.go.kr session, applied to the rescaled close.
        assert!((extras[0].market_cap - 2000.0).abs() < 1e-9);
        assert!((extras[0].trading_value - 100_000.0).abs() < 1e-9);
        assert!(bars.windows(2).all(|w| w[0].date < w[1].date));
    }

    #[test]
    fn a_basis_step_inside_the_overlap_is_rejected() {
        let history: Vec<ChartBar> = (0..60)
            .map(|i| {
                let base = 100.0 + i as f64;
                chart(i, if i < 45 { base } else { base * 5.0 })
            })
            .collect();
        let (mut bars, mut extras) = recent(30, 90);
        let before = bars.clone();
        let splice = splice_history(&history, &mut bars, &mut extras);
        assert!(matches!(splice, Splice::UnstableBasis { .. }), "{splice:?}");
        assert_eq!(bars, before);
    }

    #[test]
    fn short_overlap_and_gaps_keep_the_recent_bars_only() {
        let history: Vec<ChartBar> = (0..40).map(|i| chart(i, 100.0 + i as f64)).collect();
        let (mut bars, mut extras) = recent(30, 90);
        assert_eq!(
            splice_history(&history, &mut bars, &mut extras),
            Splice::ShortOverlap(10)
        );
        assert_eq!(bars.len(), 60);

        let mut history: Vec<ChartBar> = (0..10).map(|i| chart(i, 100.0 + i as f64)).collect();
        history.extend((30..60).map(|i| chart(i, 100.0 + i as f64)));
        let (mut bars, mut extras) = recent(30, 90);
        assert_eq!(
            splice_history(&history, &mut bars, &mut extras),
            Splice::Gap(21)
        );
        assert_eq!(bars.len(), 60);

        let (mut bars, mut extras) = recent(30, 90);
        assert_eq!(
            splice_history(&[], &mut bars, &mut extras),
            Splice::NoHistory
        );
    }
}
