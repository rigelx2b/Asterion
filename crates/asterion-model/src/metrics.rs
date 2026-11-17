//! Multi-horizon quantitative evaluation metrics.
//!
//! Evaluates directional accuracy, classification metrics (precision/recall/F1/balanced accuracy),
//! and magnitude errors (MAE, RMSE, IC) for each forecast horizon independently.

use serde::{Deserialize, Serialize};

/// Standard prediction horizons in trading days.
pub const HORIZONS: [usize; 5] = [1, 3, 5, 10, 20];

// Credit for a forecast whose price misses the realised price by at most the given
// fraction; larger misses earn nothing. Ordered from the tightest band outwards.
pub const PRICE_TIERS: [(f64, f64); 5] = [
    (0.01, 1.0),
    (0.02, 0.7),
    (0.03, 0.4),
    (0.04, 0.2),
    (0.05, 0.1),
];

// Relative distance between the forecast price and the realised price, both
// expressed as returns on the same starting close.
#[inline]
pub fn price_error(predicted_return: f64, actual_return: f64) -> f64 {
    let actual = 1.0 + actual_return;
    if actual <= 0.0 {
        return f64::INFINITY;
    }
    ((1.0 + predicted_return) / actual - 1.0).abs()
}

#[inline]
pub fn tier_score(error: f64) -> f64 {
    PRICE_TIERS
        .iter()
        .find(|(band, _)| error <= *band)
        .map_or(0.0, |(_, credit)| *credit)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonMetrics {
    pub horizon: usize,
    pub count: usize,
    pub directional_accuracy: f64,
    pub precision_up: f64,
    pub recall_up: f64,
    pub f1_up: f64,
    pub balanced_accuracy: f64,
    pub mae: f64,
    pub rmse: f64,
    pub ic: f64,
    // Mean tiered price credit of the forecast, and of "price unchanged" on the same rows.
    #[serde(default)]
    pub tier_score: f64,
    #[serde(default)]
    pub naive_tier_score: f64,
    // Share of rows whose price error is within each band of `PRICE_TIERS`.
    #[serde(default)]
    pub tier_hits: Vec<f64>,
}

impl HorizonMetrics {
    // Tiered credit gained over the no-change forecast; positive means real skill.
    pub fn tier_skill(&self) -> f64 {
        self.tier_score - self.naive_tier_score
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub model_name: String,
    pub split_name: String,
    pub horizons: Vec<HorizonMetrics>,
}

impl EvaluationReport {
    pub fn print_table(&self) {
        println!(
            "\n=== {} Evaluation Report ({}) ===",
            self.model_name, self.split_name
        );
        println!(
            "{:<8} {:<8} {:<9} {:<9} {:<9} {:<9} {:<9} {:<9} {:<9} {:<9} {:<8}",
            "Horizon",
            "Count",
            "Score",
            "Naive",
            "Skill",
            "<=1%",
            "<=2%",
            "<=5%",
            "Dir Acc",
            "Bal Acc",
            "IC"
        );
        println!("{}", "-".repeat(104));
        for h in &self.horizons {
            let hit = |i: usize| h.tier_hits.get(i).copied().unwrap_or(0.0) * 100.0;
            println!(
                "{:<8} {:<8} {:<9.4} {:<9.4} {:<+9.4} {:<8.2}% {:<8.2}% {:<8.2}% {:<8.2}% {:<8.2}% {:<+8.4}",
                format!("t+{}", h.horizon),
                h.count,
                h.tier_score,
                h.naive_tier_score,
                h.tier_skill(),
                hit(0),
                hit(1),
                hit(PRICE_TIERS.len() - 1),
                h.directional_accuracy * 100.0,
                h.balanced_accuracy * 100.0,
                h.ic
            );
        }
    }
}

/// Compute evaluation metrics for a single horizon given actual returns and predicted returns.
pub fn compute_metrics(
    horizon: usize,
    actual_returns: &[f64],
    predicted_returns: &[f64],
) -> HorizonMetrics {
    assert_eq!(
        actual_returns.len(),
        predicted_returns.len(),
        "length mismatch"
    );
    let count = actual_returns.len();
    if count == 0 {
        return HorizonMetrics {
            horizon,
            count: 0,
            directional_accuracy: 0.0,
            precision_up: 0.0,
            recall_up: 0.0,
            f1_up: 0.0,
            balanced_accuracy: 0.0,
            mae: 0.0,
            rmse: 0.0,
            ic: 0.0,
            tier_score: 0.0,
            naive_tier_score: 0.0,
            tier_hits: vec![0.0; PRICE_TIERS.len()],
        };
    }

    let mut correct_directions = 0;
    let mut tp = 0; // predicted up (>0), actual up (>0)
    let mut fp = 0; // predicted up (>0), actual down (<=0)
    let mut fn_ = 0; // predicted down (<=0), actual up (>0)
    let mut tn = 0; // predicted down (<=0), actual down (<=0)

    let mut sum_abs_err = 0.0;
    let mut sum_sq_err = 0.0;

    let mut sum_act = 0.0;
    let mut sum_pred = 0.0;
    let mut score = 0.0;
    let mut naive = 0.0;
    let mut hits = vec![0usize; PRICE_TIERS.len()];

    for (&act, &pred) in actual_returns.iter().zip(predicted_returns.iter()) {
        let act_up = act > 0.0;
        let pred_up = pred > 0.0;

        if act_up == pred_up {
            correct_directions += 1;
        }

        match (pred_up, act_up) {
            (true, true) => tp += 1,
            (true, false) => fp += 1,
            (false, true) => fn_ += 1,
            (false, false) => tn += 1,
        }

        let err = pred - act;
        sum_abs_err += err.abs();
        sum_sq_err += err * err;

        sum_act += act;
        sum_pred += pred;

        let error = price_error(pred, act);
        score += tier_score(error);
        naive += tier_score(price_error(0.0, act));
        for (hit, (band, _)) in hits.iter_mut().zip(PRICE_TIERS) {
            if error <= band {
                *hit += 1;
            }
        }
    }

    let n = count as f64;
    let dir_acc = correct_directions as f64 / n;
    let precision = if tp + fp > 0 {
        tp as f64 / (tp + fp) as f64
    } else {
        0.0
    };
    let recall = if tp + fn_ > 0 {
        tp as f64 / (tp + fn_) as f64
    } else {
        0.0
    };
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };
    let specificity = if tn + fp > 0 {
        tn as f64 / (tn + fp) as f64
    } else {
        0.0
    };
    let balanced_acc = (recall + specificity) / 2.0;

    let mae = sum_abs_err / n;
    let rmse = (sum_sq_err / n).sqrt();

    // Pearson correlation (IC)
    let mean_act = sum_act / n;
    let mean_pred = sum_pred / n;
    let mut cov = 0.0;
    let mut var_act = 0.0;
    let mut var_pred = 0.0;
    for (&act, &pred) in actual_returns.iter().zip(predicted_returns.iter()) {
        let da = act - mean_act;
        let dp = pred - mean_pred;
        cov += da * dp;
        var_act += da * da;
        var_pred += dp * dp;
    }
    let ic = if var_act > 1e-12 && var_pred > 1e-12 {
        cov / (var_act.sqrt() * var_pred.sqrt())
    } else {
        0.0
    };

    HorizonMetrics {
        horizon,
        count,
        directional_accuracy: dir_acc,
        precision_up: precision,
        recall_up: recall,
        f1_up: f1,
        balanced_accuracy: balanced_acc,
        mae,
        rmse,
        ic,
        tier_score: score / n,
        naive_tier_score: naive / n,
        tier_hits: hits.into_iter().map(|h| h as f64 / n).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_calculation_is_accurate() {
        let actual = vec![0.02, -0.01, 0.05, -0.03, 0.01];
        let pred = vec![0.01, -0.02, 0.03, 0.01, 0.02]; // 4 of 5 directions match (index 3 wrong)
        let m = compute_metrics(1, &actual, &pred);

        assert_eq!(m.count, 5);
        assert!((m.directional_accuracy - 0.8).abs() < 1e-6);
        assert!(m.mae > 0.0);
        assert!(m.rmse > 0.0);
    }

    #[test]
    fn tiers_follow_the_price_error_bands() {
        assert_eq!(tier_score(0.0), 1.0);
        assert_eq!(tier_score(0.01), 1.0);
        assert_eq!(tier_score(0.015), 0.7);
        assert_eq!(tier_score(0.045), 0.1);
        assert_eq!(tier_score(0.051), 0.0);
        // Forecast +10% against realised +10%: same price, full credit.
        assert!(price_error(0.10, 0.10) < 1e-12);
        // Forecast flat against realised +2%: price 1/1.02 of actual, just inside 2%.
        let m = compute_metrics(1, &[0.02, 0.0], &[0.0, 0.0]);
        assert!((m.tier_score - (0.7 + 1.0) / 2.0).abs() < 1e-12);
        assert_eq!(m.tier_skill(), 0.0);
        assert_eq!(m.tier_hits, vec![0.5, 1.0, 1.0, 1.0, 1.0]);
    }
}
