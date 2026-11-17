//! Per-forecast reasons: integrated-gradient contributions of every input feature,
//! grouped by theme, plus an optional short sentence built from those numbers.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::calibrate::{Calibration, HorizonCalibration};
use crate::confidence::ForecastConfidence;
use crate::features::FEATURE_NAMES;
use crate::labels::{
    REASON_ABSTAIN, REASON_DRIVER, REASON_HEADLINE, REASON_ILLIQUID, REASON_INTERVAL,
    REASON_ISSUED, REASON_MARKET, REASON_NO_PROFIT, REASON_PROFIT, feature_label, group_label,
};
use crate::metrics::HORIZONS;
use crate::neural::{MultiHorizonNet, expected_tier_score};

const IG_STEPS: usize = 32;

/// Theme a feature belongs to, used to aggregate contributions.
pub fn feature_group(name: &str) -> &'static str {
    let starts = |p: &str| name.starts_with(p);
    if starts("dart_") {
        "disclosures"
    } else if starts("fund_") {
        "fundamentals"
    } else if starts("mkt_") || starts("rel_ret_") {
        "korea_market"
    } else if starts("volume_") || starts("turnover") || starts("amihud") || starts("log_mcap") {
        "liquidity_size"
    } else if starts("volatility_") || starts("intraday") || starts("close_to_open") {
        "volatility"
    } else if starts("log_ret_")
        || starts("sma_")
        || starts("high_52w")
        || starts("low_52w")
        || starts("max_ret")
    {
        "momentum"
    } else {
        "global_macro"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contribution {
    pub feature: String,
    pub group: String,
    /// Feature value before scaling, in its natural unit.
    pub value: f64,
    /// Standard deviations from the training mean (after clipping).
    pub z_score: f64,
    /// Share of the forecast return attributed to this feature.
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupContribution {
    pub group: String,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonExplanation {
    pub horizon: usize,
    pub predicted_return: f64,
    pub predicted_price: f64,
    /// Expected absolute price error, as a fraction of the price.
    pub expected_error: f64,
    pub price_low: f64,
    pub price_high: f64,
    /// Expected tiered credit of this forecast under the model's own error estimate.
    pub expected_score: f64,
    /// Forecast for an average stock on an average day (all features at the mean).
    pub baseline_return: f64,
    /// Shift from calibration toward (or away from) today's mean forecast across all
    /// stocks: the shown forecast is baseline + market adjustment + contributions.
    #[serde(default)]
    pub market_adjustment: f64,
    /// Forecast minus baseline that the attributions do not account for.
    pub attribution_gap: f64,
    pub groups: Vec<GroupContribution>,
    pub top_features: Vec<Contribution>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<ForecastConfidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    pub symbol: String,
    pub as_of: NaiveDate,
    pub close: f64,
    pub horizons: Vec<HorizonExplanation>,
}

/// Explains the forecast for one scaled feature row. `raw` is the same row before scaling.
/// `calibration` and `date_means` (the mean raw forecast over all stocks on `as_of`)
/// turn the raw network output into the forecast that is shown; attributions are scaled
/// so they explain that number.
#[allow(clippy::too_many_arguments)]
pub fn explain(
    net: &MultiHorizonNet,
    calibration: Option<&Calibration>,
    date_means: &[f64; HORIZONS.len()],
    symbol: &str,
    as_of: NaiveDate,
    close: f64,
    raw: &[f64],
    scaled: &[f64],
    top_k: usize,
) -> Explanation {
    let forecast = net.forecast(scaled);
    let horizons = HORIZONS
        .iter()
        .enumerate()
        .map(|(h_idx, &horizon)| {
            let c = calibration
                .and_then(|cal| cal.horizons.get(h_idx).copied())
                .unwrap_or(HorizonCalibration::IDENTITY);
            let m = date_means[h_idx];
            let (attr, base, pred) = net.integrated_gradients(scaled, h_idx, IG_STEPS);
            let attr: Vec<f64> = attr.iter().map(|a| a * c.relative_weight).collect();
            let mut contributions: Vec<Contribution> = attr
                .iter()
                .enumerate()
                .map(|(i, &c)| Contribution {
                    feature: FEATURE_NAMES[i].to_owned(),
                    group: feature_group(FEATURE_NAMES[i]).to_owned(),
                    value: raw[i],
                    z_score: scaled[i],
                    contribution: c,
                })
                .collect();

            let mut groups: Vec<GroupContribution> = Vec::new();
            for c in &contributions {
                match groups.iter_mut().find(|g| g.group == c.group) {
                    Some(g) => g.contribution += c.contribution,
                    None => groups.push(GroupContribution {
                        group: c.group.clone(),
                        contribution: c.contribution,
                    }),
                }
            }
            groups.sort_by(|a, b| b.contribution.abs().total_cmp(&a.contribution.abs()));
            contributions.sort_by(|a, b| b.contribution.abs().total_cmp(&a.contribution.abs()));
            contributions.truncate(top_k);

            let error = forecast.error_scale[h_idx] * c.error_multiplier;
            let predicted_return = c.adjust(pred, m);
            let baseline_return = c.relative_weight * base;
            let market_adjustment = (c.market_weight - c.relative_weight) * m;
            let predicted_price = (close * (1.0 + predicted_return)).max(0.0);
            let mut h = HorizonExplanation {
                horizon,
                predicted_return,
                predicted_price,
                expected_error: error,
                price_low: predicted_price * (1.0 - error),
                price_high: predicted_price * (1.0 + error),
                expected_score: expected_tier_score(error),
                baseline_return,
                market_adjustment,
                attribution_gap: predicted_return
                    - baseline_return
                    - market_adjustment
                    - attr.iter().sum::<f64>(),
                groups,
                top_features: contributions,
                reason: String::new(),
                confidence: None,
            };
            h.reason = reason(&h);
            h
        })
        .collect();
    Explanation {
        symbol: symbol.to_owned(),
        as_of,
        close,
        horizons,
    }
}

/// Attaches the confidence view and appends its clauses to the reason sentence.
pub fn attach_confidence(h: &mut HorizonExplanation, c: ForecastConfidence) {
    let pct1 = |v: f64| format!("{:.1}", v * 100.0);
    let mut text = String::new();
    text.push(' ');
    text.push_str(
        &REASON_INTERVAL
            .replace("{level}", &format!("{:.0}", c.interval_level * 100.0))
            .replace("{low}", &format!("{:.0}", c.price_low))
            .replace("{high}", &format!("{:.0}", c.price_high))
            .replace("{hit}", &pct1(c.hit_probability)),
    );
    text.push(' ');
    if !c.tradeable {
        text.push_str(REASON_ILLIQUID);
    } else {
        text.push_str(if c.issued {
            REASON_ISSUED
        } else {
            REASON_ABSTAIN
        });
        text.push(' ');
        let template = if c.profit_signal {
            REASON_PROFIT
        } else {
            REASON_NO_PROFIT
        };
        text.push_str(
            &template
                .replace("{prob}", &pct1(c.profit_probability))
                .replace("{cost}", &format!("{:.2}", c.cost * 100.0)),
        );
    }
    h.reason.push_str(&text);
    h.confidence = Some(c);
}

// Fills the headline and driver templates with this forecast's numbers.
fn reason(h: &HorizonExplanation) -> String {
    let pct = |v: f64| format!("{:+.2}", v * 100.0);
    let mut text = REASON_HEADLINE
        .replace("{h}", &h.horizon.to_string())
        .replace("{price}", &format!("{:.0}", h.predicted_price))
        .replace("{ret}", &pct(h.predicted_return))
        .replace("{err}", &format!("{:.2}", h.expected_error * 100.0))
        .replace("{score}", &format!("{:.2}", h.expected_score));
    // The market term is shared by every stock on the date and no feature explains it.
    if h.market_adjustment.abs() >= 5e-4 {
        text.push(' ');
        text.push_str(&REASON_MARKET.replace("{mkt}", &pct(h.market_adjustment)));
    }
    for c in h.top_features.iter().take(3) {
        text.push(' ');
        text.push_str(
            &REASON_DRIVER
                .replace("{feature}", feature_label(&c.feature))
                .replace("{group}", group_label(&c.group))
                .replace("{z}", &format!("{:+.1}", c.z_score))
                .replace("{contrib}", &pct(c.contribution)),
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::NUM_FEATURES;

    #[test]
    fn every_feature_has_a_group_and_label() {
        for name in FEATURE_NAMES {
            let group = feature_group(name);
            assert!(!group_label(group).is_empty(), "{name}");
            assert_ne!(feature_label(name), name, "{name} has no label");
        }
        assert_eq!(feature_group("log_ret_1"), "momentum");
        assert_eq!(feature_group("vix_log"), "global_macro");
        assert_eq!(feature_group("turnover_20"), "liquidity_size");
    }

    #[test]
    fn explanation_attributions_close_the_gap() {
        let net = MultiHorizonNet::new(8, 3);
        let raw: Vec<f64> = (0..NUM_FEATURES).map(|i| i as f64 * 0.01).collect();
        let scaled: Vec<f64> = (0..NUM_FEATURES)
            .map(|i| ((i % 7) as f64 - 3.0) * 0.5)
            .collect();
        let date = NaiveDate::from_ymd_opt(2026, 11, 10).expect("date");
        let means = [0.01, -0.02, 0.0, 0.03, 0.05];
        let e = explain(
            &net, None, &means, "005930", date, 50_000.0, &raw, &scaled, 5,
        );
        assert_eq!(e.horizons.len(), HORIZONS.len());
        for h in &e.horizons {
            assert_eq!(h.top_features.len(), 5);
            assert!(h.attribution_gap.abs() < 1e-3, "{}", h.attribution_gap);
            assert!(h.price_low <= h.predicted_price && h.predicted_price <= h.price_high);
            assert!(!h.reason.contains('{'), "unfilled template: {}", h.reason);
        }
        // Calibrated: shown forecast = baseline + market adjustment + contributions.
        let mut cal = Calibration::default();
        for c in &mut cal.horizons {
            c.market_weight = 0.0;
            c.relative_weight = 0.4;
            c.error_multiplier = 2.0;
        }
        let e2 = explain(
            &net,
            Some(&cal),
            &means,
            "005930",
            date,
            50_000.0,
            &raw,
            &scaled,
            5,
        );
        for (h, (a, b)) in e.horizons.iter().zip(&e2.horizons).enumerate() {
            let expected = 0.4 * (a.predicted_return - means[h]);
            assert!((b.predicted_return - expected).abs() < 1e-9);
            assert!(b.attribution_gap.abs() < 1e-3);
            assert!((b.expected_error - 2.0 * a.expected_error).abs() < 1e-12);
        }
    }
}
