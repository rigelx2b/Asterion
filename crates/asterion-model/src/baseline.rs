//! Statistical and financial baseline models.
//!
//! Mandatory standard for comparing deep learning models against naive
//! and statistical baselines (momentum, mean reversion, ridge regression).

use crate::features::{NUM_FEATURES, Sample};
use crate::metrics::{EvaluationReport, HORIZONS, compute_metrics};

pub trait BaselineModel {
    fn name(&self) -> &str;
    fn predict(&self, sample: &Sample) -> Vec<f64>;

    fn evaluate(&self, samples: &[Sample], split_name: &str) -> EvaluationReport {
        let mut actual_per_h = vec![Vec::with_capacity(samples.len()); HORIZONS.len()];
        let mut pred_per_h = vec![Vec::with_capacity(samples.len()); HORIZONS.len()];

        for s in samples {
            let preds = self.predict(s);
            for (h_idx, _) in HORIZONS.iter().enumerate() {
                actual_per_h[h_idx].push(s.forward_returns[h_idx]);
                pred_per_h[h_idx].push(preds[h_idx]);
            }
        }

        let mut horizons = Vec::new();
        for (h_idx, &h) in HORIZONS.iter().enumerate() {
            horizons.push(compute_metrics(h, &actual_per_h[h_idx], &pred_per_h[h_idx]));
        }

        EvaluationReport {
            model_name: self.name().to_owned(),
            split_name: split_name.to_owned(),
            horizons,
        }
    }
}

/// Baseline 0: price unchanged at every horizon (random walk). Tiered skill is measured
/// against this forecast.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChangeBaseline;

impl BaselineModel for NoChangeBaseline {
    fn name(&self) -> &str {
        "No-Change Baseline"
    }

    fn predict(&self, _sample: &Sample) -> Vec<f64> {
        vec![0.0; HORIZONS.len()]
    }
}

/// Baseline 1: Momentum - predicts the sign of trailing 20-day return.
#[derive(Debug, Clone, Copy, Default)]
pub struct MomentumBaseline;

impl BaselineModel for MomentumBaseline {
    fn name(&self) -> &str {
        "Momentum Baseline"
    }

    fn predict(&self, sample: &Sample) -> Vec<f64> {
        // Feature index 5 is log_ret_20
        let trailing_ret = sample.features.get(5).copied().unwrap_or(0.0);
        let pred = if trailing_ret > 0.0 { 0.01 } else { -0.01 };
        vec![pred; HORIZONS.len()]
    }
}

/// Baseline 2: Mean Reversion - predicts opposite of trailing 5-day return.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeanReversionBaseline;

impl BaselineModel for MeanReversionBaseline {
    fn name(&self) -> &str {
        "Mean Reversion Baseline"
    }

    fn predict(&self, sample: &Sample) -> Vec<f64> {
        // Feature index 3 is log_ret_5
        let trailing_ret = sample.features.get(3).copied().unwrap_or(0.0);
        let pred = if trailing_ret > 0.0 { -0.01 } else { 0.01 };
        vec![pred; HORIZONS.len()]
    }
}

/// Baseline 3: Multi-Horizon Ridge Regression.
#[derive(Debug, Clone)]
pub struct RidgeBaseline {
    /// Weights for each horizon: `weights[h_idx]` has length `NUM_FEATURES + 1` (with bias).
    pub weights: Vec<Vec<f64>>,
}

impl RidgeBaseline {
    #[allow(clippy::needless_range_loop)]
    pub fn fit(train_samples: &[Sample], alpha: f64) -> Self {
        let n = train_samples.len();
        let d = NUM_FEATURES + 1; // plus bias
        let mut weights_per_h = Vec::with_capacity(HORIZONS.len());

        if n == 0 {
            return RidgeBaseline {
                weights: vec![vec![0.0; d]; HORIZONS.len()],
            };
        }

        // Build X^T X (d x d)
        let mut xtx = vec![vec![0.0; d]; d];
        for s in train_samples {
            let mut x_row = vec![1.0]; // bias
            x_row.extend_from_slice(&s.features[..NUM_FEATURES]);

            for i in 0..d {
                for j in 0..d {
                    xtx[i][j] += x_row[i] * x_row[j];
                }
            }
        }

        // Add L2 penalty (alpha * I) except to bias (index 0)
        for i in 1..d {
            xtx[i][i] += alpha;
        }

        // Solve for each horizon
        for h_idx in 0..HORIZONS.len() {
            let mut xty = vec![0.0; d];
            for s in train_samples {
                let mut x_row = vec![1.0];
                x_row.extend_from_slice(&s.features[..NUM_FEATURES]);
                let y = s.forward_returns[h_idx];
                for i in 0..d {
                    xty[i] += x_row[i] * y;
                }
            }

            let w = solve_linear_system(&xtx, &xty);
            weights_per_h.push(w);
        }

        RidgeBaseline {
            weights: weights_per_h,
        }
    }
}

impl BaselineModel for RidgeBaseline {
    fn name(&self) -> &str {
        "Ridge Regression Baseline"
    }

    fn predict(&self, sample: &Sample) -> Vec<f64> {
        let d = NUM_FEATURES + 1;
        let mut x_row = vec![1.0];
        x_row.extend_from_slice(&sample.features[..NUM_FEATURES]);

        let mut preds = Vec::with_capacity(HORIZONS.len());
        for h_idx in 0..HORIZONS.len() {
            let w = &self.weights[h_idx];
            let mut dot = 0.0;
            for i in 0..d {
                dot += w[i] * x_row[i];
            }
            preds.push(dot);
        }
        preds
    }
}

/// Gaussian elimination with partial pivoting.
#[allow(clippy::needless_range_loop)]
fn solve_linear_system(a: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = b.len();
    let mut mat: Vec<Vec<f64>> = a.to_vec();
    let mut rhs = b.to_vec();

    for i in 0..n {
        let mut max_row = i;
        let mut max_val = mat[i][i].abs();
        for k in (i + 1)..n {
            if mat[k][i].abs() > max_val {
                max_val = mat[k][i].abs();
                max_row = k;
            }
        }
        mat.swap(i, max_row);
        rhs.swap(i, max_row);

        let pivot = mat[i][i];
        if pivot.abs() < 1e-12 {
            continue;
        }

        for k in (i + 1)..n {
            let factor = mat[k][i] / pivot;
            for j in i..n {
                mat[k][j] -= factor * mat[i][j];
            }
            rhs[k] -= factor * rhs[i];
        }
    }

    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut sum = rhs[i];
        for j in (i + 1)..n {
            sum -= mat[i][j] * x[j];
        }
        if mat[i][i].abs() > 1e-12 {
            x[i] = sum / mat[i][i];
        }
    }
    x
}
