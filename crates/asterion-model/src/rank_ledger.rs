//! Paper-only forward test of the cross-sectional ranking arms from the 2026-11-13 CV.
//! Models are fitted once and frozen by fingerprint; each session's top-decile picks
//! are written once, and scoring later reads realised prices.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use asterion_core::{Market, Symbol};
use asterion_data::RawCache;
use asterion_data::cache::sha256_hex;
use chrono::{FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::confidence::{DEFAULT_COST, LIQUID_TRADING_VALUE};
use crate::context::{DailySeries, MarketContext};
use crate::features::{bars_and_extras, features_at};
use crate::metrics::HORIZONS;
use crate::research::{
    BUFFER_BAND, CV_HORIZONS, Candidate, PRIMARY, Panel, Scorer, TOP_FRACTION, fit, mean_t,
    rank_cross_section,
};

pub const DEFAULT_DIR: &str = "data/ledger/rank4_2026-11-13";
/// Forward-only paper ledger of the 20% holding buffer (README pre-registration 2026-11-22).
pub const BUFFER_DIR: &str = "data/ledger/buffer20_2026-11-22";
const BUFFER_PROTOCOL: &str = "README.md#buffer-forward-ledger-2026-11-22";
// Pre-registered judgment: increment > 0 and t >= 2.07 (one-sided 2.5%, 23 df) at 24
// effective periods.
const BUFFER_MIN_EFFECTIVE_PERIODS: f64 = 24.0;
const BUFFER_MIN_T: f64 = 2.07;
// Longest gap between the recorded session and the KST record day (covers long holidays).
const MAX_STALE_DAYS: i64 = 7;

fn is_stale(as_of: NaiveDate, today: NaiveDate) -> bool {
    (today - as_of).num_days() > MAX_STALE_DAYS
}
const HYPOTHESIS_FILE: &str = "hypothesis.json";
const MODELS_FILE: &str = "models.json";
const SCORE_FILE: &str = "score.json";
// Fewest liquid names sharing the latest session for a record to be written.
const MIN_LIVE: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrozenModels {
    pub fitted_at: String,
    pub train_rows: usize,
    pub train_last_date: Option<NaiveDate>,
    pub features: Vec<String>,
    // (arm id, horizon in sessions, scorer)
    pub scorers: Vec<(String, usize, Scorer)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankHypothesis {
    pub registered_on: NaiveDate,
    pub protocol: String,
    pub models_fingerprint: String,
    pub arms: Vec<String>,
    pub control_arm: String,
    pub primary_arm: String,
    pub horizons: Vec<usize>,
    pub primary_horizon: usize,
    pub top_fraction: f64,
    pub min_trading_value: f64,
    pub cost: f64,
    pub entry_rule: String,
    pub exit_rule: String,
    // Pass: primary arm minus control, primary horizon, at least this many effective
    // periods (matured daily entries / horizon), mean > 0 and t at least this.
    pub min_effective_periods: f64,
    pub min_t_stat: f64,
    pub paper_only: bool,
    // Arm whose holdings continue the chain of the entry one horizon earlier, and its
    // rank band; absent in ledgers without a buffered arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_arm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_band: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmPick {
    pub arm: String,
    pub horizon: usize,
    pub top: Vec<String>,
    // Buffered arms only: the band used and the entry whose holdings were continued
    // (`None` when the chain restarted from the plain top decile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_from: Option<NaiveDate>,
}

/// Buffered holdings: previous holdings ranked within the first `width` names of
/// `order` (best first) are kept, up to `take`, and free slots take the best names not
/// held. Returned sorted by code.
pub fn buffer_pick(
    order: &[&str],
    prev: &BTreeSet<&str>,
    take: usize,
    width: usize,
) -> Vec<String> {
    let mut chosen = vec![false; order.len()];
    let mut count = 0;
    for (c, name) in chosen.iter_mut().zip(order).take(width) {
        if count < take && prev.contains(name) {
            *c = true;
            count += 1;
        }
    }
    for c in chosen.iter_mut() {
        if count >= take {
            break;
        }
        if !*c {
            *c = true;
            count += 1;
        }
    }
    let mut out: Vec<String> = order
        .iter()
        .zip(&chosen)
        .filter(|(_, c)| **c)
        .map(|(n, _)| (*n).to_owned())
        .collect();
    out.sort();
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankEntry {
    pub as_of: NaiveDate,
    pub recorded_at: String,
    pub universe: Vec<String>,
    pub picks: Vec<ArmPick>,
}

fn fingerprint(models: &FrozenModels) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(&serde_json::to_vec(&models.scorers)?))
}

fn kst_date(rfc3339: &str) -> Option<NaiveDate> {
    let t = chrono::DateTime::parse_from_rfc3339(rfc3339).ok()?;
    Some(
        t.with_timezone(&FixedOffset::east_opt(9 * 3600)?)
            .date_naive(),
    )
}

/// Fits every arm at every CV horizon on the whole panel and fixes the hypothesis.
/// Refuses to run again once a hypothesis exists, so the frozen models cannot change.
pub fn fit_and_register(panel: &Panel, dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let hyp_path = dir.join(HYPOTHESIS_FILE);
    if hyp_path.exists() {
        return Err(format!(
            "hypothesis file exists at {}: cannot refit frozen ledger; start a new directory",
            hyp_path.display()
        )
        .into());
    }
    fs::create_dir_all(dir)?;
    let all: Vec<usize> = (0..panel.rows.len()).collect();
    let mut scorers = Vec::new();
    for (k, &h) in CV_HORIZONS.iter().enumerate() {
        for c in Candidate::BASE {
            scorers.push((c.id().to_owned(), HORIZONS[h], fit(c, panel, &all, k)));
        }
    }
    let models = FrozenModels {
        fitted_at: Utc::now().to_rfc3339(),
        train_rows: panel.rows.len(),
        train_last_date: panel.rows.last().map(|r| r.date),
        features: panel.names.iter().map(|n| (*n).to_owned()).collect(),
        scorers,
    };
    let fp = fingerprint(&models)?;
    fs::write(dir.join(MODELS_FILE), serde_json::to_vec(&models)?)?;
    let hypothesis = RankHypothesis {
        registered_on: Utc::now().date_naive(),
        protocol: "data/ledger/cv_2026-11-13/protocol.json".to_owned(),
        models_fingerprint: fp.clone(),
        arms: Candidate::BASE.iter().map(|c| c.id().to_owned()).collect(),
        control_arm: Candidate::ControlLowvol.id().to_owned(),
        primary_arm: Candidate::Gbdt.id().to_owned(),
        horizons: CV_HORIZONS.iter().map(|&h| HORIZONS[h]).collect(),
        primary_horizon: HORIZONS[CV_HORIZONS[PRIMARY]],
        top_fraction: TOP_FRACTION,
        min_trading_value: LIQUID_TRADING_VALUE,
        cost: DEFAULT_COST,
        entry_rule: crate::ledger::ENTRY_RULE.to_owned(),
        exit_rule: crate::ledger::EXIT_RULE.to_owned(),
        min_effective_periods: 12.0,
        min_t_stat: 2.0,
        paper_only: true,
        buffer_arm: None,
        buffer_band: None,
    };
    fs::write(&hyp_path, serde_json::to_vec_pretty(&hypothesis)?)?;
    Ok(fp)
}

/// Fits one `h20` gbdt on the whole panel and registers the buffer ledger: the control
/// `gbdt` (plain top decile) and the primary `gbdt_buffer` share that scorer. Refuses to
/// run again once a hypothesis exists.
pub fn fit_and_register_buffer(
    panel: &Panel,
    dir: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let hyp_path = dir.join(HYPOTHESIS_FILE);
    if hyp_path.exists() {
        return Err(format!(
            "hypothesis file exists at {}: cannot refit frozen ledger; start a new directory",
            hyp_path.display()
        )
        .into());
    }
    fs::create_dir_all(dir)?;
    let all: Vec<usize> = (0..panel.rows.len()).collect();
    let horizon = HORIZONS[CV_HORIZONS[PRIMARY]];
    let scorer = fit(Candidate::Gbdt, panel, &all, PRIMARY);
    let control = Candidate::Gbdt.id().to_owned();
    let buffered = Candidate::GbdtBuffer.id().to_owned();
    let models = FrozenModels {
        fitted_at: Utc::now().to_rfc3339(),
        train_rows: panel.rows.len(),
        train_last_date: panel.rows.last().map(|r| r.date),
        features: panel.names.iter().map(|n| (*n).to_owned()).collect(),
        scorers: vec![
            (control.clone(), horizon, scorer.clone()),
            (buffered.clone(), horizon, scorer),
        ],
    };
    let fp = fingerprint(&models)?;
    fs::write(dir.join(MODELS_FILE), serde_json::to_vec(&models)?)?;
    let hypothesis = RankHypothesis {
        registered_on: Utc::now().date_naive(),
        protocol: BUFFER_PROTOCOL.to_owned(),
        models_fingerprint: fp.clone(),
        arms: vec![control.clone(), buffered.clone()],
        control_arm: control,
        primary_arm: buffered.clone(),
        horizons: vec![horizon],
        primary_horizon: horizon,
        top_fraction: TOP_FRACTION,
        min_trading_value: LIQUID_TRADING_VALUE,
        cost: DEFAULT_COST,
        entry_rule: crate::ledger::ENTRY_RULE.to_owned(),
        exit_rule: crate::ledger::EXIT_RULE.to_owned(),
        min_effective_periods: BUFFER_MIN_EFFECTIVE_PERIODS,
        min_t_stat: BUFFER_MIN_T,
        paper_only: true,
        buffer_arm: Some(buffered),
        buffer_band: Some(BUFFER_BAND),
    };
    fs::write(&hyp_path, serde_json::to_vec_pretty(&hypothesis)?)?;
    Ok(fp)
}

fn load_frozen(dir: &Path) -> Result<(RankHypothesis, FrozenModels), Box<dyn std::error::Error>> {
    let hypothesis: RankHypothesis = serde_json::from_slice(&fs::read(dir.join(HYPOTHESIS_FILE))?)?;
    let models: FrozenModels = serde_json::from_slice(&fs::read(dir.join(MODELS_FILE))?)?;
    if fingerprint(&models)? != hypothesis.models_fingerprint {
        return Err("models.json SHA-256 does not match fingerprint in hypothesis.json".into());
    }
    Ok((hypothesis, models))
}

#[derive(Debug, Clone)]
pub struct RecordOutcome {
    pub path: PathBuf,
    pub as_of: NaiveDate,
    pub created: bool,
    pub universe: usize,
}

/// Writes the latest common session's picks once; later calls for that session are no-ops.
pub fn record(
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
    dir: &Path,
) -> Result<RecordOutcome, Box<dyn std::error::Error>> {
    let (hypothesis, models) = load_frozen(dir)?;
    let mut rows: Vec<(String, NaiveDate, Vec<f64>, f64)> = Vec::new();
    let mut seen = BTreeSet::new();
    // Trading calendar from every cached bar, to find the entry one horizon earlier.
    let mut calendar: BTreeSet<NaiveDate> = BTreeSet::new();
    for symbol in symbols {
        if !seen.insert(symbol.code().to_owned()) {
            continue;
        }
        let quotes = cache.load_quotes(symbol)?;
        let Some(latest) = quotes.last() else {
            continue;
        };
        let (bars, extras) = bars_and_extras(&quotes);
        calendar.extend(bars.iter().map(|b| b.date));
        let Some(t) = bars.len().checked_sub(1) else {
            continue;
        };
        let index = context.index_for(&latest.market_category);
        let Some(raw) = features_at(symbol.code(), &bars, &extras, t, index, context) else {
            continue;
        };
        let tv = extras.get(t).map_or(0.0, |e| e.trading_value);
        rows.push((symbol.code().to_owned(), bars[t].date, raw, tv));
    }
    let mut counts: HashMap<NaiveDate, usize> = HashMap::new();
    for r in &rows {
        *counts.entry(r.1).or_default() += 1;
    }
    let as_of = counts
        .iter()
        .max_by_key(|(d, n)| (**n, **d))
        .map(|(d, _)| *d)
        .ok_or("no symbol has sufficient price history to compute features")?;
    let live: Vec<&(String, NaiveDate, Vec<f64>, f64)> = rows
        .iter()
        .filter(|r| r.1 == as_of && r.3 >= hypothesis.min_trading_value)
        .collect();
    if live.len() < MIN_LIVE {
        return Err(format!(
            "too few liquid names ({}) share latest session {as_of} to rank",
            live.len()
        )
        .into());
    }
    // Buffer ledgers only record fresh sessions, so a stale cache cannot create an entry.
    if hypothesis.buffer_arm.is_some() {
        let kst = FixedOffset::east_opt(9 * 3600).ok_or("invalid KST offset")?;
        let today = Utc::now().with_timezone(&kst).date_naive();
        if is_stale(as_of, today) {
            return Err(format!(
                "latest common session {as_of} is more than {MAX_STALE_DAYS} days before {today}; refresh prices before recording"
            )
            .into());
        }
    }
    let path = dir.join(format!("{as_of}.json"));
    if path.exists() {
        return Ok(RecordOutcome {
            path,
            as_of,
            created: false,
            universe: live.len(),
        });
    }
    let feats: Vec<&[f64]> = live.iter().map(|r| r.2.as_slice()).collect();
    let x = rank_cross_section(&feats);
    let take = ((live.len() as f64) * hypothesis.top_fraction).ceil() as usize;
    let calendar: Vec<NaiveDate> = calendar.into_iter().collect();
    let mut picks = Vec::new();
    for (arm, horizon, scorer) in &models.scorers {
        let mut scored: Vec<(f64, &str)> = x
            .iter()
            .zip(&live)
            .map(|(xi, r)| (scorer.score(xi), r.0.as_str()))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let order: Vec<&str> = scored.iter().map(|(_, s)| *s).collect();
        let band = hypothesis
            .buffer_band
            .filter(|_| hypothesis.buffer_arm.as_deref() == Some(arm.as_str()));
        let pick = match band {
            Some(band) => {
                let width = (((live.len() as f64) * band).ceil() as usize).min(live.len());
                let (prev, chain_from) = chain_holdings(dir, &calendar, as_of, arm, *horizon)?;
                let prev_set: BTreeSet<&str> = prev.iter().map(String::as_str).collect();
                ArmPick {
                    arm: arm.clone(),
                    horizon: *horizon,
                    top: buffer_pick(&order, &prev_set, take, width),
                    band: Some(band),
                    chain_from,
                }
            }
            None => {
                let mut top: Vec<String> =
                    order.iter().take(take).map(|s| (*s).to_owned()).collect();
                top.sort();
                ArmPick {
                    arm: arm.clone(),
                    horizon: *horizon,
                    top,
                    band: None,
                    chain_from: None,
                }
            }
        };
        picks.push(pick);
    }
    let mut universe: Vec<String> = live.iter().map(|r| r.0.clone()).collect();
    universe.sort();
    let entry = RankEntry {
        as_of,
        recorded_at: Utc::now().to_rfc3339(),
        universe,
        picks,
    };
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(&entry)?)?;
    fs::rename(&tmp, &path)?;
    Ok(RecordOutcome {
        path,
        as_of,
        created: true,
        universe: live.len(),
    })
}

