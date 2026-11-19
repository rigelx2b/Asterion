//! Leakage audits of the primary-horizon walk-forward (h20); never used for selection.
//!
//! - `Base` reproduces the published protocol and keeps every test score.
//! - `Placebo` fits on labels shuffled within each date; scores are judged on true labels.
//! - `Lag` scores each row from the same symbol's features one session earlier.

use std::collections::HashMap;

use asterion_core::Bar;
use asterion_data::krx_delisted::DelistingKind;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::context::MarketContext;
use crate::features::{BarExtra, Sample};
use crate::metrics::HORIZONS;
use crate::neural::default_threads;
use crate::research::{
    CV_HORIZONS, Candidate, Fold, PRIMARY, Panel, PeriodRecord, Summary, evaluate, fit, split,
    summarize,
};
use crate::survivorship::{extract_delisted_samples, final_halt_start};

pub const AUDIT_CANDIDATES: [Candidate; 4] = [
    Candidate::ControlLowvol,
    Candidate::Composite6,
    Candidate::Ridge,
    Candidate::Gbdt,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditMode {
    Base,
    Placebo,
    Lag,
}

impl AuditMode {
    pub fn id(self) -> &'static str {
        match self {
            AuditMode::Base => "base",
            AuditMode::Placebo => "placebo",
            AuditMode::Lag => "lag",
        }
    }
}

// Per-date IC and period records of one candidate.
type CandidateRun = (Vec<(NaiveDate, f64)>, Vec<PeriodRecord>);

/// Scores of one test row under every audit candidate.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub fold: usize,
    pub row: usize,
    pub scores: [f64; AUDIT_CANDIDATES.len()],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub mode: AuditMode,
    pub train_rows: Vec<usize>,
    pub test_rows: Vec<usize>,
    pub summaries: Vec<Summary>,
}

/// For each row, the same symbol's row on the previous panel date, if it has one.
pub fn previous_session(panel: &Panel) -> Vec<Option<usize>> {
    let mut out = vec![None; panel.rows.len()];
    for g in 1..panel.groups.len() {
        let (_, ps, pe) = panel.groups[g - 1];
        let prev: HashMap<&str, usize> = (ps..pe)
            .map(|i| (panel.rows[i].symbol.as_str(), i))
            .collect();
        let (_, s, e) = panel.groups[g];
        for (i, slot) in out.iter_mut().enumerate().take(e).skip(s) {
            *slot = prev.get(panel.rows[i].symbol.as_str()).copied();
        }
    }
    out
}

/// Replaces every row's features with its previous-session row's (ranked that day).
/// Rows without one keep stale values and must be masked out by the caller.
pub fn lag_features(panel: &mut Panel, prev: &[Option<usize>]) {
    // Descending order: a predecessor (lower index) is read before it is overwritten.
    for i in (0..panel.rows.len()).rev() {
        if let Some(j) = prev[i] {
            let x = panel.rows[j].x.clone();
            panel.rows[i].x = x;
        }
    }
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Shuffles the ranked labels among the rows of each date (Fisher-Yates, seeded).
pub fn permute_labels(panel: &mut Panel, seed: u64) {
    let mut state = seed;
    for &(_, s, e) in &panel.groups {
        for i in (s + 1..e).rev() {
            let j = s + (splitmix(&mut state) % (i - s + 1) as u64) as usize;
            let (a, b) = (panel.rows[i].y, panel.rows[j].y);
            panel.rows[i].y = b;
            panel.rows[j].y = a;
        }
    }
}

fn score_rows(panel: &Panel, scorer: &crate::research::Scorer, test: &[usize]) -> Vec<f64> {
    let threads = default_threads();
    let chunk = test.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = test
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|&i| scorer.score(&panel.rows[i].x))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect()
    })
}

/// Row masks of an audit run; `false` drops the row.
#[derive(Debug, Clone, Copy, Default)]
pub struct Masks<'a> {
    // Applied to train and test.
    pub both: Option<&'a [bool]>,
    // Applied to train only.
    pub train: Option<&'a [bool]>,
}

