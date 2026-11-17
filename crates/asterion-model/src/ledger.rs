//! Forward test of the avoid-the-bottom-decile rule. Each session the frozen model's
//! picks are written once and never rewritten; scoring later reads realised prices.
//! The rule and pass criteria are fixed in `hypothesis.json` when the ledger starts.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use asterion_core::{Market, Symbol};
use asterion_data::RawCache;
use asterion_data::cache::sha256_hex;
use chrono::{FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::confidence::{DEFAULT_COST, LIQUID_TRADING_VALUE};
use crate::context::MarketContext;
use crate::features::bars_and_extras;
use crate::metrics::HORIZONS;
use crate::predict::{MIN_CROSS_SECTION, collect_rows};
use crate::strategy::{
    AvoidSummary, CONTROL_FEATURE, EXCLUDE_FRACTION, Period, bottom, control_feature_index,
    run_chain, summarize,
};
use crate::train::SavedModel;

pub const DEFAULT_ROOT: &str = "data/ledger";
const HYPOTHESIS_FILE: &str = "hypothesis.json";
const SCORE_FILE: &str = "score.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub registered_on: NaiveDate,
    pub model: String,
    pub model_best_epoch: usize,
    pub exclude_fraction: f64,
    pub min_trading_value: f64,
    pub horizons: Vec<usize>,
    pub primary_horizon: usize,
    pub cost: f64,
    // Pass: at least this many non-overlapping periods, positive mean excess, t above this.
    pub min_periods: usize,
    pub min_t_stat: f64,
    // SHA-256 of the serialized network weights; only that exact model may append.
    #[serde(default)]
    pub model_fingerprint: String,
    // Holding starts at the close of the first session after the day the picks were
    // recorded (KST) and ends `horizon` sessions later; a name without a close on the
    // exit date (halted or delisted) exits at its last close before it.
    #[serde(default)]
    pub entry_rule: String,
    #[serde(default)]
    pub exit_rule: String,
    // Control arm recorded alongside: the same rule excluding the highest values of this feature.
    #[serde(default)]
    pub control_feature: String,
}

pub const ENTRY_RULE: &str = "close_of_first_session_after_record_day_kst";
pub const EXIT_RULE: &str = "close_after_horizon_sessions_else_last_close";

pub fn model_fingerprint(model: &SavedModel) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(&serde_json::to_vec(&model.net)?))
}

