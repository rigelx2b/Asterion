//! Training and evaluation runner for Asterion models.

use std::fs;
use std::path::{Path, PathBuf};

use asterion_core::Symbol;
use asterion_data::progress::EpochPoint;
use asterion_data::{ProgressTracker, RawCache};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::baseline::{
    BaselineModel, MeanReversionBaseline, MomentumBaseline, NoChangeBaseline, RidgeBaseline,
};
use crate::calibrate::{self, Calibration, VariantScores};
use crate::confidence::{self, ConfidenceModel, ConfidenceReport};
use crate::context::MarketContext;
use crate::explain::feature_group;
use crate::features::{
    DatasetSplits, FEATURE_NAMES, NUM_FEATURES, Sample, StandardScaler, bars_and_extras,
    extract_symbol_samples, to_excess_returns,
};
use crate::metrics::{EvaluationReport, HORIZONS, price_error, tier_score};
use crate::neural::{
    AdamW, Forecast, LossConfig, MultiHorizonNet, default_threads, evaluate_forecasts, get_params,
    mean_loss, set_params, shuffle_indices, train_epoch_with_progress,
};

// What the model is asked to predict for each horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    // The stock's own forward return, so a forecast maps to a price.
    Raw,
    // Forward return minus the same-date median across the universe. Used only as the
    // training target; forecasts are still scored against realised prices.
    Excess,
}

#[derive(Debug, Clone)]
pub struct TrainConfig {
    pub epochs: usize,
    pub batch_size: usize,
    pub lr: f64,
    pub hidden_dim: usize,
    pub seed: u64,
    // Validation runs every this many epochs (and after the last one).
    pub eval_every: usize,
    // Validation runs without improvement before training stops early.
    pub patience: usize,
    pub target: TargetKind,
    pub loss: LossConfig,
    pub threads: usize,
    pub val_start: NaiveDate,
    // Validation rows before this date fit the calibration; later ones select the model.
    pub calib_split: NaiveDate,
    pub test_start: NaiveDate,
    // Whether the held-out test split is scored at the end of the run.
    pub report_test: bool,
    // Where the best weights and the feature scaler are written; `None` skips saving.
    pub model_out: Option<PathBuf>,
}

// Everything needed to run the trained network on new feature rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedModel {
    pub feature_names: Vec<String>,
    pub horizons: Vec<usize>,
    pub target: TargetKind,
    pub val_start: NaiveDate,
    // First date of validation B; older saved models predate the field.
    #[serde(default = "default_calib_split")]
    pub calib_split: NaiveDate,
    pub test_start: NaiveDate,
    pub symbols: usize,
    pub train_samples: usize,
    pub best_epoch: usize,
    // Mean over horizons of validation tiered credit minus the no-change credit.
    #[serde(default)]
    pub best_val_skill: f64,
    pub best_val_ic: f64,
    #[serde(default)]
    pub loss: Option<LossConfig>,
    // Shrinkage applied to raw network forecasts; `None` for models saved before it existed.
    #[serde(default)]
    pub calibration: Option<Calibration>,
    // Selective cutoff and empirical intervals fitted on validation B.
    #[serde(default)]
    pub confidence: Option<ConfidenceModel>,
    pub scaler: StandardScaler,
    pub net: MultiHorizonNet,
}

fn default_calib_split() -> NaiveDate {
    NaiveDate::from_ymd_opt(2024, 7, 1).expect("valid date")
}

impl SavedModel {
    pub fn save(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let model: SavedModel = serde_json::from_slice(&fs::read(path)?)?;
        if model.feature_names != FEATURE_NAMES {
            return Err(format!(
                "{} was trained on {} features that differ from the current {}; retrain it",
                path.display(),
                model.feature_names.len(),
                NUM_FEATURES
            )
            .into());
        }
        Ok(model)
    }
}

impl Default for TrainConfig {
    fn default() -> Self {
        TrainConfig {
            epochs: 40,
            batch_size: 1024,
            lr: 0.002,
            hidden_dim: 192,
            seed: 42,
            eval_every: 2,
            patience: 5,
            target: TargetKind::Raw,
            loss: LossConfig::default(),
            threads: default_threads(),
            val_start: NaiveDate::from_ymd_opt(2024, 1, 1).expect("valid date"),
            calib_split: NaiveDate::from_ymd_opt(2024, 7, 1).expect("valid date"),
            test_start: NaiveDate::from_ymd_opt(2025, 1, 1).expect("valid date"),
            report_test: false,
            model_out: None,
        }
    }
}