/// Primary-horizon folds under one audit mode, with rows masked by `keep`; with
/// `placebo_seed`, models are fitted on shuffled labels and judged on the true ones.
pub fn run_audit(
    panel: &mut Panel,
    folds: &[Fold],
    embargo: u64,
    mode: AuditMode,
    keep: Masks<'_>,
    placebo_seed: Option<u64>,
    log: &dyn Fn(&str),
) -> (AuditReport, Vec<AuditRow>) {
    let k = PRIMARY;
    let horizon = HORIZONS[CV_HORIZONS[k]];
    let original: Option<Vec<[f32; 3]>> =
        placebo_seed.map(|_| panel.rows.iter().map(|r| r.y).collect());
    let mut rows = Vec::new();
    let mut per_cand: Vec<CandidateRun> = vec![(Vec::new(), Vec::new()); AUDIT_CANDIDATES.len()];
    let mut summaries = Vec::new();
    let mut train_rows = Vec::new();
    let mut test_rows = Vec::new();
    for (fi, &fold) in folds.iter().enumerate() {
        let (mut train, mut test) = split(panel, fold, embargo);
        if let Some(mask) = keep.both {
            train.retain(|&i| mask[i]);
            test.retain(|&i| mask[i]);
        }
        if let Some(mask) = keep.train {
            train.retain(|&i| mask[i]);
        }
        if let Some(seed) = placebo_seed {
            permute_labels(panel, seed.wrapping_add(fi as u64));
        }
        let scorers: Vec<_> = AUDIT_CANDIDATES
            .iter()
            .map(|&c| fit(c, panel, &train, k))
            .collect();
        if let Some(orig) = &original {
            for (r, y) in panel.rows.iter_mut().zip(orig) {
                r.y = *y;
            }
        }
        let scores: Vec<Vec<f64>> = scorers
            .iter()
            .map(|s| score_rows(panel, s, &test))
            .collect();
        let mut fold_runs = Vec::new();
        for (ci, c) in AUDIT_CANDIDATES.iter().enumerate() {
            let (ics, records) = evaluate(panel, &test, &scores[ci], k, false);
            log(&format!(
                "audit={} fold={} candidate={} train={} test={}",
                mode.id(),
                fold.test_from,
                c.id(),
                train.len(),
                test.len()
            ));
            fold_runs.push((ics, records));
        }
        for (ci, c) in AUDIT_CANDIDATES.iter().enumerate() {
            summaries.push(summarize(
                c.id(),
                horizon,
                &fold.test_from.to_string(),
                &fold_runs[ci].0,
                &fold_runs[ci].1,
                &fold_runs[0].1,
            ));
        }
        for (ci, (ics, recs)) in fold_runs.into_iter().enumerate() {
            per_cand[ci].0.extend(ics);
            per_cand[ci].1.extend(recs);
        }
        for (q, &i) in test.iter().enumerate() {
            let mut s = [0.0; AUDIT_CANDIDATES.len()];
            for (ci, v) in s.iter_mut().enumerate() {
                *v = scores[ci][q];
            }
            rows.push(AuditRow {
                fold: fi,
                row: i,
                scores: s,
            });
        }
        train_rows.push(train.len());
        test_rows.push(test.len());
    }
    let control = per_cand[0].1.clone();
    for (ci, c) in AUDIT_CANDIDATES.iter().enumerate() {
        summaries.push(summarize(
            c.id(),
            horizon,
            "pooled",
            &per_cand[ci].0,
            &per_cand[ci].1,
            &control,
        ));
    }
    (
        AuditReport {
            mode,
            train_rows,
            test_rows,
            summaries,
        },
        rows,
    )
}

// A liquidation window after the final halt that ends below this share of the pre-halt
// close marks a distress delisting (no KRX reason list covers 2020 onward locally).
const DISTRESS_TAIL_RATIO: f64 = 0.5;

/// Delisting kind inferred from the price tail of a name that stopped trading.
pub fn infer_kind(bars: &[Bar]) -> DelistingKind {
    let Some(last_traded) = bars.iter().rposition(|b| b.volume > 0) else {
        return DelistingKind::LastPrice;
    };
    match final_halt_start(bars) {
        Some(h) if h > 0 && h <= last_traded => {
            let before = bars[h - 1].close;
            if before > 0.0 && bars[last_traded].close / before < DISTRESS_TAIL_RATIO {
                DelistingKind::Distress
            } else {
                DelistingKind::LastPrice
            }
        }
        _ => DelistingKind::LastPrice,
    }
}

// Share-count changes smaller than this are rounding, not a corporate action.
const SHARE_CHANGE_TOLERANCE: f64 = 0.01;

