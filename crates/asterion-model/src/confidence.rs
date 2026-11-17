//! Selective forecasts, empirical forecast intervals and a net-of-cost profit check.
//!
//! Everything here is fitted on validation B forecasts after calibration and only read
//! on later data: per error-scale bucket, the quantiles of the signed log residual in
//! units of the predicted error scale (split conformal), the realised share of forecasts
//! within 1% of the price, and the widest error-scale cutoff whose hit rate still clears
//! the target with a date-block lower bound.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::features::Sample;
use crate::metrics::{HORIZONS, price_error};
use crate::neural::Forecast;
use crate::strategy::{AvoidSummary, avoid_bottom, avoid_volatile};

const NUM_HORIZONS: usize = HORIZONS.len();
pub const HIT_BAND: f64 = 0.01;
pub const TARGET_HIT_RATE: f64 = 0.80;
pub const INTERVAL_LEVEL: f64 = 0.80;
// Same-day trading value (KRW) below which a row is a halt or too thin to trade.
pub const MIN_TRADING_VALUE: f64 = 1e8;
// Liquidity floor (KRW per day) for rows that the profit checks may trade.
pub const LIQUID_TRADING_VALUE: f64 = 1e9;
// Round trip: 0.20% securities transaction tax on the sale plus fees and slippage.
pub const DEFAULT_COST: f64 = 0.0025;
const BUCKETS: usize = 10;
// Residual quantiles stored at probabilities (i + 1) / (GRID + 1).
const GRID: usize = 199;
const MIN_SELECTED: usize = 2000;
const CUTOFF_STEPS: usize = 400;
pub const CURVE_COVERAGES: [f64; 9] = [0.01, 0.02, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 1.0];
pub const INTERVAL_LEVELS: [f64; 3] = [0.5, 0.8, 0.9];
pub const SIGNAL_CONFIDENCES: [f64; 4] = [0.9, 0.8, 0.7, 0.6];
const TRADING_DAYS: f64 = 250.0;

pub fn tradeable(s: &Sample) -> bool {
    s.trading_value >= MIN_TRADING_VALUE
}

pub fn liquid(s: &Sample) -> bool {
    s.trading_value >= LIQUID_TRADING_VALUE
}

// Signed log residual of the realised price per unit of predicted error scale.
fn residual(f: &Forecast, h: usize, actual: f64) -> Option<f64> {
    let s = f.error_scale[h];
    let z = ((1.0 + actual) / (1.0 + f.returns[h])).ln() / s;
    (s > 0.0 && z.is_finite()).then_some(z)
}

fn quantile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn std_dev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return f64::NAN;
    }
    let m = mean(values);
    (values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (values.len() - 1) as f64).sqrt()
}

