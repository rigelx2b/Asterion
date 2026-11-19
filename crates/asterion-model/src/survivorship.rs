//! Training samples for names delisted before the data.go.kr window (survivorship).
//!
//! A delisted name has only KIS daily bars, ending with a final trading halt and, for
//! distress cases, a short liquidation window (정리매매). Labels that would run past
//! the last bar exit at the terminal value: zero for distress, else the last traded
//! close. A horizon ending inside the final halt exits at the next traded close, or at
//! the terminal value when nothing trades after it. Samples are dated before the final
//! halt only: from the halt on, the delisting is public and no feature represents it.
//!
//! KIS closes under flag `0` are split-adjusted, so market cap is the close times the
//! OpenDART share count known at the delisting date, as for spliced history: later
//! information for earlier rows, so these samples are for training only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use asterion_core::Bar;
use asterion_data::DataError;
use asterion_data::dart_shares::{ShareCount, shares_on};
use asterion_data::kis::{ChartBar, history_cached, load_history};
use asterion_data::krx_delisted::{DelistedListing, DelistingKind, classify, is_board_transfer};
use chrono::NaiveDate;

use crate::confidence::liquid;
use crate::context::MarketContext;
use crate::features::{BarExtra, Sample, WARMUP, features_at};
use crate::metrics::HORIZONS;

// A trailing traded run at most this long after a halt is the liquidation window.
const MAX_LIQUIDATION_SESSIONS: usize = 15;
// A zero-volume run shorter than this before the liquidation window is not the final halt.
const MIN_FINAL_HALT_SESSIONS: usize = 3;

/// Per-name outcome counts of [`extract_delisted_samples`], summed over a load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DelistedCounts {
    pub names: usize,
    pub distress: usize,
    pub no_shares: usize,
    pub samples: usize,
    // Samples whose longest horizon exits at the terminal value.
    pub terminal_exits: usize,
    // Sessions from the final halt on, never sampled.
    pub halt_sessions_skipped: usize,
    // Distress names by tail shape: halt then liquidation trades, halt to the end, no halt.
    pub distress_liquidation: usize,
    pub distress_trailing_halt: usize,
    pub distress_no_halt: usize,
    // Liquid samples (see `confidence::liquid`), those whose longest horizon is a total
    // loss, and those losing at least half.
    pub liquid: usize,
    pub liquid_wipeouts: usize,
    pub liquid_half_losses: usize,
}

/// KIS bars as [`Bar`]s with market cap from one share count (zero when unknown).
/// Inconsistent bars are dropped; halted sessions keep their stale close.
pub fn chart_bars(history: &[ChartBar], shares: Option<u64>) -> (Vec<Bar>, Vec<BarExtra>) {
    let mut bars = Vec::with_capacity(history.len());
    let mut extras = Vec::with_capacity(history.len());
    for h in history {
        let bar = Bar {
            date: h.date,
            open: h.open,
            high: h.high,
            low: h.low,
            close: h.close,
            adj_close: Some(h.close),
            volume: h.volume.max(0.0).round() as u64,
        };
        if !bar.is_ohlc_consistent() {
            continue;
        }
        let market_cap = shares.map_or(0.0, |s| s as f64 * h.close);
        bars.push(bar);
        extras.push(BarExtra {
            market_cap,
            trading_value: h.trading_value.max(0.0),
        });
    }
    (bars, extras)
}

/// Index of the first session of the final halt, if the series ends with one.
pub fn final_halt_start(bars: &[Bar]) -> Option<usize> {
    let last_traded = bars.iter().rposition(|b| b.volume > 0)?;
    if last_traded + 1 < bars.len() {
        return Some(last_traded + 1);
    }
    let run_start = bars[..=last_traded]
        .iter()
        .rposition(|b| b.volume == 0)
        .map_or(0, |i| i + 1);
    if bars.len() - run_start > MAX_LIQUIDATION_SESSIONS {
        return None;
    }
    let halt_start = bars[..run_start]
        .iter()
        .rposition(|b| b.volume > 0)
        .map_or(0, |i| i + 1);
    (run_start - halt_start >= MIN_FINAL_HALT_SESSIONS).then_some(halt_start)
}

