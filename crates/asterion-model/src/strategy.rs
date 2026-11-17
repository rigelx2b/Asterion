//! Avoid-the-bottom-decile check: an equal-weight liquid universe minus the names the
//! model ranks lowest, against the same universe, both charged for their own turnover.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::confidence::liquid;
use crate::features::{FEATURE_NAMES, Sample};
use crate::neural::Forecast;

pub const EXCLUDE_FRACTION: f64 = 0.1;
// Fewest liquid names on a date for a ranking to be used.
pub const MIN_NAMES: usize = 50;
const TRADING_DAYS: f64 = 250.0;

/// One holding period: each name's realised return and the names the model excludes.
#[derive(Debug, Clone, PartialEq)]
pub struct Period {
    pub returns: Vec<(String, f64)>,
    pub excluded: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeriodResult {
    pub excess_net: f64,
    pub excluded_minus_universe: f64,
    pub extra_turnover: f64,
    pub names: usize,
    pub excluded: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvoidSummary {
    pub horizon: usize,
    pub periods: usize,
    // Periods treated as independent when computing the t statistic.
    pub independent_periods: f64,
    pub mean_names: f64,
    pub mean_excluded: f64,
    // Strategy minus universe per holding period, after each side's turnover cost.
    pub mean_excess: f64,
    pub excess_win_rate: f64,
    pub t_stat: f64,
    pub annualized_excess: f64,
    // Gross return of the excluded names minus the universe, and how often it was negative.
    pub excluded_minus_universe: f64,
    pub excluded_under_rate: f64,
    pub mean_extra_turnover: f64,
}

type Weights<'a> = HashMap<&'a str, f64>;

fn weights(period: &Period, skip_excluded: bool) -> Weights<'_> {
    let names: Vec<&str> = period
        .returns
        .iter()
        .map(|(s, _)| s.as_str())
        .filter(|s| !skip_excluded || !period.excluded.contains(*s))
        .collect();
    let w = 1.0 / names.len().max(1) as f64;
    names.into_iter().map(|s| (s, w)).collect()
}

// One-way turnover between two target weight sets; drift inside a period is ignored.
fn turnover(prev: &HashMap<&str, f64>, next: &HashMap<&str, f64>) -> f64 {
    let mut total = 0.0;
    for (s, w) in next {
        total += (w - prev.get(s).copied().unwrap_or(0.0)).abs();
    }
    for (s, w) in prev {
        if !next.contains_key(s) {
            total += w;
        }
    }
    total / 2.0
}

fn weighted_return(period: &Period, w: &HashMap<&str, f64>) -> f64 {
    period
        .returns
        .iter()
        .map(|(s, r)| w.get(s.as_str()).copied().unwrap_or(0.0) * r)
        .sum()
}

/// Results of consecutive, non-overlapping periods; the first period carries no turnover.
pub fn run_chain(chain: &[Period], cost: f64) -> Vec<PeriodResult> {
    let mut out = Vec::with_capacity(chain.len());
    let mut prev: Option<(Weights, Weights)> = None;
    for p in chain {
        let ws = weights(p, true);
        let wu = weights(p, false);
        let (ts, tu) = match &prev {
            Some((ps, pu)) => (turnover(ps, &ws), turnover(pu, &wu)),
            None => (0.0, 0.0),
        };
        let rs = weighted_return(p, &ws) - ts * cost;
        let ru = weighted_return(p, &wu) - tu * cost;
        let ex: Vec<f64> = p
            .returns
            .iter()
            .filter(|(s, _)| p.excluded.contains(s))
            .map(|(_, r)| *r)
            .collect();
        let ex_mean = if ex.is_empty() {
            f64::NAN
        } else {
            ex.iter().sum::<f64>() / ex.len() as f64
        };
        out.push(PeriodResult {
            excess_net: rs - ru,
            excluded_minus_universe: ex_mean - weighted_return(p, &wu),
            extra_turnover: ts - tu,
            names: p.returns.len(),
            excluded: ex.len(),
        });
        prev = Some((ws, wu));
    }
    out
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// `overlap` is how many chains share each calendar period (the horizon when every
/// offset is chained, 1 for a single chain).
pub fn summarize(results: &[PeriodResult], horizon: usize, overlap: usize) -> AvoidSummary {
    let excess: Vec<f64> = results.iter().map(|r| r.excess_net).collect();
    let emu: Vec<f64> = results
        .iter()
        .map(|r| r.excluded_minus_universe)
        .filter(|v| v.is_finite())
        .collect();
    let m = mean(&excess);
    let sd = if excess.len() > 1 {
        (excess.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (excess.len() - 1) as f64).sqrt()
    } else {
        f64::NAN
    };
    let independent = excess.len() as f64 / overlap.max(1) as f64;
    let share = |v: &[f64], f: &dyn Fn(f64) -> bool| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().filter(|x| f(**x)).count() as f64 / v.len() as f64
        }
    };
    AvoidSummary {
        horizon,
        periods: results.len(),
        independent_periods: independent,
        mean_names: mean(&results.iter().map(|r| r.names as f64).collect::<Vec<_>>()),
        mean_excluded: mean(
            &results
                .iter()
                .map(|r| r.excluded as f64)
                .collect::<Vec<_>>(),
        ),
        mean_excess: m,
        excess_win_rate: share(&excess, &|x| x > 0.0),
        t_stat: m / (sd / independent.max(1.0).sqrt()),
        annualized_excess: m * TRADING_DAYS / horizon as f64,
        excluded_minus_universe: mean(&emu),
        excluded_under_rate: share(&emu, &|x| x < 0.0),
        mean_extra_turnover: mean(
            &results
                .iter()
                .skip(1)
                .map(|r| r.extra_turnover)
                .collect::<Vec<_>>(),
        ),
    }
}