fn kst_date(rfc3339: &str) -> Option<NaiveDate> {
    let t = chrono::DateTime::parse_from_rfc3339(rfc3339).ok()?;
    Some(
        t.with_timezone(&FixedOffset::east_opt(9 * 3600)?)
            .date_naive(),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonPick {
    pub horizon: usize,
    pub universe: Vec<String>,
    pub excluded: Vec<String>,
    #[serde(default)]
    pub control_excluded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub as_of: NaiveDate,
    pub recorded_at: String,
    pub model: String,
    pub picks: Vec<HorizonPick>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonScore {
    pub horizon: usize,
    pub primary: bool,
    pub entries: usize,
    pub matured: usize,
    pub summary: Option<AvoidSummary>,
    #[serde(default)]
    pub control: Option<AvoidSummary>,
    // `None` until `min_periods` non-overlapping periods have matured.
    pub passed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerScore {
    pub hypothesis: Hypothesis,
    pub scored_at: String,
    pub first_entry: Option<NaiveDate>,
    pub last_entry: Option<NaiveDate>,
    pub latest_price_date: Option<NaiveDate>,
    pub horizons: Vec<HorizonScore>,
}

/// Directory holding one model's ledger.
pub fn ledger_dir(root: &Path, model_path: &Path) -> PathBuf {
    let stem = model_path
        .file_stem()
        .map_or_else(|| "model".into(), |s| s.to_string_lossy().into_owned());
    root.join(stem)
}

fn model_name(model_path: &Path) -> String {
    model_path.display().to_string().replace('\\', "/")
}

pub fn load_hypothesis(dir: &Path) -> Result<Hypothesis, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(
        dir.join(HYPOTHESIS_FILE),
    )?)?)
}

#[derive(Debug, Clone)]
pub struct RecordOutcome {
    pub path: PathBuf,
    pub as_of: NaiveDate,
    pub created: bool,
    pub universe: usize,
}

/// Writes today's picks for the latest common session unless that session is already
/// recorded. The first call also fixes the hypothesis.
pub fn record(
    model: &SavedModel,
    model_path: &Path,
    cache: &RawCache,
    symbols: &[Symbol],
    context: &MarketContext,
    root: &Path,
) -> Result<RecordOutcome, Box<dyn std::error::Error>> {
    let dir = ledger_dir(root, model_path);
    fs::create_dir_all(&dir)?;
    let hyp_path = dir.join(HYPOTHESIS_FILE);
    let fingerprint = model_fingerprint(model)?;
    if !hyp_path.exists() {
        let h = Hypothesis {
            registered_on: Utc::now().date_naive(),
            model: model_name(model_path),
            model_best_epoch: model.best_epoch,
            exclude_fraction: EXCLUDE_FRACTION,
            min_trading_value: LIQUID_TRADING_VALUE,
            horizons: vec![5, 10, 20],
            primary_horizon: 20,
            cost: DEFAULT_COST,
            min_periods: 12,
            min_t_stat: 2.0,
            model_fingerprint: fingerprint.clone(),
            entry_rule: ENTRY_RULE.to_owned(),
            exit_rule: EXIT_RULE.to_owned(),
            control_feature: CONTROL_FEATURE.to_owned(),
        };
        fs::write(&hyp_path, serde_json::to_vec_pretty(&h)?)?;
    }
    let hypothesis = load_hypothesis(&dir)?;
    if hypothesis.model_best_epoch != model.best_epoch
        || hypothesis.model_fingerprint != fingerprint
    {
        return Err(format!(
            "ledger directory {} was started for a different model than {}; cannot append, use a new model file name",
            dir.display(),
            model_path.display()
        )
        .into());
    }

    let rows = collect_rows(model, cache, symbols, context)?;
    let mut counts: HashMap<NaiveDate, usize> = HashMap::new();
    for r in &rows {
        *counts.entry(r.as_of).or_default() += 1;
    }
    let (as_of, n) = counts
        .iter()
        .max_by_key(|(d, n)| (**n, **d))
        .map(|(d, n)| (*d, *n))
        .ok_or("no symbol has enough price history to forecast")?;
    if n < MIN_CROSS_SECTION {
        return Err(
            format!("only {n} symbols share the latest session {as_of}, too few to rank").into(),
        );
    }
    let path = dir.join(format!("{as_of}.json"));
    let live: Vec<_> = rows
        .iter()
        .filter(|r| r.as_of == as_of && r.trading_value >= hypothesis.min_trading_value)
        .collect();
    if path.exists() {
        return Ok(RecordOutcome {
            path,
            as_of,
            created: false,
            universe: live.len(),
        });
    }
    let control_idx = control_feature_index();
    let picks = hypothesis
        .horizons
        .iter()
        .filter_map(|&horizon| {
            let h = HORIZONS.iter().position(|&x| x == horizon)?;
            let scored: Vec<(String, f64)> = live
                .iter()
                .map(|r| (r.symbol.clone(), r.forecast.returns[h]))
                .collect();
            let mut excluded: Vec<String> = bottom(&scored).into_iter().collect();
            excluded.sort();
            let control: Vec<(String, f64)> = live
                .iter()
                .map(|r| (r.symbol.clone(), -r.raw[control_idx]))
                .collect();
            let mut control_excluded: Vec<String> = bottom(&control).into_iter().collect();
            control_excluded.sort();
            let mut universe: Vec<String> = scored.into_iter().map(|(s, _)| s).collect();
            universe.sort();
            Some(HorizonPick {
                horizon,
                universe,
                excluded,
                control_excluded,
            })
        })
        .collect();
    let entry = LedgerEntry {
        as_of,
        recorded_at: Utc::now().to_rfc3339(),
        model: hypothesis.model.clone(),
        picks,
    };
    // Write-once: a temp file renamed into place so a crash never leaves half an entry.
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

fn load_entries(dir: &Path) -> Result<Vec<LedgerEntry>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let Some(name) = name else { continue };
        if !name.ends_with(".json") || name == HYPOTHESIS_FILE || name == SCORE_FILE {
            continue;
        }
        entries.push(serde_json::from_slice::<LedgerEntry>(&fs::read(&path)?)?);
    }
    entries.sort_by_key(|e| e.as_of);
    Ok(entries)
}