// Holdings of `arm` in the entry exactly `horizon` sessions before `as_of`, with that
// entry's date; empty (a chain restart) when the entry or the arm is missing.
fn chain_holdings(
    dir: &Path,
    calendar: &[NaiveDate],
    as_of: NaiveDate,
    arm: &str,
    horizon: usize,
) -> Result<(Vec<String>, Option<NaiveDate>), Box<dyn std::error::Error>> {
    let Ok(i) = calendar.binary_search(&as_of) else {
        return Ok((Vec::new(), None));
    };
    let Some(j) = i.checked_sub(horizon) else {
        return Ok((Vec::new(), None));
    };
    let date = calendar[j];
    let path = dir.join(format!("{date}.json"));
    if !path.exists() {
        return Ok((Vec::new(), None));
    }
    let entry: RankEntry = serde_json::from_slice(&fs::read(&path)?)?;
    Ok(entry
        .picks
        .into_iter()
        .find(|p| p.arm == arm && p.horizon == horizon)
        .map_or((Vec::new(), None), |p| (p.top, Some(date))))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmScore {
    pub arm: String,
    pub horizon: usize,
    pub matured: usize,
    pub effective_periods: f64,
    pub top_net_mean: f64,
    pub universe_net_mean: f64,
    pub kospi_mean: f64,
    pub excess_mean: f64,
    pub excess_t: f64,
    pub increment_vs_control_mean: f64,
    pub increment_vs_control_t: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankScore {
    pub hypothesis: RankHypothesis,
    pub scored_at: String,
    pub entries: usize,
    pub latest_price_date: Option<NaiveDate>,
    pub arms: Vec<ArmScore>,
    // None until enough effective periods have matured.
    pub passed: Option<bool>,
}

fn ew_turnover(prev: Option<&[String]>, next: &[String]) -> f64 {
    let Some(prev) = prev else { return 1.0 };
    let set: BTreeSet<&str> = prev.iter().map(String::as_str).collect();
    let stay = next.iter().filter(|s| set.contains(s.as_str())).count();
    let (wp, wn) = (
        1.0 / prev.len().max(1) as f64,
        1.0 / next.len().max(1) as f64,
    );
    // Buys: new names at full weight plus any weight increase on kept names.
    (next.len() - stay) as f64 * wn + stay as f64 * (wn - wp).max(0.0)
}

/// Scores every matured entry; each entry is a holding period, overlapping entries are
/// counted as entries / horizon independent periods.
pub fn score(
    cache: &RawCache,
    kospi: &DailySeries,
    dir: &Path,
) -> Result<RankScore, Box<dyn std::error::Error>> {
    let (hypothesis, _) = load_frozen(dir)?;
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.ends_with(".json")
            && ![HYPOTHESIS_FILE, MODELS_FILE, SCORE_FILE].contains(&name.as_str())
        {
            entries.push(serde_json::from_slice::<RankEntry>(&fs::read(&path)?)?);
        }
    }
    entries.sort_by_key(|e| e.as_of);
    // A buffered arm written by a binary without buffer support would hold plain picks.
    if let Some(arm) = &hypothesis.buffer_arm {
        for e in &entries {
            if e.picks.iter().any(|p| &p.arm == arm && p.band.is_none()) {
                return Err(format!(
                    "ledger entry {} arm {arm} is missing band field (written by an older binary without buffer support)",
                    e.as_of
                )
                .into());
            }
        }
    }
    let codes: BTreeSet<&str> = entries
        .iter()
        .flat_map(|e| e.universe.iter().map(String::as_str))
        .collect();
    let mut closes: HashMap<&str, Vec<(NaiveDate, f64)>> = HashMap::new();
    let mut calendar: BTreeSet<NaiveDate> = BTreeSet::new();
    for code in codes {
        let Ok(symbol) = Symbol::new(Market::Kr, code) else {
            continue;
        };
        let (bars, _) = bars_and_extras(&cache.load_quotes(&symbol)?);
        let series: Vec<(NaiveDate, f64)> = bars.iter().map(|b| (b.date, b.close)).collect();
        calendar.extend(series.iter().map(|(d, _)| *d));
        closes.insert(code, series);
    }
    let calendar: Vec<NaiveDate> = calendar.into_iter().collect();
    let close_on = |code: &str, d: NaiveDate| -> Option<f64> {
        let s = closes.get(code)?;
        s.binary_search_by_key(&d, |(x, _)| *x).ok().map(|i| s[i].1)
    };
    let close_by = |code: &str, d: NaiveDate| -> Option<f64> {
        let s = closes.get(code)?;
        let i = s.partition_point(|(x, _)| *x <= d);
        (i > 0).then(|| s[i - 1].1)
    };
    let ew = |names: &[String], a: NaiveDate, b: NaiveDate| -> f64 {
        let r: Vec<f64> = names
            .iter()
            .filter_map(|c| Some(close_by(c, b)? / close_on(c, a)? - 1.0))
            .collect();
        r.iter().sum::<f64>() / r.len().max(1) as f64
    };
    let cost = hypothesis.cost;
    let mut arms = Vec::new();
    for &horizon in &hypothesis.horizons {
        // Per arm: (p0, excess net) for matured entries, plus the summary.
        let mut excess_by_arm: HashMap<String, HashMap<usize, f64>> = HashMap::new();
        for arm in &hypothesis.arms {
            let mut prev: HashMap<usize, (Vec<String>, Vec<String>)> = HashMap::new();
            let (mut tops, mut unis, mut idx, mut exc) =
                (Vec::new(), Vec::new(), Vec::new(), HashMap::new());
            for e in &entries {
                let Some(pick) = e
                    .picks
                    .iter()
                    .find(|p| &p.arm == arm && p.horizon == horizon)
                else {
                    continue;
                };
                let Some(rec) = kst_date(&e.recorded_at) else {
                    continue;
                };
                let p0 = calendar.partition_point(|d| *d <= rec.max(e.as_of));
                let (Some(&a), Some(&b)) = (calendar.get(p0), calendar.get(p0 + horizon)) else {
                    continue;
                };
                // Turnover against the entry exactly one horizon earlier, else a full buy.
                let before = p0.checked_sub(horizon).and_then(|q| prev.get(&q));
                let tt = ew_turnover(before.map(|v| v.0.as_slice()), &pick.top);
                let tu = ew_turnover(before.map(|v| v.1.as_slice()), &e.universe);
                let top = ew(&pick.top, a, b) - tt * cost;
                let uni = ew(&e.universe, a, b) - tu * cost;
                let k = match (kospi.index_at(a, false, 0), kospi.index_at(b, false, 0)) {
                    (Some(i), Some(j)) => kospi.value(j) / kospi.value(i) - 1.0,
                    _ => f64::NAN,
                };
                prev.insert(p0, (pick.top.clone(), e.universe.clone()));
                tops.push(top);
                unis.push(uni);
                if k.is_finite() {
                    idx.push(k);
                }
                exc.insert(p0, top - uni);
            }
            let ex: Vec<f64> = exc.values().copied().collect();
            let (ex_m, ex_t) = mean_t(&ex, horizon);
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
            arms.push(ArmScore {
                arm: arm.clone(),
                horizon,
                matured: tops.len(),
                effective_periods: tops.len() as f64 / horizon as f64,
                top_net_mean: mean(&tops),
                universe_net_mean: mean(&unis),
                kospi_mean: mean(&idx),
                excess_mean: ex_m,
                excess_t: ex_t,
                increment_vs_control_mean: 0.0,
                increment_vs_control_t: 0.0,
            });
            excess_by_arm.insert(arm.clone(), exc);
        }
        if let Some(ctrl) = excess_by_arm.get(&hypothesis.control_arm) {
            for a in arms.iter_mut().filter(|a| a.horizon == horizon) {
                let Some(mine) = excess_by_arm.get(&a.arm) else {
                    continue;
                };
                let inc: Vec<f64> = mine
                    .iter()
                    .filter_map(|(p, v)| ctrl.get(p).map(|c| v - c))
                    .collect();
                let (m, t) = mean_t(&inc, horizon);
                a.increment_vs_control_mean = m;
                a.increment_vs_control_t = t;
            }
        }
    }
    let primary = arms
        .iter()
        .find(|a| a.arm == hypothesis.primary_arm && a.horizon == hypothesis.primary_horizon);
    let passed = primary.and_then(|a| {
        (a.effective_periods >= hypothesis.min_effective_periods).then_some(
            a.increment_vs_control_mean > 0.0 && a.increment_vs_control_t >= hypothesis.min_t_stat,
        )
    });
    let score = RankScore {
        hypothesis,
        scored_at: Utc::now().to_rfc3339(),
        entries: entries.len(),
        latest_price_date: calendar.last().copied(),
        arms,
        passed,
    };
    fs::write(dir.join(SCORE_FILE), serde_json::to_vec_pretty(&score)?)?;
    Ok(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_pick_keeps_holdings_inside_the_band_and_fills_by_rank() {
        let order = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let none = BTreeSet::new();
        assert_eq!(buffer_pick(&order, &none, 2, 4), vec!["a", "b"]);
        // `d` is inside the band of 4 and kept; `g` is outside and dropped.
        let prev: BTreeSet<&str> = ["d", "g"].into_iter().collect();
        assert_eq!(buffer_pick(&order, &prev, 2, 4), vec!["a", "d"]);
        // More kept names than slots: the best-ranked ones stay.
        let prev: BTreeSet<&str> = ["b", "c", "d"].into_iter().collect();
        assert_eq!(buffer_pick(&order, &prev, 2, 4), vec!["b", "c"]);
        // A band as wide as the holdings reproduces the plain top names.
        let prev: BTreeSet<&str> = ["c"].into_iter().collect();
        assert_eq!(buffer_pick(&order, &prev, 2, 2), vec!["a", "b"]);
    }

    #[test]
    fn records_older_than_a_week_are_stale() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).expect("date");
        assert!(!is_stale(d(11, 20), d(11, 22)));
        assert!(!is_stale(d(11, 15), d(11, 22)));
        assert!(is_stale(d(11, 13), d(11, 22)));
    }

    #[test]
    fn old_hypotheses_and_entries_still_parse() {
        let hyp = r#"{"registered_on":"2026-11-13","protocol":"p","models_fingerprint":"f","arms":["gbdt"],"control_arm":"control_lowvol","primary_arm":"gbdt","horizons":[20],"primary_horizon":20,"top_fraction":0.1,"min_trading_value":1e9,"cost":0.0025,"entry_rule":"e","exit_rule":"x","min_effective_periods":12.0,"min_t_stat":2.0,"paper_only":true}"#;
        let h: RankHypothesis = serde_json::from_str(hyp).expect("parses");
        assert_eq!(h.buffer_arm, None);
        let pick = r#"{"arm":"gbdt","horizon":20,"top":["005930"]}"#;
        let p: ArmPick = serde_json::from_str(pick).expect("parses");
        assert_eq!(p.band, None);
        // Plain picks serialize exactly as before.
        assert_eq!(serde_json::to_string(&p).expect("serializes"), pick);
    }

    #[test]
    fn turnover_is_full_buy_without_history() {
        let next = vec!["a".to_owned(), "b".to_owned()];
        assert!((ew_turnover(None, &next) - 1.0).abs() < 1e-12);
        let prev = vec!["a".to_owned(), "c".to_owned()];
        assert!((ew_turnover(Some(&prev), &next) - 0.5).abs() < 1e-12);
    }
}