/// Bars with every session from the final halt on that follows a share-count reduction
/// rescaled onto the pre-halt count (implied by unadjusted market cap over close). A
/// capital reduction during the halt is not in the exchange base price, so liquidation
/// prices would otherwise jump.
pub fn share_basis_after_halt(bars: &[Bar], extras: &[BarExtra]) -> Vec<Bar> {
    let mut out = bars.to_vec();
    let Some(h) = final_halt_start(bars).filter(|&h| h > 0) else {
        return out;
    };
    let shares = |k: usize| -> Option<f64> {
        let e = extras.get(k)?;
        (e.market_cap > 0.0 && bars[k].close > 0.0).then(|| e.market_cap / bars[k].close)
    };
    let Some(base) = shares(h - 1) else {
        return out;
    };
    for (k, b) in out.iter_mut().enumerate().skip(h) {
        let Some(now) = shares(k) else {
            continue;
        };
        let f = now / base;
        // Only reductions preserve value per old share; new shares (conversions, rights)
        // dilute, so the traded price is already what an old holder owns.
        if f < 1.0 - SHARE_CHANGE_TOLERANCE {
            b.open *= f;
            b.high *= f;
            b.low *= f;
            b.close *= f;
            b.adj_close = Some(b.close);
            b.volume = (b.volume as f64 / f).round() as u64;
        }
    }
    out
}

