//! Post-hoc calibration of raw network forecasts for the tiered price credit.
//!
//! A forecast splits into the mean over all stocks on its date (the market part) and the
//! stock's deviation from that mean (the relative part). The tiered credit punishes any
//! magnitude the model cannot back up, so each part gets its own shrinkage weight, and
//! the error scale is refitted to the calibrated forecast.

use std::collections::HashMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::features::Sample;
use crate::metrics::{HORIZONS, price_error, tier_score};
use crate::neural::Forecast;

const NUM_HORIZONS: usize = HORIZONS.len();
// Relative weights tried when fitting, 0.00..=1.50.
const RELATIVE_GRID: usize = 31;
const RELATIVE_STEP: f64 = 0.05;
pub const MARKET_GRID: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HorizonCalibration {
    pub market_weight: f64,
    pub relative_weight: f64,
    // Multiplier on the network's error scale so it matches calibrated forecasts.
    pub error_multiplier: f64,
}

impl HorizonCalibration {
    pub const IDENTITY: HorizonCalibration = HorizonCalibration {
        market_weight: 1.0,
        relative_weight: 1.0,
        error_multiplier: 1.0,
    };

    pub fn adjust(&self, raw: f64, date_mean: f64) -> f64 {
        self.market_weight * date_mean + self.relative_weight * (raw - date_mean)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub horizons: Vec<HorizonCalibration>,
}

impl Default for Calibration {
    fn default() -> Self {
        Calibration {
            horizons: vec![HorizonCalibration::IDENTITY; NUM_HORIZONS],
        }
    }
}

/// Per row, the mean raw forecast of all rows sharing its date.
pub fn date_means(raw: &[Forecast], dates: &[NaiveDate]) -> Vec<[f64; NUM_HORIZONS]> {
    let mut sums: HashMap<NaiveDate, ([f64; NUM_HORIZONS], usize)> = HashMap::new();
    for (f, d) in raw.iter().zip(dates) {
        let entry = sums.entry(*d).or_insert(([0.0; NUM_HORIZONS], 0));
        for h in 0..NUM_HORIZONS {
            entry.0[h] += f.returns[h];
        }
        entry.1 += 1;
    }
    dates
        .iter()
        .map(|d| {
            let (sum, n) = sums[d];
            sum.map(|v| v / n as f64)
        })
        .collect()
}

impl Calibration {
    pub fn apply_one(&self, raw: &Forecast, means: &[f64; NUM_HORIZONS]) -> Forecast {
        let mut out = *raw;
        for (h, c) in self.horizons.iter().enumerate().take(NUM_HORIZONS) {
            out.returns[h] = c.adjust(raw.returns[h], means[h]);
            out.error_scale[h] = raw.error_scale[h] * c.error_multiplier;
        }
        out
    }

    pub fn apply(&self, raw: &[Forecast], samples: &[Sample]) -> Vec<Forecast> {
        let dates: Vec<NaiveDate> = samples.iter().map(|s| s.date).collect();
        let means = date_means(raw, &dates);
        raw.iter()
            .zip(&means)
            .map(|(f, m)| self.apply_one(f, m))
            .collect()
    }
}

fn mean_score(
    raw: &[Forecast],
    means: &[[f64; NUM_HORIZONS]],
    samples: &[Sample],
    h: usize,
    c: HorizonCalibration,
) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    raw.iter()
        .zip(means)
        .zip(samples)
        .map(|((f, m), s)| {
            tier_score(price_error(
                c.adjust(f.returns[h], m[h]),
                s.forward_returns[h],
            ))
        })
        .sum::<f64>()
        / samples.len() as f64
}

/// Fits the relative weight on `samples` with the market weight held at `market_weight`,
/// then the error multiplier (Laplace maximum likelihood for a scaled error).
pub fn fit(raw: &[Forecast], samples: &[Sample], market_weight: f64) -> Calibration {
    let dates: Vec<NaiveDate> = samples.iter().map(|s| s.date).collect();
    let means = date_means(raw, &dates);
    let horizons = (0..NUM_HORIZONS)
        .map(|h| {
            let mut best = HorizonCalibration {
                market_weight,
                relative_weight: 0.0,
                error_multiplier: 1.0,
            };
            let mut best_score = f64::NEG_INFINITY;
            for k in 0..RELATIVE_GRID {
                let candidate = HorizonCalibration {
                    relative_weight: k as f64 * RELATIVE_STEP,
                    ..best
                };
                let score = mean_score(raw, &means, samples, h, candidate);
                if score > best_score {
                    best_score = score;
                    best = candidate;
                }
            }
            let ratios: Vec<f64> = raw
                .iter()
                .zip(&means)
                .zip(samples)
                .map(|((f, m), s)| {
                    price_error(best.adjust(f.returns[h], m[h]), s.forward_returns[h])
                        / f.error_scale[h].max(1e-6)
                })
                .filter(|r| r.is_finite())
                .collect();
            if !ratios.is_empty() {
                best.error_multiplier = ratios.iter().sum::<f64>() / ratios.len() as f64;
            }
            best
        })
        .collect();
    Calibration { horizons }
}

