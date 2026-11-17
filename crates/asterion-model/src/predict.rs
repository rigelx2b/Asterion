//! Forecasts and reasons for the latest cached session of each symbol.

use std::collections::{HashMap, HashSet};

use asterion_core::Symbol;
use asterion_data::RawCache;
use chrono::NaiveDate;

use crate::calibrate::date_means;
use crate::context::MarketContext;
use crate::explain::{Explanation, attach_confidence, explain};
use crate::features::{bars_and_extras, features_at};
use crate::metrics::HORIZONS;
use crate::neural::Forecast;
use crate::train::SavedModel;

// Fewest stocks sharing a date for their mean forecast to stand in for the market part.
pub const MIN_CROSS_SECTION: usize = 100;

pub(crate) struct Row {
    pub symbol: String,
    pub as_of: NaiveDate,
    pub close: f64,
    pub raw: Vec<f64>,
    pub scaled: Vec<f64>,
    pub forecast: Forecast,
    pub trading_value: f64,
}

/// Explanations for `requested`. The calibrated forecast removes the mean forecast of all
/// stocks on the same date, so `cross_section` (normally every cached symbol) is forecast
/// too; symbols with too little history are skipped.
pub fn predict_latest(
    model: &SavedModel,
    cache: &RawCache,
    requested: &[Symbol],
    cross_section: &[Symbol],
    context: &MarketContext,
    top_k: usize,
    cost: f64,
) -> Result<Vec<Explanation>, Box<dyn std::error::Error>> {
    let symbols: Vec<Symbol> = cross_section.iter().chain(requested).cloned().collect();
    let rows = collect_rows(model, cache, &symbols, context)?;

    let forecasts: Vec<Forecast> = rows.iter().map(|r| r.forecast).collect();
    let dates: Vec<NaiveDate> = rows.iter().map(|r| r.as_of).collect();
    let means = date_means(&forecasts, &dates);
    let mut counts: HashMap<NaiveDate, usize> = HashMap::new();
    for d in &dates {
        *counts.entry(*d).or_default() += 1;
    }
    let modal = counts
        .iter()
        .max_by_key(|(d, n)| (**n, **d))
        .map(|(d, _)| *d);
    let modal_count = modal.map_or(0, |d| counts[&d]);
    if model.calibration.is_some() && modal_count < MIN_CROSS_SECTION {
        return Err(format!(
            "calibrated forecasts need at least {MIN_CROSS_SECTION} stocks on the same date; only {modal_count} found"
        )
        .into());
    }
    let modal_means = rows
        .iter()
        .zip(&means)
        .find(|(r, _)| Some(r.as_of) == modal)
        .map_or([0.0; HORIZONS.len()], |(_, m)| *m);

    let wanted: HashSet<&str> = requested.iter().map(Symbol::code).collect();
    Ok(rows
        .iter()
        .zip(&means)
        .filter(|(r, _)| wanted.contains(r.symbol.as_str()))
        .map(|(r, m)| {
            // A stale date with a thin cross-section borrows the most common date's mean.
            let m = if counts[&r.as_of] >= MIN_CROSS_SECTION {
                *m
            } else {
                modal_means
            };
            let mut e = explain(
                &model.net,
                model.calibration.as_ref(),
                &m,
                &r.symbol,
                r.as_of,
                r.close,
                &r.raw,
                &r.scaled,
                top_k,
            );
            if let Some(conf) = &model.confidence {
                for (h, hc) in e.horizons.iter_mut().zip(&conf.horizons) {
                    let view = hc.view(
                        h.predicted_return,
                        h.expected_error,
                        r.close,
                        r.trading_value,
                        cost,
                    );
                    attach_confidence(h, view);
                }
            }
            e
        })
        .collect())
}

// Latest-session forecast of every symbol with enough history, each code once.
pub(crate) fn collect_rows(
    model: &SavedModel,
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
) -> Result<Vec<Row>, Box<dyn std::error::Error>> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for symbol in symbols {
        if !seen.insert(symbol.code().to_owned()) {
            continue;
        }
        let quotes = cache.load_quotes(symbol)?;
        let Some(latest) = quotes.last() else {
            continue;
        };
        let (bars, extras) = bars_and_extras(&quotes);
        let Some(t) = bars.len().checked_sub(1) else {
            continue;
        };
        let index = context.index_for(&latest.market_category);
        let Some(raw) = features_at(symbol.code(), &bars, &extras, t, index, context) else {
            continue;
        };
        let mut scaled = raw.clone();
        model.scaler.transform_row(&mut scaled);
        let forecast = model.net.forecast(&scaled);
        rows.push(Row {
            symbol: symbol.code().to_owned(),
            as_of: bars[t].date,
            // Forecast prices are quoted on the unadjusted close the market shows today.
            close: latest.close,
            raw,
            scaled,
            forecast,
            trading_value: extras.get(t).map_or(0.0, |e| e.trading_value),
        });
    }

    Ok(rows)
}