/// Scores every matured entry against the cached prices and writes `score.json`.
pub fn score(
    model_path: &Path,
    cache: &RawCache,
    root: &Path,
) -> Result<LedgerScore, Box<dyn std::error::Error>> {
    let dir = ledger_dir(root, model_path);
    let hypothesis = load_hypothesis(&dir)?;
    let entries = load_entries(&dir)?;
    let codes: BTreeSet<&str> = entries
        .iter()
        .flat_map(|e| e.picks.iter())
        .flat_map(|p| p.universe.iter().map(String::as_str))
        .collect();
    // Adjusted closes by date for every name that was ever in a universe.
    let mut closes: HashMap<String, Vec<(NaiveDate, f64)>> = HashMap::new();
    let mut calendar: BTreeSet<NaiveDate> = BTreeSet::new();
    for code in codes {
        let Ok(symbol) = Symbol::new(Market::Kr, code) else {
            continue;
        };
        let quotes = cache.load_quotes(&symbol)?;
        let (bars, _) = bars_and_extras(&quotes);
        let series: Vec<(NaiveDate, f64)> = bars.iter().map(|b| (b.date, b.close)).collect();
        calendar.extend(series.iter().map(|(d, _)| *d));
        closes.insert(code.to_owned(), series);
    }
    let calendar: Vec<NaiveDate> = calendar.into_iter().collect();
    let close_on_or_before = |code: &str, date: NaiveDate| -> Option<f64> {
        let s = closes.get(code)?;
        let i = s.partition_point(|(d, _)| *d <= date);
        (i > 0).then(|| s[i - 1].1)
    };
    let close_on = |code: &str, date: NaiveDate| -> Option<f64> {
        let s = closes.get(code)?;
        s.binary_search_by_key(&date, |(d, _)| *d)
            .ok()
            .map(|i| s[i].1)
    };

    let horizons = hypothesis
        .horizons
        .iter()
        .map(|&horizon| {
            let mut chain = Vec::new();
            let mut control_chain = Vec::new();
            let mut last: Option<usize> = None;
            let mut matured = 0;
            let with_pick: Vec<(&LedgerEntry, &HorizonPick)> = entries
                .iter()
                .filter_map(|e| {
                    e.picks
                        .iter()
                        .find(|p| p.horizon == horizon)
                        .map(|p| (e, p))
                })
                .collect();
            for (e, pick) in &with_pick {
                let Some(recorded) = kst_date(&e.recorded_at) else {
                    continue;
                };
                // First session after the record day; it must already be in the cache.
                let p0 = calendar.partition_point(|d| *d <= recorded.max(e.as_of));
                let Some(&entry) = calendar.get(p0) else {
                    continue;
                };
                let Some(&target) = calendar.get(p0 + horizon) else {
                    continue;
                };
                matured += 1;
                if last.is_some_and(|l| p0 < l + horizon) {
                    continue;
                }
                last = Some(p0);
                // A name with no close on the target date (halted or delisted) keeps its last one.
                let returns: Vec<(String, f64)> = pick
                    .universe
                    .iter()
                    .filter_map(|code| {
                        let start = close_on(code, entry)?;
                        let end = close_on_or_before(code, target)?;
                        Some((code.clone(), end / start - 1.0))
                    })
                    .collect();
                let excluded: HashSet<String> = pick.excluded.iter().cloned().collect();
                let control: HashSet<String> = pick.control_excluded.iter().cloned().collect();
                control_chain.push(Period {
                    returns: returns.clone(),
                    excluded: control,
                });
                chain.push(Period { returns, excluded });
            }
            let results = run_chain(&chain, hypothesis.cost);
            let summary = (!results.is_empty()).then(|| summarize(&results, horizon, 1));
            let control_results = run_chain(&control_chain, hypothesis.cost);
            let control =
                (!control_results.is_empty()).then(|| summarize(&control_results, horizon, 1));
            let passed = summary.as_ref().and_then(|s| {
                (s.periods >= hypothesis.min_periods)
                    .then_some(s.mean_excess > 0.0 && s.t_stat >= hypothesis.min_t_stat)
            });
            HorizonScore {
                horizon,
                primary: horizon == hypothesis.primary_horizon,
                entries: with_pick.len(),
                matured,
                summary,
                control,
                passed,
            }
        })
        .collect();
    let score = LedgerScore {
        hypothesis,
        scored_at: Utc::now().to_rfc3339(),
        first_entry: entries.first().map(|e| e.as_of),
        last_entry: entries.last().map(|e| e.as_of),
        latest_price_date: calendar.last().copied(),
        horizons,
    };
    fs::write(dir.join(SCORE_FILE), serde_json::to_vec_pretty(&score)?)?;
    Ok(score)
}

pub fn print_score(s: &LedgerScore) {
    println!(
        "ledger {} entries {:?}..{:?}, prices to {:?}, rule: exclude bottom {:.0}% of names trading >= {:.0} KRW, cost {:.2}%, pass at >= {} periods with t >= {:.1}",
        s.hypothesis.model,
        s.first_entry,
        s.last_entry,
        s.latest_price_date,
        s.hypothesis.exclude_fraction * 100.0,
        s.hypothesis.min_trading_value,
        s.hypothesis.cost * 100.0,
        s.hypothesis.min_periods,
        s.hypothesis.min_t_stat
    );
    for h in &s.horizons {
        let verdict = match h.passed {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "pending",
        };
        match &h.summary {
            Some(a) => println!(
                "  t+{:<3}{} entries {} matured {} periods {}: excess {:+.3}% per period (win {:.1}%, t {:+.2}) excluded vs universe {:+.3}% -> {verdict}",
                h.horizon,
                if h.primary { "*" } else { " " },
                h.entries,
                h.matured,
                a.periods,
                a.mean_excess * 100.0,
                a.excess_win_rate * 100.0,
                a.t_stat,
                a.excluded_minus_universe * 100.0
            ),
            None => println!(
                "  t+{:<3}{} entries {} matured 0 -> {verdict}",
                h.horizon,
                if h.primary { "*" } else { " " },
                h.entries
            ),
        }
        if let Some(c) = &h.control {
            println!(
                "        control (exclude most volatile): excess {:+.3}% per period (win {:.1}%, t {:+.2})",
                c.mean_excess * 100.0,
                c.excess_win_rate * 100.0,
                c.t_stat
            );
        }
    }
}