/// Names ranked lowest by `score`, `EXCLUDE_FRACTION` of them, ties broken by name.
pub fn bottom(scored: &[(String, f64)]) -> HashSet<String> {
    let mut v: Vec<&(String, f64)> = scored.iter().collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let k = (scored.len() as f64 * EXCLUDE_FRACTION).floor() as usize;
    v.into_iter().take(k).map(|(s, _)| s.clone()).collect()
}

/// Every offset chained over liquid rows of `samples`, ranked by the raw forecast.
pub fn avoid_bottom(
    raw: &[Forecast],
    samples: &[Sample],
    h: usize,
    horizon: usize,
    cost: f64,
) -> AvoidSummary {
    avoid_lowest(samples, h, horizon, cost, &|i| raw[i].returns[h])
}

/// Control arm: exclude the most volatile tenth (20-day volatility), no model involved.
pub fn avoid_volatile(samples: &[Sample], h: usize, horizon: usize, cost: f64) -> AvoidSummary {
    let idx = control_feature_index();
    avoid_lowest(samples, h, horizon, cost, &|i| {
        samples[i].features.get(idx).map_or(0.0, |v| -v)
    })
}

pub fn control_feature_index() -> usize {
    FEATURE_NAMES
        .iter()
        .position(|n| *n == CONTROL_FEATURE)
        .expect("control feature is in the feature list")
}

pub const CONTROL_FEATURE: &str = "volatility_20";

// Every offset chained over liquid rows; each date excludes the lowest `score` tenth.
fn avoid_lowest(
    samples: &[Sample],
    h: usize,
    horizon: usize,
    cost: f64,
    score: &dyn Fn(usize) -> f64,
) -> AvoidSummary {
    let mut by_date: BTreeMap<NaiveDate, Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        if liquid(s) {
            by_date.entry(s.date).or_default().push(i);
        }
    }
    let periods: Vec<Period> = by_date
        .values()
        .filter(|rows| rows.len() >= MIN_NAMES)
        .map(|rows| {
            let scored: Vec<(String, f64)> = rows
                .iter()
                .map(|&i| (samples[i].symbol.clone(), score(i)))
                .collect();
            Period {
                excluded: bottom(&scored),
                returns: rows
                    .iter()
                    .map(|&i| (samples[i].symbol.clone(), samples[i].forward_returns[h]))
                    .collect(),
            }
        })
        .collect();
    let mut results = Vec::new();
    for offset in 0..horizon.min(periods.len()) {
        let chain: Vec<Period> = periods
            .iter()
            .skip(offset)
            .step_by(horizon)
            .cloned()
            .collect();
        results.extend(run_chain(&chain, cost));
    }
    summarize(&results, horizon, horizon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn period(rets: &[(&str, f64)], excluded: &[&str]) -> Period {
        Period {
            returns: rets.iter().map(|(s, r)| ((*s).to_owned(), *r)).collect(),
            excluded: excluded.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn excluding_a_loser_beats_the_universe_by_its_share() {
        let p = period(
            &[("a", 0.02), ("b", 0.02), ("c", 0.02), ("d", -0.06)],
            &["d"],
        );
        let r = run_chain(&[p], 0.0025)[0];
        // Universe 0.0, strategy +2%; no turnover on the first period.
        assert!((r.excess_net - 0.02).abs() < 1e-12);
        assert!((r.excluded_minus_universe + 0.06).abs() < 1e-12);
    }

    #[test]
    fn changing_the_excluded_name_costs_turnover() {
        let a = period(&[("a", 0.0), ("b", 0.0), ("c", 0.0), ("d", 0.0)], &["d"]);
        let b = period(&[("a", 0.0), ("b", 0.0), ("c", 0.0), ("d", 0.0)], &["a"]);
        let r = run_chain(&[a, b], 0.01);
        // Sell a (1/3) and buy d (1/3): one-way turnover 1/3, cost 1/3 of 1%.
        assert!((r[1].extra_turnover - 1.0 / 3.0).abs() < 1e-12);
        assert!((r[1].excess_net + 0.01 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn bottom_takes_the_lowest_tenth() {
        let scored: Vec<(String, f64)> = (0..20).map(|i| (format!("{i:02}"), i as f64)).collect();
        let b = bottom(&scored);
        assert_eq!(b.len(), 2);
        assert!(b.contains("00") && b.contains("01"));
    }
}