/// Market weight per horizon that maximises the credit on `samples`, keeping the
/// relative weights of `base`.
pub fn fit_market_weights(raw: &[Forecast], samples: &[Sample], base: &Calibration) -> Vec<f64> {
    let dates: Vec<NaiveDate> = samples.iter().map(|s| s.date).collect();
    let means = date_means(raw, &dates);
    (0..NUM_HORIZONS)
        .map(|h| {
            MARKET_GRID
                .into_iter()
                .map(|w| {
                    let c = HorizonCalibration {
                        market_weight: w,
                        ..base.horizons[h]
                    };
                    (w, mean_score(raw, &means, samples, h, c))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map_or(0.0, |(w, _)| w)
        })
        .collect()
}

/// Tiered credit of the raw forecast, the forecast with its date mean removed, and the
/// calibrated forecast, next to the no-change credit, for one horizon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VariantScores {
    pub horizon: usize,
    pub naive: f64,
    pub raw: f64,
    pub demeaned: f64,
    pub calibrated: f64,
    // Mean over dates of the within-date correlation of forecast and outcome.
    pub cross_sectional_ic: f64,
    // Correlation across dates of the date-mean forecast and the date-mean outcome.
    pub date_mean_ic: f64,
    pub dates: usize,
}

fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len() as f64;
    if xs.len() < 3 {
        return None;
    }
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let (mut c, mut vx, mut vy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        c += (x - mx) * (y - my);
        vx += (x - mx).powi(2);
        vy += (y - my).powi(2);
    }
    (vx > 1e-18 && vy > 1e-18).then(|| c / (vx.sqrt() * vy.sqrt()))
}

pub fn variant_scores(
    raw: &[Forecast],
    samples: &[Sample],
    calibration: &Calibration,
) -> Vec<VariantScores> {
    let dates: Vec<NaiveDate> = samples.iter().map(|s| s.date).collect();
    let means = date_means(raw, &dates);
    let mut by_date: HashMap<NaiveDate, Vec<usize>> = HashMap::new();
    for (i, d) in dates.iter().enumerate() {
        by_date.entry(*d).or_default().push(i);
    }
    (0..NUM_HORIZONS)
        .map(|h| {
            let demeaned = HorizonCalibration {
                market_weight: 0.0,
                relative_weight: 1.0,
                error_multiplier: 1.0,
            };
            let mut ics = Vec::new();
            let (mut mean_pred, mut mean_actual) = (Vec::new(), Vec::new());
            for rows in by_date.values() {
                let p: Vec<f64> = rows.iter().map(|&i| raw[i].returns[h]).collect();
                let a: Vec<f64> = rows
                    .iter()
                    .map(|&i| samples[i].forward_returns[h])
                    .collect();
                if let Some(ic) = pearson(&p, &a) {
                    ics.push(ic);
                }
                mean_pred.push(p.iter().sum::<f64>() / p.len() as f64);
                mean_actual.push(a.iter().sum::<f64>() / a.len() as f64);
            }
            VariantScores {
                horizon: HORIZONS[h],
                naive: mean_score(
                    raw,
                    &means,
                    samples,
                    h,
                    HorizonCalibration {
                        market_weight: 0.0,
                        relative_weight: 0.0,
                        error_multiplier: 1.0,
                    },
                ),
                raw: mean_score(raw, &means, samples, h, HorizonCalibration::IDENTITY),
                demeaned: mean_score(raw, &means, samples, h, demeaned),
                calibrated: mean_score(raw, &means, samples, h, calibration.horizons[h]),
                cross_sectional_ic: if ics.is_empty() {
                    0.0
                } else {
                    ics.iter().sum::<f64>() / ics.len() as f64
                },
                date_mean_ic: pearson(&mean_pred, &mean_actual).unwrap_or(0.0),
                dates: by_date.len(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(day: u32, actual: f64) -> Sample {
        let date = NaiveDate::from_ymd_opt(2024, 7, day).expect("date");
        Sample {
            symbol: format!("{day}"),
            date,
            features: Vec::new(),
            forward_returns: vec![actual; NUM_HORIZONS],
            target_end: date,
            trading_value: 0.0,
        }
    }

    fn forecast(r: f64) -> Forecast {
        Forecast {
            returns: [r; NUM_HORIZONS],
            error_scale: [0.02; NUM_HORIZONS],
        }
    }

    #[test]
    fn zero_weights_reproduce_no_change_and_fit_never_loses_to_it_in_sample() {
        // Forecasts are twice the realised deviation and carry a large common bias.
        let samples: Vec<Sample> = (0..40)
            .map(|i| sample(1 + i % 2, if i % 4 < 2 { 0.04 } else { -0.04 }))
            .collect();
        let raw: Vec<Forecast> = samples
            .iter()
            .map(|s| forecast(0.10 + 2.0 * s.forward_returns[0]))
            .collect();
        let calibration = fit(&raw, &samples, 0.0);
        let v = variant_scores(&raw, &samples, &calibration);
        for row in &v {
            assert!(row.calibrated >= row.naive - 1e-12);
            assert!(row.demeaned > row.raw, "removing the common bias helps");
        }
        // Weights within about 0.13 of 0.5 land every forecast inside the 1% band.
        let c = calibration.horizons[0];
        assert!((c.relative_weight - 0.5).abs() < 0.15, "{c:?}");
        assert!((v[0].calibrated - 1.0).abs() < 1e-12);
        let adjusted = calibration.apply(&raw, &samples);
        assert!((adjusted[0].returns[0] - samples[0].forward_returns[0]).abs() < 0.011);
    }
}
