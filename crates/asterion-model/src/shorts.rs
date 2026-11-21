//! Short-sale trading features (KIS per-stock daily short-sale trading), research panel
//! only. These are traded short volumes, not outstanding short balances.
//!
//! A day's short-sale trading is published after the close, so the features at the
//! close of `date` use sessions strictly before `date`. Short-sale value is scaled by
//! the same sessions' trading value from the same response (both in KRW).

use std::collections::HashMap;

use asterion_data::kis::ShortSaleDay;
use chrono::NaiveDate;

pub const SHORT_FEATURE_NAMES: [&str; NUM_SHORT_FEATURES] = ["shortsale_5", "shortsale_20"];
pub const NUM_SHORT_FEATURES: usize = 2;
const SHORT: usize = 5;
const LONG: usize = 20;
// A window whose oldest session is older than this many calendar days counts as missing.
const MAX_SPAN_DAYS: i64 = 40;

#[derive(Debug, Clone, Default)]
struct Series {
    dates: Vec<NaiveDate>,
    // Prefix sums with a leading zero: short-sale value and trading value (KRW).
    short: Vec<f64>,
    value: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ShortIndex {
    by_code: HashMap<String, Series>,
}

impl ShortIndex {
    pub fn insert(&mut self, code: &str, days: &[ShortSaleDay]) {
        let mut s = Series {
            dates: Vec::with_capacity(days.len()),
            short: vec![0.0],
            value: vec![0.0],
        };
        for d in days {
            s.dates.push(d.date);
            let last = s.dates.len() - 1;
            s.short.push(s.short[last] + d.short_value.max(0.0));
            s.value.push(s.value[last] + d.trading_value.max(0.0));
        }
        self.by_code.insert(code.to_owned(), s);
    }

    pub fn len(&self) -> usize {
        self.by_code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }

    /// True when the 20 sessions before `date` are all cached and recent.
    pub fn covers(&self, code: &str, date: NaiveDate) -> bool {
        self.by_code.get(code).is_some_and(|s| {
            let hi = s.dates.partition_point(|d| *d < date);
            hi >= LONG && (date - s.dates[hi - LONG]).num_days() <= MAX_SPAN_DAYS
        })
    }

    /// Short-sale value over the last 5 and 20 sessions before `date`, each over the
    /// same sessions' trading value; zero when not covered.
    pub fn features(&self, code: &str, date: NaiveDate) -> [f64; NUM_SHORT_FEATURES] {
        let mut out = [0.0; NUM_SHORT_FEATURES];
        if !self.covers(code, date) {
            return out;
        }
        let Some(s) = self.by_code.get(code) else {
            return out;
        };
        let hi = s.dates.partition_point(|d| *d < date);
        let ratio = |n: usize| {
            let tv = s.value[hi] - s.value[hi - n];
            if tv > 0.0 {
                (s.short[hi] - s.short[hi - n]) / tv
            } else {
                0.0
            }
        };
        out[0] = ratio(SHORT);
        out[1] = ratio(LONG);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(i: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2024, 1, 1)
            .and_then(|d| d.checked_add_days(chrono::Days::new(i)))
            .expect("date")
    }

    fn shorts(n: u64) -> Vec<ShortSaleDay> {
        (0..n)
            .map(|i| ShortSaleDay {
                date: day(i),
                short_qty: 0.0,
                short_value: i as f64,
                volume: 0.0,
                trading_value: 100.0,
            })
            .collect()
    }

    #[test]
    fn features_use_sessions_strictly_before_the_date() {
        let mut idx = ShortIndex::default();
        idx.insert("A", &shorts(30));
        assert!(!idx.covers("A", day(19)));
        assert!(idx.covers("A", day(20)));
        let f = idx.features("A", day(25));
        // Sessions 20..=24: 20+21+22+23+24 = 110 over 5 x 100.
        assert!((f[0] - 110.0 / 500.0).abs() < 1e-12);
        // Sessions 5..=24: sum 290 over 20 x 100.
        assert!((f[1] - 290.0 / 2000.0).abs() < 1e-12);
        // The day's own short sales never enter: changing session 25 leaves day 25 unchanged.
        let mut changed = shorts(30);
        changed[25].short_value = 1e9;
        let mut idx2 = ShortIndex::default();
        idx2.insert("A", &changed);
        assert_eq!(idx2.features("A", day(25)), f);
        assert_eq!(idx.features("B", day(25)), [0.0; NUM_SHORT_FEATURES]);
    }
}
