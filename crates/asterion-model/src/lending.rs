//! Stock-lending balance features (KIS per-stock daily lending), research panel only.
//! Lending balances proxy short interest; they are not the actual short-sale balance.
//!
//! The publication time of daily lending data is unverified, so the features at the
//! close of `date` use sessions up to the second-latest session before `date` (`s`).
//! Both features are value-based, so splits inside the window do not distort them.

use std::collections::HashMap;

use asterion_data::kis::LendingDay;
use chrono::NaiveDate;

pub const LENDING_FEATURE_NAMES: [&str; NUM_LENDING_FEATURES] =
    ["lending_level", "lending_flow_20"];
pub const NUM_LENDING_FEATURES: usize = 2;
const WINDOW: usize = 20;
// Sessions that must be cached up to `s`, and the calendar span they must fit in.
const COVER_SESSIONS: usize = 22;
const MAX_SPAN_DAYS: i64 = 45;
// Sessions skipped between `s` and the row date: `s` is the second-latest one before it.
const LAG: usize = 2;
// Balance amounts are reported in million KRW.
const AMOUNT_UNIT: f64 = 1e6;

#[derive(Debug, Clone, Default)]
struct Series {
    dates: Vec<NaiveDate>,
    // Balance amount per session (KRW).
    balance: Vec<f64>,
    // Prefix sums with a leading zero: trading value and net new lending value (KRW).
    value: Vec<f64>,
    flow: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct LendingIndex {
    by_code: HashMap<String, Series>,
}

impl LendingIndex {
    pub fn insert(&mut self, code: &str, days: &[LendingDay]) {
        let mut s = Series {
            dates: Vec::with_capacity(days.len()),
            balance: Vec::with_capacity(days.len()),
            value: vec![0.0],
            flow: vec![0.0],
        };
        for d in days {
            s.dates.push(d.date);
            s.balance.push(d.balance_amount.max(0.0) * AMOUNT_UNIT);
            let last = s.dates.len() - 1;
            let price = d.close.max(0.0);
            s.value.push(s.value[last] + d.volume.max(0.0) * price);
            s.flow
                .push(s.flow[last] + (d.new_shares - d.repaid_shares) * price);
        }
        self.by_code.insert(code.to_owned(), s);
    }

    pub fn len(&self) -> usize {
        self.by_code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }

    // Index of `s`, the second-latest cached session before `date`, when covered.
    fn anchor(&self, code: &str, date: NaiveDate) -> Option<(&Series, usize)> {
        let s = self.by_code.get(code)?;
        let before = s.dates.partition_point(|d| *d < date);
        let at = before.checked_sub(LAG)?;
        let first = (at + 1).checked_sub(COVER_SESSIONS)?;
        ((date - s.dates[first]).num_days() <= MAX_SPAN_DAYS).then_some((s, at))
    }

    /// True when the 22 sessions up to `s` are cached within 45 calendar days of `date`.
    pub fn covers(&self, code: &str, date: NaiveDate) -> bool {
        self.anchor(code, date).is_some()
    }

    /// `lending_level` (balance over mean 20-session trading value) and `lending_flow_20`
    /// (net new lending value over trading value), both over the 20 sessions ending at
    /// `s`; zero when not covered or when the trading value is zero.
    pub fn features(&self, code: &str, date: NaiveDate) -> [f64; NUM_LENDING_FEATURES] {
        let mut out = [0.0; NUM_LENDING_FEATURES];
        let Some((s, at)) = self.anchor(code, date) else {
            return out;
        };
        let (lo, hi) = (at + 1 - WINDOW, at + 1);
        let tv = s.value[hi] - s.value[lo];
        if tv > 0.0 {
            out[0] = s.balance[at] / (tv / WINDOW as f64);
            out[1] = (s.flow[hi] - s.flow[lo]) / tv;
        }
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

    fn lending(n: u64) -> Vec<LendingDay> {
        (0..n)
            .map(|i| LendingDay {
                date: day(i),
                balance_shares: 0.0,
                balance_amount: i as f64,
                new_shares: 2.0,
                repaid_shares: 1.0,
                volume: 10.0,
                close: 1000.0,
            })
            .collect()
    }

    #[test]
    fn features_skip_the_session_before_the_date() {
        let mut idx = LendingIndex::default();
        idx.insert("A", &lending(40));
        // 22 sessions up to `s` need 23 sessions before the date.
        assert!(!idx.covers("A", day(22)));
        assert!(idx.covers("A", day(23)));
        let f = idx.features("A", day(30));
        // s = session 28: balance 28 million over mean value 10 x 1000.
        assert!((f[0] - 28.0e6 / 1e4).abs() < 1e-9);
        // Net new lending 1 share x 1000 per session over 10 x 1000 per session.
        assert!((f[1] - 0.1).abs() < 1e-12);
        // Sessions 29 and 30 never enter: changing them leaves day 30 unchanged.
        let mut changed = lending(40);
        changed[29].balance_amount = 1e9;
        changed[29].volume = 1e9;
        changed[30].new_shares = 1e9;
        let mut idx2 = LendingIndex::default();
        idx2.insert("A", &changed);
        assert_eq!(idx2.features("A", day(30)), f);
        assert_eq!(idx.features("B", day(30)), [0.0; NUM_LENDING_FEATURES]);
    }

    #[test]
    fn halted_windows_give_zero_instead_of_dividing_by_zero() {
        let mut halted = lending(40);
        for d in &mut halted {
            d.volume = 0.0;
        }
        let mut idx = LendingIndex::default();
        idx.insert("A", &halted);
        assert_eq!(idx.features("A", day(30)), [0.0; NUM_LENDING_FEATURES]);
    }
}