/// Samples of one delisted name; `delisted` is the KRX delisting date.
pub fn extract_delisted_samples(
    symbol: &str,
    bars: &[Bar],
    extras: &[BarExtra],
    market_category: &str,
    ctx: &MarketContext,
    kind: DelistingKind,
    delisted: NaiveDate,
) -> (Vec<Sample>, DelistedCounts) {
    let mut counts = DelistedCounts {
        names: 1,
        distress: usize::from(kind == DelistingKind::Distress),
        ..DelistedCounts::default()
    };
    let Some(last_traded) = bars.iter().rposition(|b| b.volume > 0) else {
        return (Vec::new(), counts);
    };
    let halt = final_halt_start(bars);
    if kind == DelistingKind::Distress {
        match halt {
            None => counts.distress_no_halt = 1,
            Some(h) if h > last_traded => counts.distress_trailing_halt = 1,
            Some(_) => counts.distress_liquidation = 1,
        }
    }
    let sample_end = halt.unwrap_or(last_traded + 1);
    counts.halt_sessions_skipped = bars.len() - sample_end;
    let terminal = match kind {
        DelistingKind::Distress => 0.0,
        DelistingKind::LastPrice => bars[last_traded].close,
    };
    let terminal_date = delisted.max(bars[bars.len() - 1].date);
    // Exit price and date of a position whose horizon ends at session `k`, and whether
    // it is the terminal value.
    let exit = |k: usize| -> (f64, NaiveDate, bool) {
        if k >= bars.len() {
            return (terminal, terminal_date, true);
        }
        if halt.is_some_and(|h| k >= h) && bars[k].volume == 0 {
            return match (k + 1..bars.len()).find(|&j| bars[j].volume > 0) {
                Some(j) => (bars[j].close, bars[j].date, false),
                None => (terminal, terminal_date, true),
            };
        }
        (bars[k].close, bars[k].date, false)
    };
    // The liquidation-window jumps are the delisting itself, so only earlier sessions
    // are checked for jumps the price basis cannot explain.
    let check_until = halt.unwrap_or(bars.len());
    let index = ctx.index_for(market_category);
    let max_horizon = *HORIZONS.iter().max().unwrap_or(&20);
    let mut samples = Vec::new();
    for t in WARMUP..sample_end {
        let jump = ((t + 1)..=(t + max_horizon).min(check_until - 1)).any(|k| {
            let ratio = bars[k].close / bars[k - 1].close;
            !(0.6..=1.6).contains(&ratio)
        });
        if jump {
            continue;
        }
        let Some(features) = features_at(symbol, bars, extras, t, index, ctx) else {
            continue;
        };
        let close_t = bars[t].close;
        let forward_returns: Vec<f64> = HORIZONS
            .iter()
            .map(|&h| exit(t + h).0 / close_t - 1.0)
            .collect();
        let (_, target_end, at_terminal) = exit(t + max_horizon);
        counts.terminal_exits += usize::from(at_terminal);
        samples.push(Sample {
            symbol: symbol.to_owned(),
            date: bars[t].date,
            features,
            forward_returns,
            target_end,
            trading_value: extras.get(t).map_or(0.0, |e| e.trading_value),
        });
    }
    counts.samples = samples.len();
    (samples, counts)
}

/// One delisted name selected for the training universe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelistedName {
    pub code: String,
    pub market_category: String,
    pub date: NaiveDate,
    pub kind: DelistingKind,
}

/// Names delisted on or after `from` per (market category, KIND list), skipping codes in
/// `exclude` (names already in the universe) and board transfers. Fails on an
/// unclassified reason or on a code listed twice in the window.
pub fn select_delisted(
    lists: &[(&str, Vec<DelistedListing>)],
    exclude: &BTreeSet<String>,
    from: NaiveDate,
) -> Result<Vec<DelistedName>, String> {
    let mut out: Vec<DelistedName> = Vec::new();
    let mut unmatched = BTreeSet::new();
    for (market, listings) in lists {
        for l in listings {
            if l.date < from || exclude.contains(&l.code) || is_board_transfer(&l.reason) {
                continue;
            }
            match classify(&l.reason) {
                Some(kind) => out.push(DelistedName {
                    code: l.code.clone(),
                    market_category: (*market).to_owned(),
                    date: l.date,
                    kind,
                }),
                None => {
                    unmatched.insert(l.reason.clone());
                }
            }
        }
    }
    if !unmatched.is_empty() {
        let reasons: Vec<String> = unmatched.into_iter().collect();
        return Err(format!(
            "unclassified delisting reasons: {}",
            reasons.join(" | ")
        ));
    }
    out.sort_by(|a, b| a.code.cmp(&b.code));
    if let Some(w) = out.windows(2).find(|w| w[0].code == w[1].code) {
        return Err(format!("code {} delisted twice in the window", w[0].code));
    }
    Ok(out)
}

