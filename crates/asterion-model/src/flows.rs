//! Investor-type flow features (KIS per-stock daily net buying), research panel only.
//!
//! A day's investor breakdown is published after the close, so the features at the
//! close of `date` use sessions strictly before `date`. Net buying is scaled by the
//! same sessions' unadjusted trading value from the same response, so no price basis
//! enters.

use std::collections::HashMap;

use asterion_data::kis::InvestorDay;
use chrono::NaiveDate;

pub const FLOW_FEATURE_NAMES: [&str; NUM_FLOW_FEATURES] = [
    "flow_foreign_5",
    "flow_foreign_20",
    "flow_inst_5",
    "flow_inst_20",
];
pub const NUM_FLOW_FEATURES: usize = 4;
const SHORT: usize = 5;
const LONG: usize = 20;
// Net-buy values are reported in million KRW, trading value in KRW.
const VALUE_UNIT: f64 = 1e6;
// A window whose oldest session is older than this many calendar days counts as missing.
const MAX_SPAN_DAYS: i64 = 40;

#[derive(Debug, Clone, Default)]
struct Series {
    dates: Vec<NaiveDate>,
    // Prefix sums with a leading zero: foreign net, institution net, trading value (KRW).
    foreign: Vec<f64>,
    institution: Vec<f64>,
    value: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct FlowIndex {
    by_code: HashMap<String, Series>,
}

impl FlowIndex {
    pub fn insert(&mut self, code: &str, days: &[InvestorDay]) {
        let mut s = Series {
            dates: Vec::with_capacity(days.len()),
            foreign: vec![0.0],
            institution: vec![0.0],
            value: vec![0.0],
        };
        for d in days {
            s.dates.push(d.date);
            let last = s.dates.len() - 1;
            s.foreign
                .push(s.foreign[last] + d.foreign_net_value * VALUE_UNIT);
            s.institution
                .push(s.institution[last] + d.institution_net_value * VALUE_UNIT);
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

    /// Foreign and institution net buying over the last 5 and 20 sessions before `date`,
    /// each over the same sessions' trading value; zero when not covered.
    pub fn features(&self, code: &str, date: NaiveDate) -> [f64; NUM_FLOW_FEATURES] {
        let mut out = [0.0; NUM_FLOW_FEATURES];
        if !self.covers(code, date) {
            return out;
        }
        let Some(s) = self.by_code.get(code) else {
            return out;
        };
        let hi = s.dates.partition_point(|d| *d < date);
        let ratio = |sum: &[f64], n: usize| {
            let tv = s.value[hi] - s.value[hi - n];
            if tv > 0.0 {
                (sum[hi] - sum[hi - n]) / tv
            } else {
                0.0
            }
        };
        out[0] = ratio(&s.foreign, SHORT);
        out[1] = ratio(&s.foreign, LONG);
        out[2] = ratio(&s.institution, SHORT);
        out[3] = ratio(&s.institution, LONG);
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

    fn flows(n: u64) -> Vec<InvestorDay> {
        (0..n)
            .map(|i| InvestorDay {
                date: day(i),
                foreign_net_qty: 0.0,
                institution_net_qty: 0.0,
                individual_net_qty: 0.0,
                foreign_net_value: i as f64,
                institution_net_value: -1.0,
                individual_net_value: 0.0,
                volume: 0.0,
                trading_value: 1e7,
            })
            .collect()
    }

    #[test]
    fn features_use_sessions_strictly_before_the_date() {
        let mut idx = FlowIndex::default();
        idx.insert("A", &flows(30));
        assert!(!idx.covers("A", day(19)));
        assert!(idx.covers("A", day(20)));
        let f = idx.features("A", day(25));
        // Sessions 20..=24: foreign 20+21+22+23+24 = 110 million over 5 x 10 million.
        assert!((f[0] - 110.0 / 5.0 * 1e6 / 1e7).abs() < 1e-12);
        assert!((f[2] + 1e6 / 1e7).abs() < 1e-12);
        // The day's own flow never enters: changing session 25 leaves day 25 unchanged.
        let mut changed = flows(30);
        changed[25].foreign_net_value = 1e9;
        let mut idx2 = FlowIndex::default();
        idx2.insert("A", &changed);
        assert_eq!(idx2.features("A", day(25)), f);
        assert_eq!(idx.features("B", day(25)), [0.0; NUM_FLOW_FEATURES]);
    }
}