// How well the expected credit predicted by the scale heads matches the realised credit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationBin {
    pub count: usize,
    pub predicted_score: f64,
    pub realised_score: f64,
    pub naive_score: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonDiagnostics {
    pub horizon: usize,
    // Spread of the forecasts; near zero means the model collapsed to a constant.
    pub prediction_std: f64,
    pub actual_std: f64,
    // Bins of rising model confidence, each holding the same number of rows.
    pub calibration: Vec<CalibrationBin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureImportance {
    pub feature: String,
    pub group: String,
    // Mean absolute integrated-gradient contribution, in return units.
    pub mean_abs_contribution: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingReport {
    pub best_epoch: usize,
    pub best_val_skill: f64,
    pub calibration: Calibration,
    pub baselines_val: Vec<EvaluationReport>,
    pub neural_train: EvaluationReport,
    pub neural_val_raw: EvaluationReport,
    pub neural_val_calibrated: EvaluationReport,
    pub neural_test: Option<EvaluationReport>,
    pub val_variants: Vec<VariantScores>,
    pub val_diagnostics: Vec<HorizonDiagnostics>,
    // Per horizon, features ranked by importance on a validation subsample.
    pub importance: Vec<Vec<FeatureImportance>>,
}

#[derive(Debug)]
pub struct TrainingResult {
    pub report: TrainingReport,
}

fn mean_skill(report: &EvaluationReport) -> f64 {
    report.horizons.iter().map(|h| h.tier_skill()).sum::<f64>() / report.horizons.len() as f64
}

fn mean_ic(report: &EvaluationReport) -> f64 {
    report.horizons.iter().map(|h| h.ic).sum::<f64>() / report.horizons.len() as f64
}

fn std_dev(values: impl Iterator<Item = f64> + Clone) -> f64 {
    let n = values.clone().count().max(1) as f64;
    let mean = values.clone().sum::<f64>() / n;
    (values.map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt()
}

pub fn diagnostics(
    forecasts: &[Forecast],
    samples: &[Sample],
    bins: usize,
) -> Vec<HorizonDiagnostics> {
    HORIZONS
        .iter()
        .enumerate()
        .map(|(h_idx, &horizon)| {
            let mut rows: Vec<(f64, f64, f64)> = forecasts
                .iter()
                .zip(samples)
                .map(|(f, s)| {
                    let actual = s.forward_returns[h_idx];
                    (
                        f.expected_score(h_idx),
                        tier_score(price_error(f.returns[h_idx], actual)),
                        tier_score(price_error(0.0, actual)),
                    )
                })
                .collect();
            rows.sort_by(|a, b| a.0.total_cmp(&b.0));
            let size = rows.len().div_ceil(bins.max(1)).max(1);
            let calibration = rows
                .chunks(size)
                .map(|c| {
                    let n = c.len() as f64;
                    CalibrationBin {
                        count: c.len(),
                        predicted_score: c.iter().map(|r| r.0).sum::<f64>() / n,
                        realised_score: c.iter().map(|r| r.1).sum::<f64>() / n,
                        naive_score: c.iter().map(|r| r.2).sum::<f64>() / n,
                    }
                })
                .collect();
            HorizonDiagnostics {
                horizon,
                prediction_std: std_dev(forecasts.iter().map(|f| f.returns[h_idx])),
                actual_std: std_dev(samples.iter().map(|s| s.forward_returns[h_idx])),
                calibration,
            }
        })
        .collect()
}

fn print_diagnostics(title: &str, diags: &[HorizonDiagnostics]) {
    println!("\n=== {title}: confidence calibration (low -> high confidence bins) ===");
    for d in diags {
        println!(
            "t+{:<3} pred_std={:.4} actual_std={:.4}",
            d.horizon, d.prediction_std, d.actual_std
        );
        for b in &d.calibration {
            println!(
                "      n={:<8} expected={:.3} realised={:.3} naive={:.3} skill={:+.4}",
                b.count,
                b.predicted_score,
                b.realised_score,
                b.naive_score,
                b.realised_score - b.naive_score
            );
        }
    }
}

// Mean absolute attribution per feature over an evenly spaced subsample of `samples`.
fn feature_importance(
    net: &MultiHorizonNet,
    samples: &[Sample],
    max_rows: usize,
    threads: usize,
) -> Vec<Vec<FeatureImportance>> {
    let step = (samples.len() / max_rows.max(1)).max(1);
    let rows: Vec<&Sample> = samples.iter().step_by(step).take(max_rows).collect();
    let per_thread = rows.len().div_ceil(threads.max(1)).max(1);
    HORIZONS
        .iter()
        .enumerate()
        .map(|(h_idx, _)| {
            let totals = std::thread::scope(|scope| {
                let handles: Vec<_> = rows
                    .chunks(per_thread)
                    .map(|part| {
                        scope.spawn(move || {
                            let mut acc = vec![0.0; NUM_FEATURES];
                            for s in part {
                                let (attr, _, _) = net.integrated_gradients(&s.features, h_idx, 16);
                                for (a, v) in acc.iter_mut().zip(attr) {
                                    *a += v.abs();
                                }
                            }
                            acc
                        })
                    })
                    .collect();
                let mut acc = vec![0.0; NUM_FEATURES];
                for h in handles {
                    for (a, v) in acc.iter_mut().zip(h.join().unwrap_or_default()) {
                        *a += v;
                    }
                }
                acc
            });
            let n = rows.len().max(1) as f64;
            let mut list: Vec<FeatureImportance> = totals
                .iter()
                .enumerate()
                .map(|(i, t)| FeatureImportance {
                    feature: FEATURE_NAMES[i].to_owned(),
                    group: feature_group(FEATURE_NAMES[i]).to_owned(),
                    mean_abs_contribution: t / n,
                })
                .collect();
            list.sort_by(|a, b| b.mean_abs_contribution.total_cmp(&a.mean_abs_contribution));
            list
        })
        .collect()
}

pub fn load_samples(
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
    progress: &mut ProgressTracker,
) -> Result<(Vec<Sample>, usize), Box<dyn std::error::Error>> {
    let mut all_samples = Vec::new();
    let mut loaded_count = 0;
    progress.stage("loading", symbols.len() as u64);
    for (i, symbol) in symbols.iter().enumerate() {
        progress.current(symbol.code());
        progress.advance(i as u64);
        let quotes = cache.load_quotes(symbol)?;
        let Some(latest) = quotes.last() else {
            continue;
        };
        let (bars, extras) = bars_and_extras(&quotes);
        let samples = extract_symbol_samples(
            symbol.code(),
            &bars,
            &extras,
            &latest.market_category,
            context,
        );
        if !samples.is_empty() {
            loaded_count += 1;
        }
        all_samples.extend(samples);
    }
    Ok((all_samples, loaded_count))
}

// Validation forecasts of one network state, calibrated on slice A and scored on slice B.
struct Evaluation {
    calibration: Calibration,
    raw_b: Vec<Forecast>,
    report_b: EvaluationReport,
}

fn evaluate_state(
    net: &MultiHorizonNet,
    val_a: &[Sample],
    val_b: &[Sample],
    threads: usize,
) -> Evaluation {
    let raw_a = net.forecast_all(val_a, threads);
    let calibration = calibrate::fit(&raw_a, val_a, 0.0);
    let raw_b = net.forecast_all(val_b, threads);
    let calibrated_b = calibration.apply(&raw_b, val_b);
    let report_b = evaluate_forecasts(&calibrated_b, val_b, "Validation B (calibrated)");
    Evaluation {
        calibration,
        raw_b,
        report_b,
    }
}

fn print_variants(title: &str, variants: &[VariantScores]) {
    println!("\n=== {title}: forecast variants (tiered credit) ===");
    println!(
        "{:<8} {:<9} {:<9} {:<9} {:<10} {:<10} {:<9} {:<9}",
        "Horizon", "Naive", "Raw", "Demeaned", "Calibrated", "XS-IC", "DateIC", "Dates"
    );
    for v in variants {
        println!(
            "t+{:<6} {:<9.4} {:<9.4} {:<9.4} {:<10.4} {:<+10.4} {:<+9.4} {:<9}",
            v.horizon,
            v.naive,
            v.raw,
            v.demeaned,
            v.calibrated,
            v.cross_sectional_ic,
            v.date_mean_ic,
            v.dates
        );
    }
}

pub fn run_training(
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
    config: &TrainConfig,
    progress: &mut ProgressTracker,
) -> Result<TrainingResult, Box<dyn std::error::Error>> {
    for (name, rows) in context.summary() {
        if rows == 0 {
            println!("Warning: {name} is not cached; its features stay at neutral values.");
        }
    }
    println!(
        "disclosures={} fundamental_reports={}",
        context.disclosures.len(),
        context.fundamentals.len()
    );
    println!("Loading cached quotes for {} symbols...", symbols.len());
    let (all_samples, loaded_count) = load_samples(cache, symbols, context, progress)?;
    println!(
        "Extracted {} samples across {} symbols, {} features each.",
        all_samples.len(),
        loaded_count,
        NUM_FEATURES
    );

    let splits = DatasetSplits::split_and_scale(all_samples, config.val_start, config.test_start);
    let DatasetSplits {
        train,
        val,
        test,
        scaler,
    } = splits;
    // Slice A fits the calibration; slice B selects the checkpoint and is reported.
    let (val_a, val_b): (Vec<Sample>, Vec<Sample>) =
        val.into_iter().partition(|s| s.date < config.calib_split);
    let val_a: Vec<Sample> = val_a
        .into_iter()
        .filter(|s| s.target_end < config.calib_split)
        .collect();
    println!(
        "Dataset partitioned chronologically (rows whose targets cross a boundary are purged):\n  Train: {} samples (< {})\n  Val A: {} samples ({}..{}, calibration fit)\n  Val B: {} samples ({}..{}, model selection)\n  Test:  {} samples (>= {}){}\n",
        train.len(),
        config.val_start,
        val_a.len(),
        config.val_start,
        config.calib_split,
        val_b.len(),
        config.calib_split,
        config.test_start,
        test.len(),
        config.test_start,
        if config.report_test {
            ""
        } else {
            " [held out, not scored]"
        }
    );
    progress.counter("symbols", loaded_count as f64);
    progress.counter("samples_train", train.len() as f64);
    progress.counter("samples_val", (val_a.len() + val_b.len()) as f64);
    progress.counter("samples_test", test.len() as f64);

    // 1. Baselines on validation B, the slice the network is judged on.
    println!("Evaluating baselines on validation B...");
    progress.stage("baselines", 4);
    let ridge = RidgeBaseline::fit(&train, 10.0);
    let baselines: [&dyn BaselineModel; 4] = [
        &NoChangeBaseline,
        &MomentumBaseline,
        &MeanReversionBaseline,
        &ridge,
    ];
    let mut baselines_val = Vec::new();
    for (i, model) in baselines.iter().enumerate() {
        let report = model.evaluate(&val_b, "Validation B");
        report.print_table();
        progress.advance(i as u64 + 1);
        progress.table(&report);
        baselines_val.push(report);
    }

    // Excess targets change only what the network fits; validation and test are always
    // scored against realised prices.
    let train = if config.target == TargetKind::Excess {
        let train = to_excess_returns(train);
        println!(
            "target=excess (training split only) train_samples={}",
            train.len()
        );
        train
    } else {
        println!("target=raw train_samples={}", train.len());
        train
    };

    // 2. Network
    println!(
        "\nInitializing network (hidden_dim={}, threads={}, loss={:?})...",
        config.hidden_dim, config.threads, config.loss
    );
    let mut net = MultiHorizonNet::new(config.hidden_dim, config.seed);
    let num_params = get_params(&net).len();
    println!("Model parameters: {num_params}");
    progress.counter("params", num_params as f64);

    let mut optimizer = AdamW::new(num_params, config.lr);
    // Kept weights maximise calibrated tiered credit over no-change on slice B. The
    // untrained state (epoch 0, close to no-change) is a candidate too.
    let initial = evaluate_state(&net, &val_a, &val_b, config.threads);
    let mut best_val_skill = mean_skill(&initial.report_b);
    let mut best_val_ic = mean_ic(&initial.report_b);
    let mut best_epoch = 0;
    let mut best_params = get_params(&net);
    let mut best_calibration = initial.calibration;
    println!("Epoch   0: val_b_skill={best_val_skill:+.5} (untrained)");
    let mut stale = 0;
    let mut order: Vec<usize> = (0..train.len()).collect();
    let mut shuffle_state = config.seed;

    println!("\nStarting training for up to {} epochs...", config.epochs);
    progress.stage("training", config.epochs as u64);
    let eval_every = config.eval_every.max(1);
    for epoch in 1..=config.epochs {
        // Cosine decay to 5% of the starting rate.
        let phase = (epoch - 1) as f64 / config.epochs.max(1) as f64;
        optimizer.lr =
            config.lr * (0.05 + 0.95 * 0.5 * (1.0 + (std::f64::consts::PI * phase).cos()));
        shuffle_indices(&mut order, &mut shuffle_state);
        let loss = train_epoch_with_progress(
            &mut net,
            &mut optimizer,
            &train,
            &order,
            config.batch_size,
            &config.loss,
            config.threads,
            &mut |done, total| progress.step(done, total),
        );

        let mut val_metric = None;
        if epoch % eval_every == 0 || epoch == config.epochs {
            let eval = evaluate_state(&net, &val_a, &val_b, config.threads);
            let skill = mean_skill(&eval.report_b);
            let ic = mean_ic(&eval.report_b);
            let raw_skill = mean_skill(&evaluate_forecasts(&eval.raw_b, &val_b, "raw"));
            let spread = std_dev(eval.raw_b.iter().map(|f| f.returns[0]));
            let weights: Vec<String> = eval
                .calibration
                .horizons
                .iter()
                .map(|c| format!("{:.2}", c.relative_weight))
                .collect();
            if skill > best_val_skill {
                best_val_skill = skill;
                best_val_ic = ic;
                best_epoch = epoch;
                best_params = get_params(&net);
                best_calibration = eval.calibration;
                stale = 0;
            } else {
                stale += 1;
            }
            println!(
                "Epoch {epoch:>3}/{}: lr={:.5} train_loss={loss:.6} val_b_skill={skill:+.5} raw_skill={raw_skill:+.5} val_b_ic={ic:+.4} pred_std_t1={spread:.5} rel_w=[{}] best_epoch={best_epoch}",
                config.epochs,
                optimizer.lr,
                weights.join(",")
            );
            val_metric = Some(skill);
            progress.counter("best_val_skill", best_val_skill);
            progress.counter("best_val_ic", best_val_ic);
            progress.counter("best_epoch", best_epoch as f64);
            if stale >= config.patience {
                println!("No validation improvement in {stale} evaluations; stopping early.");
                progress.advance(epoch as u64);
                break;
            }
        } else {
            println!(
                "Epoch {epoch:>3}/{}: lr={:.5} train_loss={loss:.6}",
                config.epochs, optimizer.lr
            );
        }
        progress.advance(epoch as u64);
        progress.epoch(EpochPoint {
            epoch: epoch as u32,
            train_loss: loss,
            val_accuracy: val_metric,
        });
    }

    set_params(&mut net, &best_params);
    let mut calibration = best_calibration;

    // Market weights: fitted on A, kept only where slice B confirms them.
    let raw_a = net.forecast_all(&val_a, config.threads);
    let raw_b = net.forecast_all(&val_b, config.threads);
    let fitted_market = calibrate::fit_market_weights(&raw_a, &val_a, &calibration);
    let base_scores = calibrate::variant_scores(&raw_b, &val_b, &calibration);
    for (h, w) in fitted_market.iter().enumerate() {
        if *w == 0.0 {
            continue;
        }
        let mut trial = calibration.clone();
        trial.horizons[h] = calibrate::fit(&raw_a, &val_a, *w).horizons[h];
        let trial_scores = calibrate::variant_scores(&raw_b, &val_b, &trial);
        let keep = trial_scores[h].calibrated > base_scores[h].calibrated;
        println!(
            "t+{} market_weight={w:.2} on B: {:.5} vs {:.5} without -> {}",
            HORIZONS[h],
            trial_scores[h].calibrated,
            base_scores[h].calibrated,
            if keep { "kept" } else { "dropped" }
        );
        if keep {
            calibration.horizons[h] = trial.horizons[h];
        }
    }
    for (h, c) in calibration.horizons.iter().enumerate() {
        println!(
            "calibration t+{}: market_weight={:.2} relative_weight={:.2} error_multiplier={:.3}",
            HORIZONS[h], c.market_weight, c.relative_weight, c.error_multiplier
        );
    }
    let calibrated_b = calibration.apply(&raw_b, &val_b);
    let neural_val_calibrated =
        evaluate_forecasts(&calibrated_b, &val_b, "Validation B (calibrated)");
    best_val_skill = mean_skill(&neural_val_calibrated);
    let confidence_model = ConfidenceModel::fit(&calibrated_b, &val_b);
    if let Some(c) = &confidence_model {
        print_cutoffs(c);
    }
    println!(
        "best_epoch={best_epoch} val_b_skill={best_val_skill:+.5} val_b_loss_raw={:.6}",
        mean_loss(&net, &val_b, &config.loss)
    );

    if let Some(path) = &config.model_out {
        let saved = SavedModel {
            feature_names: FEATURE_NAMES.iter().map(|n| (*n).to_owned()).collect(),
            horizons: HORIZONS.to_vec(),
            target: config.target,
            val_start: config.val_start,
            calib_split: config.calib_split,
            test_start: config.test_start,
            symbols: loaded_count,
            train_samples: train.len(),
            best_epoch,
            best_val_skill,
            best_val_ic,
            loss: Some(config.loss),
            calibration: Some(calibration.clone()),
            confidence: confidence_model.clone(),
            scaler: scaler.clone(),
            net: net.clone(),
        };
        saved.save(path)?;
        println!("model_out={}", path.display());
    }

    // Final evaluation
    progress.stage("evaluating", 3);
    let neural_train = net.evaluate(&train, "Train In-Sample (raw)");
    progress.advance(1);
    let neural_val_raw = evaluate_forecasts(&raw_b, &val_b, "Validation B (raw)");
    progress.advance(2);
    neural_train.print_table();
    neural_val_raw.print_table();
    neural_val_calibrated.print_table();
    progress.table(&neural_train);
    progress.table(&neural_val_raw);
    progress.table(&neural_val_calibrated);

    let val_variants = calibrate::variant_scores(&raw_b, &val_b, &calibration);
    print_variants("Validation B", &val_variants);
    let val_diagnostics = diagnostics(&calibrated_b, &val_b, 5);
    print_diagnostics("Validation B", &val_diagnostics);

    let neural_test = config.report_test.then(|| {
        let raw_test = net.forecast_all(&test, config.threads);
        let calibrated = calibration.apply(&raw_test, &test);
        let report = evaluate_forecasts(&calibrated, &test, "Out-Of-Sample Test (calibrated)");
        report.print_table();
        progress.table(&report);
        print_variants(
            "Test",
            &calibrate::variant_scores(&raw_test, &test, &calibration),
        );
        print_diagnostics("Test", &diagnostics(&calibrated, &test, 5));
        if let Some(c) = &confidence_model {
            let r = confidence::evaluate(
                c,
                &raw_test,
                &calibrated,
                &test,
                "Test",
                confidence::DEFAULT_COST,
            );
            confidence::print_report(&r);
            progress.table(&r);
        }
        report
    });
    progress.advance(3);

    println!("\nComputing feature importance on a validation B subsample...");
    let mut importance = feature_importance(&net, &val_b, 3000, config.threads);
    for (h_idx, list) in importance.iter_mut().enumerate() {
        let w = calibration.horizons[h_idx].relative_weight;
        for f in list.iter_mut() {
            f.mean_abs_contribution *= w;
        }
        let top: Vec<String> = list
            .iter()
            .take(8)
            .map(|f| format!("{}={:.5}", f.feature, f.mean_abs_contribution))
            .collect();
        println!("t+{:<3} {}", HORIZONS[h_idx], top.join(" "));
    }

    let report = TrainingReport {
        best_epoch,
        best_val_skill,
        calibration,
        baselines_val,
        neural_train,
        neural_val_raw,
        neural_val_calibrated,
        neural_test,
        val_variants,
        val_diagnostics,
        importance,
    };
    if let Some(path) = &config.model_out {
        let report_path = path.with_extension("report.json");
        fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
        println!("report_out={}", report_path.display());
    }
    Ok(TrainingResult { report })
}

/// Scores a frozen model on every row dated on or after its `test_start`, with the
/// scaler and calibration saved at training time. Meant to run once per final model.
pub fn evaluate_saved(
    model: &SavedModel,
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
    progress: &mut ProgressTracker,
) -> Result<EvaluationReport, Box<dyn std::error::Error>> {
    let (all_samples, loaded_count) = load_samples(cache, symbols, context, progress)?;
    let mut test: Vec<Sample> = all_samples
        .into_iter()
        .filter(|s| s.date >= model.test_start)
        .collect();
    test.sort_by_key(|s| s.date);
    for s in &mut test {
        model.scaler.transform(s);
    }
    println!(
        "test_samples={} symbols={loaded_count} from={} best_epoch={} val_b_skill={:+.5}",
        test.len(),
        model.test_start,
        model.best_epoch,
        model.best_val_skill
    );
    let threads = default_threads();
    let calibration = model.calibration.clone().unwrap_or_default();
    let raw = model.net.forecast_all(&test, threads);
    let calibrated = calibration.apply(&raw, &test);

    NoChangeBaseline.evaluate(&test, "Test").print_table();
    evaluate_forecasts(&raw, &test, "Test (raw)").print_table();
    let report = evaluate_forecasts(&calibrated, &test, "Test (calibrated)");
    report.print_table();
    progress.table(&report);
    print_variants(
        "Test",
        &calibrate::variant_scores(&raw, &test, &calibration),
    );
    print_diagnostics("Test", &diagnostics(&calibrated, &test, 5));
    if let Some(c) = &model.confidence {
        let r = confidence::evaluate(
            c,
            &raw,
            &calibrated,
            &test,
            "Test",
            confidence::DEFAULT_COST,
        );
        confidence::print_report(&r);
        progress.table(&r);
    }
    Ok(report)
}

fn print_cutoffs(model: &ConfidenceModel) {
    println!(
        "
confidence fitted on {}..{} ({} tradeable rows), target hit<={:.0}% rate {:.0}%:",
        model.fit_from,
        model.fit_to,
        model.fit_rows,
        model.hit_band * 100.0,
        model.target_hit_rate * 100.0
    );
    for h in &model.horizons {
        match h.max_scale {
            Some(m) => println!(
                "  t+{:<3} cutoff scale<={m:.4} coverage {:.1}% hit {:.1}% block lower bound {:.1}%",
                h.horizon,
                h.fit_coverage * 100.0,
                h.fit_hit_rate.unwrap_or(f64::NAN) * 100.0,
                h.fit_lower_bound.unwrap_or(f64::NAN) * 100.0
            ),
            None => println!("  t+{:<3} no cutoff reaches the target", h.horizon),
        }
    }
}

/// Fits the selective cutoff and intervals of a frozen model on its validation B rows,
/// stores them in `model`, and scores them on validation B and on test.
pub fn fit_confidence(
    model: &mut SavedModel,
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
    cost: f64,
    progress: &mut ProgressTracker,
) -> Result<(ConfidenceReport, ConfidenceReport), Box<dyn std::error::Error>> {
    let (all_samples, loaded_count) = load_samples(cache, symbols, context, progress)?;
    let (mut val_b, mut test) = (Vec::new(), Vec::new());
    for s in all_samples {
        if s.date >= model.test_start {
            test.push(s);
        } else if s.date >= model.calib_split && s.target_end < model.test_start {
            val_b.push(s);
        }
    }
    for split in [&mut val_b, &mut test] {
        split.sort_by_key(|s| s.date);
        for s in split.iter_mut() {
            model.scaler.transform(s);
        }
    }
    println!(
        "symbols={loaded_count} val_b={} ({}..{}) test={} (>= {})",
        val_b.len(),
        model.calib_split,
        model.test_start,
        test.len(),
        model.test_start
    );
    let threads = default_threads();
    let calibration = model.calibration.clone().unwrap_or_default();
    progress.stage("forecasting", 2);
    let raw_b = model.net.forecast_all(&val_b, threads);
    let cal_b = calibration.apply(&raw_b, &val_b);
    progress.advance(1);
    let raw_t = model.net.forecast_all(&test, threads);
    let cal_t = calibration.apply(&raw_t, &test);
    progress.advance(2);
    let fitted = ConfidenceModel::fit(&cal_b, &val_b)
        .ok_or("too few tradeable validation B rows to fit the confidence model")?;
    print_cutoffs(&fitted);
    let report_b =
        confidence::evaluate(&fitted, &raw_b, &cal_b, &val_b, "Validation B (fit)", cost);
    let report_t = confidence::evaluate(&fitted, &raw_t, &cal_t, &test, "Test", cost);
    confidence::print_report(&report_b);
    confidence::print_report(&report_t);
    progress.table(&report_b);
    progress.table(&report_t);
    model.confidence = Some(fitted);
    Ok((report_b, report_t))
}