/// Samples of every name in `names` from cached KIS bars (adjustment flag `adjusted`),
/// dated on or after `samples_from`. Returns the codes with no KIS cache separately.
pub fn load_delisted_samples(
    names: &[DelistedName],
    history_root: &Path,
    adjusted: &str,
    shares: &BTreeMap<String, Vec<ShareCount>>,
    ctx: &MarketContext,
    samples_from: NaiveDate,
) -> Result<(Vec<Sample>, DelistedCounts, Vec<String>), DataError> {
    let mut all = Vec::new();
    let mut counts = DelistedCounts::default();
    let mut missing = Vec::new();
    for name in names {
        if !history_cached(history_root, &name.code, adjusted) {
            missing.push(name.code.clone());
            continue;
        }
        let mut history = load_history(history_root, &name.code, adjusted)?;
        history.retain(|h| h.date <= name.date);
        let counts_for = shares.get(&name.code).map_or(&[][..], Vec::as_slice);
        let (bars, extras) = chart_bars(&history, shares_on(counts_for, name.date));
        let (samples, mut c) = extract_delisted_samples(
            &name.code,
            &bars,
            &extras,
            &name.market_category,
            ctx,
            name.kind,
            name.date,
        );
        c.no_shares = usize::from(counts_for.is_empty());
        let samples: Vec<Sample> = samples
            .into_iter()
            .filter(|s| s.date >= samples_from)
            .collect();
        c.samples = samples.len();
        for s in samples.iter().filter(|s| liquid(s)) {
            let longest = s.forward_returns.last().copied().unwrap_or(0.0);
            c.liquid += 1;
            c.liquid_wipeouts += usize::from(longest <= -1.0);
            c.liquid_half_losses += usize::from(longest <= -0.5);
        }
        counts.add(c);
        all.extend(samples);
    }
    Ok((all, counts, missing))
}

impl DelistedCounts {
    pub fn add(&mut self, other: DelistedCounts) {
        self.names += other.names;
        self.distress += other.distress;
        self.no_shares += other.no_shares;
        self.samples += other.samples;
        self.terminal_exits += other.terminal_exits;
        self.halt_sessions_skipped += other.halt_sessions_skipped;
        self.distress_liquidation += other.distress_liquidation;
        self.distress_trailing_halt += other.distress_trailing_halt;
        self.distress_no_halt += other.distress_no_halt;
        self.liquid += other.liquid;
        self.liquid_wipeouts += other.liquid_wipeouts;
        self.liquid_half_losses += other.liquid_half_losses;
    }
}

#[cfg(test)]
mod tests {
    use chrono::Days;

    use super::*;

    fn day(i: usize) -> NaiveDate {
        NaiveDate::from_ymd_opt(2018, 1, 1)
            .and_then(|d| d.checked_add_days(Days::new(i as u64)))
            .expect("date")
    }

    // (close, traded) per session.
    fn bars(spec: &[(f64, bool)]) -> Vec<Bar> {
        spec.iter()
            .enumerate()
            .map(|(i, &(close, traded))| Bar {
                date: day(i),
                open: close,
                high: close,
                low: close,
                close,
                adj_close: Some(close),
                volume: if traded { 1_000 } else { 0 },
            })
            .collect()
    }

    fn rising(n: usize) -> Vec<(f64, bool)> {
        (0..n).map(|i| (100.0 + i as f64 * 0.1, true)).collect()
    }

    fn extract(spec: &[(f64, bool)], kind: DelistingKind) -> (Vec<Sample>, DelistedCounts) {
        let b = bars(spec);
        let delisted = b[b.len() - 1].date;
        extract_delisted_samples(
            "000001",
            &b,
            &[],
            "KOSDAQ",
            &MarketContext::default(),
            kind,
            delisted,
        )
    }