/// Rows dated on or after `from` of a universe name whose bars stop before the data end,
/// labelled like delisted training rows (exits through the final halt and liquidation
/// window instead of stale halted closes; rows from the halt on are not sampled).
/// `target_end` is the scheduled horizon end, so the rows are for test windows only.
pub fn stopped_name_samples(
    symbol: &str,
    bars: &[Bar],
    extras: &[BarExtra],
    market_category: &str,
    ctx: &MarketContext,
    from: NaiveDate,
) -> (Vec<Sample>, DelistingKind) {
    let rebased = share_basis_after_halt(bars, extras);
    let bars = rebased.as_slice();
    let kind = infer_kind(bars);
    let Some(last) = bars.last() else {
        return (Vec::new(), kind);
    };
    let (mut samples, _) =
        extract_delisted_samples(symbol, bars, extras, market_category, ctx, kind, last.date);
    samples.retain(|s| s.date >= from);
    // Test membership by the scheduled last session, not the (often months later) exit:
    // these rows then carry information past `target_end` and must never train.
    let max_horizon = *HORIZONS.iter().max().unwrap_or(&20);
    for s in &mut samples {
        if let Ok(t) = bars.binary_search_by_key(&s.date, |b| b.date) {
            s.target_end = bars[(t + max_horizon).min(bars.len() - 1)].date;
        }
    }
    (samples, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::Row;

    #[test]
    fn kind_is_distress_only_after_a_halt_and_a_collapse() {
        let day = |i: usize| {
            NaiveDate::from_ymd_opt(2024, 1, 1)
                .and_then(|d| d.checked_add_days(chrono::Days::new(i as u64)))
                .expect("date")
        };
        let bars = |spec: &[(f64, u64)]| -> Vec<Bar> {
            spec.iter()
                .enumerate()
                .map(|(i, &(c, v))| Bar {
                    date: day(i),
                    open: c,
                    high: c,
                    low: c,
                    close: c,
                    adj_close: Some(c),
                    volume: v,
                })
                .collect()
        };
        let mut spec = vec![(100.0, 10); 10];
        spec.extend([(100.0, 0); 5]);
        let mut crash = spec.clone();
        crash.extend([(30.0, 10), (10.0, 10)]);
        assert_eq!(infer_kind(&bars(&crash)), DelistingKind::Distress);
        let mut flat = spec.clone();
        flat.extend([(95.0, 10), (90.0, 10)]);
        assert_eq!(infer_kind(&bars(&flat)), DelistingKind::LastPrice);
        // Ends inside a halt (a merger): last price.
        assert_eq!(infer_kind(&bars(&spec)), DelistingKind::LastPrice);
    }

    fn panel(dates: &[(u32, &[&str])]) -> Panel {
        let mut rows = Vec::new();
        let mut groups = Vec::new();
        for (d, syms) in dates {
            let date = NaiveDate::from_ymd_opt(2024, 1, *d).expect("date");
            let first = rows.len();
            for (n, s) in syms.iter().enumerate() {
                rows.push(Row {
                    symbol: (*s).to_owned(),
                    date,
                    target_end: date,
                    x: vec![*d as f32 * 10.0 + n as f32],
                    ret: vec![0.0; HORIZONS.len()],
                    y: [n as f32; 3],
                });
            }
            groups.push((date, first, rows.len()));
        }
        Panel {
            names: vec!["f"],
            base_cols: 1,
            rows,
            groups,
        }
    }

    #[test]
    fn relabelled_rows_keep_the_scheduled_target_end_and_the_liquidation_exit() {
        let day = |i: usize| {
            NaiveDate::from_ymd_opt(2024, 1, 1)
                .and_then(|d| d.checked_add_days(chrono::Days::new(i as u64)))
                .expect("date")
        };
        let mut spec: Vec<(f64, u64)> = (0..100).map(|i| (100.0 + i as f64, 10)).collect();
        spec.extend([(199.0, 0); 30]);
        spec.extend([(40.0, 10), (20.0, 10)]);
        let bars: Vec<Bar> = spec
            .iter()
            .enumerate()
            .map(|(i, &(c, v))| Bar {
                date: day(i),
                open: c,
                high: c,
                low: c,
                close: c,
                adj_close: Some(c),
                volume: v,
            })
            .collect();
        let (samples, kind) =
            stopped_name_samples("X", &bars, &[], "KOSPI", &MarketContext::default(), day(0));
        assert_eq!(kind, DelistingKind::Distress);
        let last = samples.last().expect("samples before the halt");
        assert_eq!(last.date, day(99));
        // Scheduled end inside the halt, exit at the first liquidation close.
        assert_eq!(last.target_end, day(119));
        assert!((last.forward_returns[HORIZONS.len() - 1] - (40.0 / 199.0 - 1.0)).abs() < 1e-12);
    }

    #[test]
    fn a_capital_reduction_during_the_halt_is_rescaled_by_share_count() {
        let day = |i: usize| {
            NaiveDate::from_ymd_opt(2024, 1, 1)
                .and_then(|d| d.checked_add_days(chrono::Days::new(i as u64)))
                .expect("date")
        };
        // 1000 shares at 2,000 then a 100:1 reduction: liquidation trades at 50,000.
        let spec: Vec<(f64, u64, f64)> = (0..10)
            .map(|_| (2000.0, 10, 1000.0))
            .chain((0..5).map(|_| (2000.0, 0, 1000.0)))
            .chain([(50_000.0, 10, 10.0), (40_000.0, 10, 10.0)])
            .collect();
        let bars: Vec<Bar> = spec
            .iter()
            .enumerate()
            .map(|(i, &(c, v, _))| Bar {
                date: day(i),
                open: c,
                high: c,
                low: c,
                close: c,
                adj_close: Some(c),
                volume: v,
            })
            .collect();
        let extras: Vec<BarExtra> = spec
            .iter()
            .map(|&(c, _, s)| BarExtra {
                market_cap: c * s,
                trading_value: 0.0,
            })
            .collect();
        let rebased = share_basis_after_halt(&bars, &extras);
        assert!((rebased[15].close - 500.0).abs() < 1e-9);
        assert!((rebased[16].close - 400.0).abs() < 1e-9);
        assert_eq!(rebased[9].close, 2000.0);
        assert_eq!(infer_kind(&rebased), DelistingKind::Distress);
        assert_eq!(infer_kind(&bars), DelistingKind::LastPrice);
        // New shares issued during the halt dilute: traded prices stay as they are.
        let diluted: Vec<BarExtra> = extras
            .iter()
            .enumerate()
            .map(|(i, e)| BarExtra {
                market_cap: if i >= 15 {
                    bars[i].close * 4000.0
                } else {
                    e.market_cap
                },
                trading_value: 0.0,
            })
            .collect();
        assert_eq!(share_basis_after_halt(&bars, &diluted)[16].close, 40_000.0);
    }

    #[test]
    fn lag_takes_the_previous_session_row_of_the_same_symbol() {
        let mut p = panel(&[(1, &["a", "b"]), (2, &["b", "c"]), (3, &["a", "b"])]);
        let prev = previous_session(&p);
        assert_eq!(prev, vec![None, None, Some(1), None, None, Some(2)]);
        lag_features(&mut p, &prev);
        // b on day 2 takes b's day-1 value; b on day 3 takes b's original day-2 value.
        assert_eq!(p.rows[2].x, vec![11.0]);
        assert_eq!(p.rows[5].x, vec![20.0]);
    }

    #[test]
    fn permutation_stays_within_each_date() {
        let mut p = panel(&[(1, &["a", "b", "c", "d"]), (2, &["a", "b", "c"])]);
        permute_labels(&mut p, 3);
        let mut d1: Vec<f32> = p.rows[..4].iter().map(|r| r.y[0]).collect();
        let mut d2: Vec<f32> = p.rows[4..].iter().map(|r| r.y[0]).collect();
        d1.sort_by(f32::total_cmp);
        d2.sort_by(f32::total_cmp);
        assert_eq!(d1, vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(d2, vec![0.0, 1.0, 2.0]);
    }
}