fn sorted(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonConfidence {
    pub horizon: usize,
    // Upper error-scale edge of every bucket but the last; buckets hold equal row counts.
    pub bucket_edges: Vec<f64>,
    pub residual_quantiles: Vec<Vec<f64>>,
    pub bucket_hit_rates: Vec<f64>,
    // Error-scale cutoff for each of `CURVE_COVERAGES` on the fit rows.
    pub coverage_scales: Vec<f64>,
    // Forecasts at or below this error scale are issued; `None` when no cutoff qualifies.
    pub max_scale: Option<f64>,
    pub fit_coverage: f64,
    pub fit_hit_rate: Option<f64>,
    pub fit_lower_bound: Option<f64>,
}

impl HorizonConfidence {
    fn bucket(&self, scale: f64) -> usize {
        self.bucket_edges
            .iter()
            .position(|&e| scale <= e)
            .unwrap_or(self.bucket_edges.len())
    }

    pub fn residual_quantile(&self, scale: f64, p: f64) -> f64 {
        let q = &self.residual_quantiles[self.bucket(scale)];
        let x = (p * (GRID + 1) as f64 - 1.0).clamp(0.0, (GRID - 1) as f64);
        let lo = x.floor() as usize;
        let hi = x.ceil() as usize;
        q[lo] + (q[hi] - q[lo]) * (x - lo as f64)
    }

    // Return at which the realised return falls below with probability `p`.
    pub fn return_quantile(&self, predicted: f64, scale: f64, p: f64) -> f64 {
        (1.0 + predicted) * (self.residual_quantile(scale, p) * scale).exp() - 1.0
    }

    pub fn interval(&self, predicted: f64, scale: f64, level: f64) -> (f64, f64) {
        let tail = (1.0 - level) / 2.0;
        (
            self.return_quantile(predicted, scale, tail),
            self.return_quantile(predicted, scale, 1.0 - tail),
        )
    }

    pub fn hit_probability(&self, scale: f64) -> f64 {
        self.bucket_hit_rates[self.bucket(scale)]
    }

    pub fn selects(&self, scale: f64) -> bool {
        self.max_scale.is_some_and(|m| scale <= m)
    }

    // Probability that the realised return exceeds `threshold`, read off the residual grid.
    pub fn probability_above(&self, predicted: f64, scale: f64, threshold: f64) -> f64 {
        let z = ((1.0 + threshold) / (1.0 + predicted)).ln() / scale;
        let q = &self.residual_quantiles[self.bucket(scale)];
        let p = |i: usize| (i + 1) as f64 / (GRID + 1) as f64;
        if z <= q[0] {
            return 1.0 - p(0);
        }
        if z >= q[GRID - 1] {
            return 1.0 - p(GRID - 1);
        }
        let i = q.partition_point(|&v| v < z).clamp(1, GRID - 1);
        let span = q[i] - q[i - 1];
        let t = if span > 0.0 {
            (z - q[i - 1]) / span
        } else {
            0.5
        };
        1.0 - (p(i - 1) + t * (p(i) - p(i - 1)))
    }

    /// Interval, hit probability and signals for one calibrated forecast.
    pub fn view(
        &self,
        predicted: f64,
        scale: f64,
        close: f64,
        trading_value: f64,
        cost: f64,
    ) -> ForecastConfidence {
        let (lo, hi) = self.interval(predicted, scale, INTERVAL_LEVEL);
        let tradeable = trading_value >= MIN_TRADING_VALUE;
        let profit_probability = self.probability_above(predicted, scale, cost);
        ForecastConfidence {
            interval_level: INTERVAL_LEVEL,
            price_low: close * (1.0 + lo),
            price_high: close * (1.0 + hi),
            hit_probability: self.hit_probability(scale),
            issued: tradeable && self.selects(scale),
            tradeable,
            cost,
            profit_probability,
            profit_signal: trading_value >= LIQUID_TRADING_VALUE
                && profit_probability >= PROFIT_SIGNAL_CONFIDENCE,
        }
    }
}

// One-sided confidence a live profit signal needs: the 80% interval's lower end above cost.
pub const PROFIT_SIGNAL_CONFIDENCE: f64 = 0.9;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ForecastConfidence {
    pub interval_level: f64,
    pub price_low: f64,
    pub price_high: f64,
    // Validation share of forecasts in the same error-scale bucket within 1% of the price.
    pub hit_probability: f64,
    // Passed the selective cutoff, so the point price is meant to be used.
    pub issued: bool,
    pub tradeable: bool,
    pub cost: f64,
    pub profit_probability: f64,
    pub profit_signal: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceModel {
    pub fit_from: NaiveDate,
    pub fit_to: NaiveDate,
    pub fit_rows: usize,
    pub hit_band: f64,
    pub target_hit_rate: f64,
    pub horizons: Vec<HorizonConfidence>,
}

struct FitRow {
    scale: f64,
    z: f64,
    hit: bool,
    block: usize,
}

// Date-block mean hit rate minus two standard errors, blocks weighted equally.
fn block_lower_bound(rows: &[FitRow], blocks: usize) -> (f64, f64) {
    let mut hits = vec![0usize; blocks];
    let mut counts = vec![0usize; blocks];
    for r in rows {
        counts[r.block] += 1;
        hits[r.block] += usize::from(r.hit);
    }
    let rates: Vec<f64> = hits
        .iter()
        .zip(&counts)
        .filter(|(_, n)| **n >= 20)
        .map(|(h, n)| *h as f64 / *n as f64)
        .collect();
    let pooled = rows.iter().filter(|r| r.hit).count() as f64 / rows.len().max(1) as f64;
    if rates.len() < 3 {
        return (pooled, f64::NEG_INFINITY);
    }
    let lb = mean(&rates) - 2.0 * std_dev(&rates) / (rates.len() as f64).sqrt();
    (pooled, lb)
}

impl ConfidenceModel {
    /// Fits on calibrated `forecasts` of `samples`, using tradeable rows only.
    pub fn fit(forecasts: &[Forecast], samples: &[Sample]) -> Option<ConfidenceModel> {
        let idx: Vec<usize> = (0..samples.len())
            .filter(|&i| tradeable(&samples[i]))
            .collect();
        if idx.len() < MIN_SELECTED * 2 {
            return None;
        }
        let dates: Vec<NaiveDate> = sorted_dates(idx.iter().map(|&i| samples[i].date));
        let rank: HashMap<NaiveDate, usize> =
            dates.iter().enumerate().map(|(r, d)| (*d, r)).collect();
        let horizons = HORIZONS
            .iter()
            .enumerate()
            .map(|(h, &horizon)| {
                // Blocks of at least `horizon` sessions so overlapping targets share a block.
                let block_len = horizon.max(5);
                let blocks = dates.len().div_ceil(block_len);
                let mut rows: Vec<FitRow> = idx
                    .iter()
                    .filter_map(|&i| {
                        let f = &forecasts[i];
                        let actual = samples[i].forward_returns[h];
                        Some(FitRow {
                            scale: f.error_scale[h],
                            z: residual(f, h, actual)?,
                            hit: price_error(f.returns[h], actual) <= HIT_BAND,
                            block: rank[&samples[i].date] / block_len,
                        })
                    })
                    .collect();
                rows.sort_by(|a, b| a.scale.total_cmp(&b.scale));
                let n = rows.len();
                let size = n.div_ceil(BUCKETS);
                let chunks: Vec<&[FitRow]> = rows.chunks(size).collect();
                let bucket_edges = chunks[..chunks.len() - 1]
                    .iter()
                    .map(|c| c[c.len() - 1].scale)
                    .collect();
                let residual_quantiles = chunks
                    .iter()
                    .map(|c| {
                        let z = sorted(c.iter().map(|r| r.z).collect());
                        (0..GRID)
                            .map(|i| quantile_sorted(&z, (i + 1) as f64 / (GRID + 1) as f64))
                            .collect()
                    })
                    .collect();
                let bucket_hit_rates = chunks
                    .iter()
                    .map(|c| c.iter().filter(|r| r.hit).count() as f64 / c.len() as f64)
                    .collect();
                let coverage_scales = CURVE_COVERAGES
                    .iter()
                    .map(|c| rows[((c * n as f64).ceil() as usize).clamp(1, n) - 1].scale)
                    .collect();

                let mut best: Option<(usize, f64, f64)> = None;
                for step in 1..=CUTOFF_STEPS {
                    let k = (n * step).div_ceil(CUTOFF_STEPS);
                    if k < MIN_SELECTED {
                        continue;
                    }
                    let (pooled, lb) = block_lower_bound(&rows[..k], blocks);
                    if lb >= TARGET_HIT_RATE {
                        best = Some((k, pooled, lb));
                    }
                }
                let (max_scale, fit_coverage, fit_hit_rate, fit_lower_bound) = match best {
                    Some((k, pooled, lb)) => (
                        Some(rows[k - 1].scale),
                        k as f64 / n as f64,
                        Some(pooled),
                        Some(lb),
                    ),
                    None => (None, 0.0, None, None),
                };
                HorizonConfidence {
                    horizon,
                    bucket_edges,
                    residual_quantiles,
                    bucket_hit_rates,
                    coverage_scales,
                    max_scale,
                    fit_coverage,
                    fit_hit_rate,
                    fit_lower_bound,
                }
            })
            .collect();
        Some(ConfidenceModel {
            fit_from: dates[0],
            fit_to: dates[dates.len() - 1],
            fit_rows: idx.len(),
            hit_band: HIT_BAND,
            target_hit_rate: TARGET_HIT_RATE,
            horizons,
        })
    }
}

fn sorted_dates(dates: impl Iterator<Item = NaiveDate>) -> Vec<NaiveDate> {
    let mut d: Vec<NaiveDate> = dates.collect();
    d.sort();
    d.dedup();
    d
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub fit_coverage: f64,
    pub max_scale: f64,
    pub coverage: f64,
    pub hit_rate: f64,
    pub naive_hit_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionSummary {
    pub max_scale: Option<f64>,
    pub rows: usize,
    pub selected: usize,
    pub coverage: f64,
    pub per_date: f64,
    pub hit_rate: f64,
    pub naive_hit_rate: f64,
    // Spread of the hit rate across dates that issued at least five forecasts.
    pub date_hit_p10: f64,
    pub date_hit_median: f64,
    pub dates_selected: usize,
    pub dates_total: usize,
    pub symbols: usize,
    pub top5_share: f64,
    // Halted or near-untraded rows that would have passed the cutoff, excluded above.
    pub untradeable_passing: usize,
    pub untradeable_naive_hit_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalSummary {
    pub level: f64,
    pub coverage: f64,
    pub mean_width: f64,
    pub date_coverage_p10: f64,
    pub date_coverage_median: f64,
    pub date_coverage_p90: f64,
    // Share of dates whose coverage falls more than ten points short of `level`.
    pub dates_short: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalSummary {
    // Probability, under the fitted residuals, that the net return is positive.
    pub confidence: f64,
    pub count: usize,
    pub per_date: f64,
    pub dates_with_signal: usize,
    pub win_rate: f64,
    pub mean_net: f64,
    pub median_net: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortfolioSummary {
    pub dates: usize,
    pub mean_names: f64,
    // Top-decile return after the round-trip cost, the liquid universe's mean return,
    // and the difference, per holding period of `horizon` sessions.
    pub mean_top_net: f64,
    pub mean_universe: f64,
    pub mean_excess: f64,
    pub excess_win_rate: f64,
    pub top_net_win_rate: f64,
    // Overlap-adjusted: standard error from `dates / horizon` independent periods.
    pub t_stat: f64,
    pub annualized_excess: f64,
    pub long_short_gross: f64,
    pub long_short_win_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonConfidenceReport {
    pub horizon: usize,
    pub curve: Vec<CurvePoint>,
    pub selection: SelectionSummary,
    pub intervals: Vec<IntervalSummary>,
    pub signals: Vec<SignalSummary>,
    pub portfolio: PortfolioSummary,
    #[serde(default)]
    pub avoid: Option<AvoidSummary>,
    // Same rule with the most volatile tenth excluded instead of the model's lowest.
    #[serde(default)]
    pub avoid_control: Option<AvoidSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceReport {
    pub model_name: String,
    pub split_name: String,
    pub cost: f64,
    pub horizons: Vec<HorizonConfidenceReport>,
}

fn by_date(idx: &[usize], samples: &[Sample]) -> BTreeMap<NaiveDate, Vec<usize>> {
    let mut map: BTreeMap<NaiveDate, Vec<usize>> = BTreeMap::new();
    for &i in idx {
        map.entry(samples[i].date).or_default().push(i);
    }
    map
}

fn hit(f: &Forecast, h: usize, actual: f64) -> bool {
    price_error(f.returns[h], actual) <= HIT_BAND
}

fn naive_hit(actual: f64) -> bool {
    price_error(0.0, actual) <= HIT_BAND
}

fn share(flags: impl Iterator<Item = bool>) -> f64 {
    let (mut n, mut k) = (0usize, 0usize);
    for f in flags {
        n += 1;
        k += usize::from(f);
    }
    if n == 0 {
        f64::NAN
    } else {
        k as f64 / n as f64
    }
}

fn selection_summary(
    hc: &HorizonConfidence,
    h: usize,
    calibrated: &[Forecast],
    samples: &[Sample],
    tradeable_idx: &[usize],
    dates_total: usize,
) -> SelectionSummary {
    let chosen: Vec<usize> = tradeable_idx
        .iter()
        .copied()
        .filter(|&i| hc.selects(calibrated[i].error_scale[h]))
        .collect();
    let actual = |i: usize| samples[i].forward_returns[h];
    let mut date_rates = Vec::new();
    let groups = by_date(&chosen, samples);
    for rows in groups.values() {
        if rows.len() >= 5 {
            date_rates.push(share(
                rows.iter().map(|&i| hit(&calibrated[i], h, actual(i))),
            ));
        }
    }
    let date_rates = sorted(date_rates);
    let mut per_symbol: HashMap<&str, usize> = HashMap::new();
    for &i in &chosen {
        *per_symbol.entry(samples[i].symbol.as_str()).or_default() += 1;
    }
    let mut counts: Vec<usize> = per_symbol.values().copied().collect();
    counts.sort_unstable_by(|a, b| b.cmp(a));
    let untradeable: Vec<usize> = (0..samples.len())
        .filter(|&i| !tradeable(&samples[i]) && hc.selects(calibrated[i].error_scale[h]))
        .collect();
    SelectionSummary {
        max_scale: hc.max_scale,
        rows: tradeable_idx.len(),
        selected: chosen.len(),
        coverage: chosen.len() as f64 / tradeable_idx.len().max(1) as f64,
        per_date: chosen.len() as f64 / dates_total.max(1) as f64,
        hit_rate: share(chosen.iter().map(|&i| hit(&calibrated[i], h, actual(i)))),
        naive_hit_rate: share(chosen.iter().map(|&i| naive_hit(actual(i)))),
        date_hit_p10: quantile_sorted(&date_rates, 0.1),
        date_hit_median: quantile_sorted(&date_rates, 0.5),
        dates_selected: groups.len(),
        dates_total,
        symbols: per_symbol.len(),
        top5_share: counts.iter().take(5).sum::<usize>() as f64 / chosen.len().max(1) as f64,
        untradeable_passing: untradeable.len(),
        untradeable_naive_hit_rate: share(untradeable.iter().map(|&i| naive_hit(actual(i)))),
    }
}

fn interval_summary(
    hc: &HorizonConfidence,
    h: usize,
    level: f64,
    calibrated: &[Forecast],
    samples: &[Sample],
    groups: &BTreeMap<NaiveDate, Vec<usize>>,
) -> IntervalSummary {
    let mut inside_all = 0usize;
    let mut total = 0usize;
    let mut width = 0.0;
    let mut date_cov = Vec::new();
    for rows in groups.values() {
        let mut inside = 0usize;
        for &i in rows {
            let f = &calibrated[i];
            let (lo, hi) = hc.interval(f.returns[h], f.error_scale[h], level);
            let a = samples[i].forward_returns[h];
            inside += usize::from(a >= lo && a <= hi);
            width += hi - lo;
        }
        inside_all += inside;
        total += rows.len();
        date_cov.push(inside as f64 / rows.len() as f64);
    }
    let date_cov = sorted(date_cov);
    IntervalSummary {
        level,
        coverage: inside_all as f64 / total.max(1) as f64,
        mean_width: width / total.max(1) as f64,
        date_coverage_p10: quantile_sorted(&date_cov, 0.1),
        date_coverage_median: quantile_sorted(&date_cov, 0.5),
        date_coverage_p90: quantile_sorted(&date_cov, 0.9),
        dates_short: share(date_cov.iter().map(|c| *c < level - 0.10)),
    }
}

#[allow(clippy::too_many_arguments)]
fn signal_summary(
    hc: &HorizonConfidence,
    h: usize,
    confidence: f64,
    cost: f64,
    calibrated: &[Forecast],
    samples: &[Sample],
    liquid_idx: &[usize],
    dates_total: usize,
) -> SignalSummary {
    let chosen: Vec<usize> = liquid_idx
        .iter()
        .copied()
        .filter(|&i| {
            let f = &calibrated[i];
            hc.return_quantile(f.returns[h], f.error_scale[h], 1.0 - confidence) > cost
        })
        .collect();
    let net: Vec<f64> = chosen
        .iter()
        .map(|&i| samples[i].forward_returns[h] - cost)
        .collect();
    let dates = by_date(&chosen, samples).len();
    SignalSummary {
        confidence,
        count: chosen.len(),
        per_date: chosen.len() as f64 / dates_total.max(1) as f64,
        dates_with_signal: dates,
        win_rate: share(net.iter().map(|v| *v > 0.0)),
        mean_net: mean(&net),
        median_net: quantile_sorted(&sorted(net), 0.5),
    }
}

fn portfolio_summary(
    h: usize,
    horizon: usize,
    cost: f64,
    raw: &[Forecast],
    samples: &[Sample],
    groups: &BTreeMap<NaiveDate, Vec<usize>>,
) -> PortfolioSummary {
    let (mut top, mut uni, mut excess, mut ls, mut names) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for rows in groups.values() {
        if rows.len() < 50 {
            continue;
        }
        let mut ranked: Vec<(f64, f64)> = rows
            .iter()
            .map(|&i| (raw[i].returns[h], samples[i].forward_returns[h]))
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        let k = ranked.len() / 10;
        let avg = |s: &[(f64, f64)]| s.iter().map(|r| r.1).sum::<f64>() / s.len() as f64;
        let t = avg(&ranked[..k]) - cost;
        let u = avg(&ranked);
        top.push(t);
        uni.push(u);
        excess.push(t - u);
        ls.push(avg(&ranked[..k]) - avg(&ranked[ranked.len() - k..]));
        names.push(k as f64);
    }
    let periods = (excess.len() as f64 / horizon as f64).max(1.0);
    PortfolioSummary {
        dates: excess.len(),
        mean_names: mean(&names),
        mean_top_net: mean(&top),
        mean_universe: mean(&uni),
        mean_excess: mean(&excess),
        excess_win_rate: share(excess.iter().map(|v| *v > 0.0)),
        top_net_win_rate: share(top.iter().map(|v| *v > 0.0)),
        t_stat: mean(&excess) / (std_dev(&excess) / periods.sqrt()),
        annualized_excess: mean(&excess) * TRADING_DAYS / horizon as f64,
        long_short_gross: mean(&ls),
        long_short_win_rate: share(ls.iter().map(|v| *v > 0.0)),
    }
}

/// Scores `model` on `samples`: the coverage curve, the chosen selective cutoff,
/// interval coverage, net-of-cost signals and the top-decile portfolio.
pub fn evaluate(
    model: &ConfidenceModel,
    raw: &[Forecast],
    calibrated: &[Forecast],
    samples: &[Sample],
    split_name: &str,
    cost: f64,
) -> ConfidenceReport {
    let tradeable_idx: Vec<usize> = (0..samples.len())
        .filter(|&i| tradeable(&samples[i]))
        .collect();
    let liquid_idx: Vec<usize> = (0..samples.len())
        .filter(|&i| liquid(&samples[i]))
        .collect();
    let tradeable_groups = by_date(&tradeable_idx, samples);
    let liquid_groups = by_date(&liquid_idx, samples);
    let dates_total = tradeable_groups.len();
    let horizons = model
        .horizons
        .iter()
        .enumerate()
        .take(NUM_HORIZONS)
        .map(|(h, hc)| {
            let curve = CURVE_COVERAGES
                .iter()
                .zip(&hc.coverage_scales)
                .map(|(&fit_coverage, &max_scale)| {
                    let chosen: Vec<usize> = tradeable_idx
                        .iter()
                        .copied()
                        .filter(|&i| calibrated[i].error_scale[h] <= max_scale)
                        .collect();
                    CurvePoint {
                        fit_coverage,
                        max_scale,
                        coverage: chosen.len() as f64 / tradeable_idx.len().max(1) as f64,
                        hit_rate: share(
                            chosen
                                .iter()
                                .map(|&i| hit(&calibrated[i], h, samples[i].forward_returns[h])),
                        ),
                        naive_hit_rate: share(
                            chosen
                                .iter()
                                .map(|&i| naive_hit(samples[i].forward_returns[h])),
                        ),
                    }
                })
                .collect();
            HorizonConfidenceReport {
                horizon: hc.horizon,
                curve,
                selection: selection_summary(
                    hc,
                    h,
                    calibrated,
                    samples,
                    &tradeable_idx,
                    dates_total,
                ),
                intervals: INTERVAL_LEVELS
                    .iter()
                    .map(|&l| interval_summary(hc, h, l, calibrated, samples, &tradeable_groups))
                    .collect(),
                signals: SIGNAL_CONFIDENCES
                    .iter()
                    .map(|&c| {
                        signal_summary(
                            hc,
                            h,
                            c,
                            cost,
                            calibrated,
                            samples,
                            &liquid_idx,
                            dates_total,
                        )
                    })
                    .collect(),
                portfolio: portfolio_summary(h, hc.horizon, cost, raw, samples, &liquid_groups),
                avoid: Some(avoid_bottom(raw, samples, h, hc.horizon, cost)),
                avoid_control: Some(avoid_volatile(samples, h, hc.horizon, cost)),
            }
        })
        .collect();
    ConfidenceReport {
        model_name: "Selective".to_owned(),
        split_name: split_name.to_owned(),
        cost,
        horizons,
    }
}

pub fn print_report(report: &ConfidenceReport) {
    let pct = |v: f64| format!("{:.1}%", v * 100.0);
    let ret = |v: f64| format!("{:+.2}%", v * 100.0);
    println!(
        "\n=== {}: selective forecasts, intervals and net-of-cost checks (cost {:.2}%) ===",
        report.split_name,
        report.cost * 100.0
    );
    for r in &report.horizons {
        let s = &r.selection;
        println!("t+{}", r.horizon);
        println!("  coverage curve (cutoff from fit split -> hit<=1% model / naive):");
        for p in &r.curve {
            println!(
                "    fit {:>6} -> cov {:>6}  hit {:>6} / {:>6}",
                pct(p.fit_coverage),
                pct(p.coverage),
                pct(p.hit_rate),
                pct(p.naive_hit_rate)
            );
        }
        match s.max_scale {
            Some(m) => println!(
                "  cutoff scale<={m:.4}: {}/{} rows ({}), {:.1}/day on {}/{} dates, hit {} (naive {}), date hit p10 {} median {}, {} symbols, top5 {}, excluded untradeable {} (naive hit {})",
                s.selected,
                s.rows,
                pct(s.coverage),
                s.per_date,
                s.dates_selected,
                s.dates_total,
                pct(s.hit_rate),
                pct(s.naive_hit_rate),
                pct(s.date_hit_p10),
                pct(s.date_hit_median),
                s.symbols,
                pct(s.top5_share),
                s.untradeable_passing,
                pct(s.untradeable_naive_hit_rate)
            ),
            None => println!("  cutoff: none reached the target on the fit split"),
        }
        for i in &r.intervals {
            println!(
                "  interval {:>4}: coverage {} width {} | per-date p10 {} median {} p90 {} | dates >10pt short {}",
                pct(i.level),
                pct(i.coverage),
                pct(i.mean_width),
                pct(i.date_coverage_p10),
                pct(i.date_coverage_median),
                pct(i.date_coverage_p90),
                pct(i.dates_short)
            );
        }
        for g in &r.signals {
            println!(
                "  signal P(net>0)>={}: {} rows ({:.2}/day, {} dates) win {} mean {} median {}",
                pct(g.confidence),
                g.count,
                g.per_date,
                g.dates_with_signal,
                pct(g.win_rate),
                ret(g.mean_net),
                ret(g.median_net)
            );
        }
        let p = &r.portfolio;
        println!(
            "  top decile ({:.0} names, {} dates): net {} vs universe {} -> excess {} (win {}, t {:+.2}, annualized {}), net>0 on {} | long-short gross {} (win {})",
            p.mean_names,
            p.dates,
            ret(p.mean_top_net),
            ret(p.mean_universe),
            ret(p.mean_excess),
            pct(p.excess_win_rate),
            p.t_stat,
            ret(p.annualized_excess),
            pct(p.top_net_win_rate),
            ret(p.long_short_gross),
            pct(p.long_short_win_rate)
        );
        for (label, a) in [("model", &r.avoid), ("control", &r.avoid_control)] {
            let Some(a) = a else { continue };
            println!(
                "  avoid {label} ({:.0} of {:.0} names, {} periods, ~{:.0} independent): excess {} per period (win {}, t {:+.2}, annualized {}) | excluded vs universe {} (under on {}) | extra turnover {}",
                a.mean_excluded,
                a.mean_names,
                a.periods,
                a.independent_periods,
                ret(a.mean_excess),
                pct(a.excess_win_rate),
                a.t_stat,
                ret(a.annualized_excess),
                ret(a.excluded_minus_universe),
                pct(a.excluded_under_rate),
                pct(a.mean_extra_turnover)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(state: &mut u64) -> f64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (*state >> 11) as f64 / (1u64 << 53) as f64
    }

    // Laplace residuals with a per-row scale, so quantiles in scale units are known.
    fn data(n: usize) -> (Vec<Forecast>, Vec<Sample>) {
        let mut state = 7u64;
        let start = NaiveDate::from_ymd_opt(2024, 7, 1).expect("valid date");
        let mut forecasts = Vec::new();
        let mut samples = Vec::new();
        for i in 0..n {
            let scale = 0.002 + 0.03 * draw(&mut state);
            let u = draw(&mut state) - 0.5;
            let z = -u.signum() * (1.0 - 2.0 * u.abs()).max(1e-12).ln();
            let actual = (z * scale).exp() - 1.0;
            forecasts.push(Forecast {
                returns: [0.0; NUM_HORIZONS],
                error_scale: [scale; NUM_HORIZONS],
            });
            samples.push(Sample {
                symbol: format!("{:06}", i % 300),
                date: start + chrono::Days::new((i / 300) as u64),
                features: Vec::new(),
                forward_returns: vec![actual; NUM_HORIZONS],
                target_end: start,
                trading_value: 5e9,
            });
        }
        (forecasts, samples)
    }

    #[test]
    fn intervals_cover_their_level_and_cutoff_meets_target() {
        let (forecasts, samples) = data(60_000);
        let model = ConfidenceModel::fit(&forecasts, &samples).expect("enough rows");
        let report = evaluate(&model, &forecasts, &forecasts, &samples, "fit", 0.0);
        for r in &report.horizons {
            let i80 = r
                .intervals
                .iter()
                .find(|i| i.level == 0.8)
                .expect("80% level");
            assert!(
                (i80.coverage - 0.8).abs() < 0.01,
                "coverage {}",
                i80.coverage
            );
            assert!(r.selection.hit_rate >= TARGET_HIT_RATE - 0.01);
        }
        // Laplace z: P(|z| <= 1%/scale) = 1 - exp(-0.01/scale); the cutoff must sit near
        // the scale where the running hit rate drops to 80%, far below the widest scale.
        let hc = &model.horizons[0];
        assert!(hc.max_scale.expect("cutoff") < 0.02);
    }

    #[test]
    fn untradeable_rows_are_excluded_from_fit() {
        let (forecasts, mut samples) = data(10_000);
        for s in &mut samples {
            s.trading_value = 0.0;
        }
        assert!(ConfidenceModel::fit(&forecasts, &samples).is_none());
    }
}