    #[test]
    fn final_halt_is_found_before_a_liquidation_window_only() {
        let mut spec = rising(100);
        assert_eq!(final_halt_start(&bars(&spec)), None);
        // A one-day zero-volume blip near the end is not a final halt.
        spec[95].1 = false;
        assert_eq!(final_halt_start(&bars(&spec)), None);
        let mut spec = rising(100);
        spec.extend((0..10).map(|_| (110.0, false)));
        assert_eq!(final_halt_start(&bars(&spec)), Some(100));
        spec.extend((0..7).map(|i| (10.0 - i as f64, true)));
        assert_eq!(final_halt_start(&bars(&spec)), Some(100));
    }

    #[test]
    fn distress_labels_exit_through_liquidation_or_at_zero() {
        // 100 traded, 10 halted at 110, 7 liquidation sessions from 10 down to 4.
        let mut spec = rising(100);
        let stale = spec[99].0;
        spec.extend((0..10).map(|_| (stale, false)));
        spec.extend((0..7).map(|i| (10.0 - i as f64, true)));
        let (samples, counts) = extract(&spec, DelistingKind::Distress);
        assert_eq!(counts.halt_sessions_skipped, 17);
        assert_eq!(samples.len(), 100 - WARMUP);
        let last = samples.last().expect("sample");
        let close = spec[99].0;
        // h1 lands inside the halt: exit at the first liquidation close.
        assert!((last.forward_returns[0] - (10.0 / close - 1.0)).abs() < 1e-12);
        // h20 runs past the last bar: terminal value zero.
        assert_eq!(last.forward_returns[4], -1.0);
        assert!(
            samples
                .iter()
                .all(|s| s.forward_returns.iter().all(|r| r.is_finite()))
        );
        // Liquidation jumps do not drop the samples whose horizons span them.
        assert!(counts.terminal_exits > 0);
    }

    #[test]
    fn last_price_exit_holds_the_last_traded_close() {
        let mut spec = rising(100);
        let stale = spec[99].0;
        spec.extend((0..30).map(|_| (stale, false)));
        let (samples, _) = extract(&spec, DelistingKind::LastPrice);
        let last = samples.last().expect("sample");
        assert_eq!(last.forward_returns[4], 0.0);
        assert_eq!(last.target_end, day(129));
    }

    #[test]
    fn selection_skips_universe_codes_and_old_delistings_and_fails_on_unknown_reasons() {
        let listing = |code: &str, y: i32, reason: &str| DelistedListing {
            code: code.to_owned(),
            name: String::new(),
            date: NaiveDate::from_ymd_opt(y, 6, 1).expect("date"),
            reason: reason.to_owned(),
        };
        let from = NaiveDate::from_ymd_opt(2015, 1, 1).expect("date");
        let exclude: BTreeSet<String> = ["000003".to_owned()].into();
        let lists = vec![(
            "KOSDAQ",
            vec![
                listing("000001", 2016, "자본전액잠식"),
                listing("000002", 2014, "자본전액잠식"),
                listing("000003", 2017, "피흡수합병"),
                listing("000004", 2018, "피흡수합병"),
                listing("000007", 2018, "유가증권시장 상장"),
            ],
        )];
        let names = select_delisted(&lists, &exclude, from).expect("select");
        let got: Vec<(&str, DelistingKind)> =
            names.iter().map(|n| (n.code.as_str(), n.kind)).collect();
        assert_eq!(
            got,
            [
                ("000001", DelistingKind::Distress),
                ("000004", DelistingKind::LastPrice)
            ]
        );
        assert_eq!(names[0].market_category, "KOSDAQ");
        let unknown = vec![("KOSPI", vec![listing("000005", 2019, "기타")])];
        assert!(select_delisted(&unknown, &exclude, from).is_err());
        let twice = vec![(
            "KOSPI",
            vec![
                listing("000006", 2016, "피흡수합병"),
                listing("000006", 2019, "피흡수합병"),
            ],
        )];
        assert!(select_delisted(&twice, &exclude, from).is_err());
    }

    #[test]
    fn unexplained_jumps_before_the_halt_still_drop_samples() {
        let mut spec = rising(100);
        spec[80].0 = 300.0;
        let (samples, _) = extract(&spec, DelistingKind::LastPrice);
        assert!(
            samples
                .iter()
                .all(|s| s.date < day(60) || s.date >= day(81))
        );
    }
}
