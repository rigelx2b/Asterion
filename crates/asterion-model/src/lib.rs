//! `asterion-model`: Deep learning and quantitative forecasting engine for Asterion.
//!
//! Forecasts the price at several horizons, scores forecasts by tiered price accuracy
//! against a no-change baseline, and explains each forecast through feature attributions.

pub mod baseline;
pub mod calibrate;
pub mod confidence;
pub mod context;
pub mod disclosures;
pub mod explain;
pub mod features;
pub mod fundamentals;
pub mod history;
mod labels;
pub mod ledger;
pub mod metrics;
pub mod neural;
pub mod predict;
pub mod rank_ledger;
pub mod research;
pub mod strategy;
pub mod survivorship;
pub mod train;

pub use baseline::{
    BaselineModel, MeanReversionBaseline, MomentumBaseline, NoChangeBaseline, RidgeBaseline,
};
pub use confidence::{ConfidenceModel, ConfidenceReport};
pub use context::{DailySeries, MarketContext};
pub use disclosures::DisclosureIndex;
pub use explain::{Explanation, HorizonExplanation, explain, feature_group};
pub use features::{
    DatasetSplits, FEATURE_NAMES, NUM_FEATURES, Sample, StandardScaler, to_excess_returns,
};
pub use fundamentals::FundamentalIndex;
pub use metrics::{
    EvaluationReport, HORIZONS, HorizonMetrics, PRICE_TIERS, compute_metrics, price_error,
    tier_score,
};
pub use neural::{AdamW, Forecast, LossConfig, MultiHorizonNet};
pub use predict::predict_latest;
pub use train::{
    SavedModel, TargetKind, TrainConfig, TrainingReport, TrainingResult, evaluate_saved,
    fit_confidence, run_training,
};
