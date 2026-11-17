//! Cross-sectional ranking research: purged walk-forward folds that compare rank-based
//! scores against a low-volatility control by top-decile long-only net returns.
//!
//! Every stock-level feature is ranked within its date, so scores need no fitted scaler
//! and carry no information from other dates. Market-wide features are dropped because
//! they are constant within a date.

use std::collections::HashMap;
use std::thread;

use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::confidence::liquid;
use crate::features::{FEATURE_NAMES, Sample};
use crate::metrics::HORIZONS;
use crate::neural::default_threads;

// Indices into HORIZONS for 5, 10 and 20 sessions.
pub const CV_HORIZONS: [usize; 3] = [2, 3, 4];
pub const PRIMARY: usize = 2;
pub const TOP_FRACTION: f64 = 0.1;
pub const MIN_NAMES: usize = 50;
pub const COST: f64 = 0.0025;
const TRADING_DAYS: f64 = 250.0;
const BINS: usize = 32;

const MARKET_PREFIXES: [&str; 15] = [
    "mkt_",
    "sp500_",
    "nasdaq_",
    "vix_",
    "us10y_",
    "usdkrw_",
    "wti_",
    "dollar_",
    "usdjpy_",
    "usdcny_",
    "us2y_",
    "term_spread",
    "baa_",
    "vxn_",
    "breakeven_",
];

const COMPOSITE: [(&str, f64); 6] = [
    ("volatility_20", -1.0),
    ("max_ret_20", -1.0),
    ("log_ret_20", -1.0),
    ("fund_earnings_yield", 1.0),
    ("fund_book_to_market", 1.0),
    ("fund_roe", 1.0),
];

pub const CONTROL: &str = "volatility_20";

fn is_market_wide(name: &str) -> bool {
    MARKET_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Liquid rows ranked within each date, sorted by date.
#[derive(Debug, Clone)]
pub struct Panel {
    pub names: Vec<&'static str>,
    // Leading columns of `x` that the base candidates use; extra columns follow.
    pub base_cols: usize,
    pub rows: Vec<Row>,
    // Contiguous [start, end) row ranges, one per date, ascending.
    pub groups: Vec<(NaiveDate, usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub symbol: String,
    pub date: NaiveDate,
    pub target_end: NaiveDate,
    pub x: Vec<f32>,
    // Raw forward returns for every entry of HORIZONS.
    pub ret: Vec<f64>,
    // Ranked forward return per CV_HORIZONS entry.
    pub y: [f32; 3],
}

/// Average-tie ranks scaled to [-0.5, 0.5]; a single value maps to 0.
pub fn centered_ranks(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let mut out = vec![0.0; n];
    if n < 2 {
        return out;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && values[order[j]] == values[order[i]] {
            j += 1;
        }
        let r = (i + j - 1) as f64 / 2.0 / (n - 1) as f64 - 0.5;
        for &k in &order[i..j] {
            out[k] = r;
        }
        i = j;
    }
    out
}

/// Kept feature columns: every stock-level feature, in FEATURE_NAMES order.
pub fn kept_features() -> Vec<usize> {
    (0..FEATURE_NAMES.len())
        .filter(|&j| !is_market_wide(FEATURE_NAMES[j]))
        .collect()
}

/// Ranks one date's raw feature rows into the kept columns. Fundamentals are ranked
/// only among names with statements available; the rest stay at 0.
pub fn rank_cross_section(features: &[&[f64]]) -> Vec<Vec<f32>> {
    let kept = kept_features();
    let avail = FEATURE_NAMES.iter().position(|n| *n == "fund_available");
    let n = features.len();
    let mut x = vec![vec![0f32; kept.len()]; n];
    let has_fund: Vec<bool> = features
        .iter()
        .map(|f| avail.is_some_and(|a| f[a] > 0.5))
        .collect();
    for (c, &j) in kept.iter().enumerate() {
        let name = FEATURE_NAMES[j];
        let fundamental = name.starts_with("fund_") && name != "fund_available";
        let members: Vec<usize> = (0..n).filter(|&i| !fundamental || has_fund[i]).collect();
        let values: Vec<f64> = members.iter().map(|&i| features[i][j]).collect();
        for (&i, r) in members.iter().zip(centered_ranks(&values)) {
            x[i][c] = r as f32;
        }
    }
    x
}

impl Panel {
    pub fn build(samples: Vec<Sample>) -> Panel {
        Self::build_with(samples, &[], &|_, _| Vec::new())
    }

    /// Like `build`, plus extra raw columns per (symbol, date) ranked per date and
    /// appended after the base columns.
    pub fn build_with(
        samples: Vec<Sample>,
        extra_names: &[&'static str],
        extra: &(dyn Fn(&str, NaiveDate) -> Vec<f64> + Sync),
    ) -> Panel {
        let mut names: Vec<&'static str> =
            kept_features().iter().map(|&j| FEATURE_NAMES[j]).collect();
        let base_cols = names.len();
        names.extend_from_slice(extra_names);
        let mut samples: Vec<Sample> = samples.into_iter().filter(liquid).collect();
        samples.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.symbol.cmp(&b.symbol)));

        let mut rows = Vec::with_capacity(samples.len());
        let mut groups = Vec::new();
        let mut start = 0;
        while start < samples.len() {
            let date = samples[start].date;
            let end = start + samples[start..].partition_point(|s| s.date == date);
            let group = &samples[start..end];
            start = end;
            if group.len() < MIN_NAMES {
                continue;
            }
            let n = group.len();
            let feats: Vec<&[f64]> = group.iter().map(|s| s.features.as_slice()).collect();
            let mut x = rank_cross_section(&feats);
            if !extra_names.is_empty() {
                let raw: Vec<Vec<f64>> = group.iter().map(|s| extra(&s.symbol, s.date)).collect();
                for c in 0..extra_names.len() {
                    let values: Vec<f64> = raw.iter().map(|r| r[c]).collect();
                    for (xi, r) in x.iter_mut().zip(centered_ranks(&values)) {
                        xi.push(r as f32);
                    }
                }
            }
            let mut y = vec![[0f32; 3]; n];
            for (k, &h) in CV_HORIZONS.iter().enumerate() {
                let values: Vec<f64> = group.iter().map(|s| s.forward_returns[h]).collect();
                for (i, r) in centered_ranks(&values).into_iter().enumerate() {
                    y[i][k] = r as f32;
                }
            }
            let first = rows.len();
            for ((s, xi), yi) in group.iter().zip(x).zip(y) {
                rows.push(Row {
                    symbol: s.symbol.clone(),
                    date: s.date,
                    target_end: s.target_end,
                    x: xi,
                    ret: s.forward_returns.clone(),
                    y: yi,
                });
            }
            groups.push((date, first, rows.len()));
        }
        Panel {
            names,
            base_cols,
            rows,
            groups,
        }
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| *n == name)
    }
}

/// Scoring rules compared by the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Candidate {
    ControlLowvol,
    Composite6,
    Ridge,
    Gbdt,
    // Trial 4 (addendum_dart_topics.json): gbdt on base plus title-topic columns.
    GbdtDart,
}

impl Candidate {
    pub const ALL: [Candidate; 5] = [
        Candidate::ControlLowvol,
        Candidate::Composite6,
        Candidate::Ridge,
        Candidate::Gbdt,
        Candidate::GbdtDart,
    ];
    // The arms frozen in the rank4 ledger.
    pub const BASE: [Candidate; 4] = [
        Candidate::ControlLowvol,
        Candidate::Composite6,
        Candidate::Ridge,
        Candidate::Gbdt,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Candidate::ControlLowvol => "control_lowvol",
            Candidate::Composite6 => "composite6",
            Candidate::Ridge => "ridge",
            Candidate::Gbdt => "gbdt",
            Candidate::GbdtDart => "gbdt_dart",
        }
    }
}

/// A fitted score; fixed-rule candidates carry their weights as a linear model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Scorer {
    Linear { weights: Vec<f64>, bias: f64 },
    Trees(Gbdt),
}

impl Scorer {
    pub fn score(&self, x: &[f32]) -> f64 {
        match self {
            Scorer::Linear { weights, bias } => {
                bias + weights
                    .iter()
                    .zip(x)
                    .map(|(w, v)| w * f64::from(*v))
                    .sum::<f64>()
            }
            Scorer::Trees(g) => g.predict(x),
        }
    }
}

pub fn fit(candidate: Candidate, panel: &Panel, train: &[usize], k: usize) -> Scorer {
    let p = panel.base_cols;
    let fixed = |pairs: &[(&str, f64)]| {
        let mut weights = vec![0.0; p];
        for (name, sign) in pairs {
            if let Some(j) = panel.index_of(name) {
                weights[j] = sign / pairs.len() as f64;
            }
        }
        Scorer::Linear { weights, bias: 0.0 }
    };
    match candidate {
        Candidate::ControlLowvol => fixed(&[(CONTROL, -1.0)]),
        Candidate::Composite6 => fixed(&COMPOSITE),
        Candidate::Ridge => fit_ridge(panel, train, k, p),
        Candidate::Gbdt => Scorer::Trees(Gbdt::fit(panel, train, k, p, &GbdtParams::default())),
        Candidate::GbdtDart => Scorer::Trees(Gbdt::fit(
            panel,
            train,
            k,
            panel.names.len(),
            &GbdtParams::default(),
        )),
    }
}

fn par_chunks<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&[T]) -> R + Sync,
{
    let threads = default_threads();
    let chunk = items.len().div_ceil(threads).max(1);
    thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| scope.spawn(move || f(part)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

fn fit_ridge(panel: &Panel, train: &[usize], k: usize, p: usize) -> Scorer {
    let d = p + 1;
    let parts = par_chunks(train, |part| {
        let mut xtx = vec![0.0; d * d];
        let mut xty = vec![0.0; d];
        let mut z = vec![0.0; d];
        for &i in part {
            let row = &panel.rows[i];
            for (zj, v) in z.iter_mut().zip(&row.x) {
                *zj = f64::from(*v);
            }
            z[p] = 1.0;
            let y = f64::from(row.y[k]);
            for a in 0..d {
                xty[a] += z[a] * y;
                for b in a..d {
                    xtx[a * d + b] += z[a] * z[b];
                }
            }
        }
        (xtx, xty)
    });
    let mut xtx = vec![0.0; d * d];
    let mut xty = vec![0.0; d];
    for (a, b) in parts {
        xtx.iter_mut().zip(a).for_each(|(s, v)| *s += v);
        xty.iter_mut().zip(b).for_each(|(s, v)| *s += v);
    }
    for a in 0..d {
        for b in 0..a {
            xtx[a * d + b] = xtx[b * d + a];
        }
    }
    let lambda = 1e-3 * train.len() as f64;
    for a in 0..p {
        xtx[a * d + a] += lambda;
    }
    let beta = solve(xtx, xty, d);
    Scorer::Linear {
        weights: beta[..p].to_vec(),
        bias: beta[p],
    }
}

// Gaussian elimination with partial pivoting on a dense d x d system.
fn solve(mut a: Vec<f64>, mut b: Vec<f64>, d: usize) -> Vec<f64> {
    for col in 0..d {
        let piv = (col..d)
            .max_by(|&i, &j| a[i * d + col].abs().total_cmp(&a[j * d + col].abs()))
            .unwrap_or(col);
        if a[piv * d + col].abs() < 1e-12 {
            continue;
        }
        if piv != col {
            for c in 0..d {
                a.swap(piv * d + c, col * d + c);
            }
            b.swap(piv, col);
        }
        for r in col + 1..d {
            let f = a[r * d + col] / a[col * d + col];
            if f != 0.0 {
                for c in col..d {
                    a[r * d + c] -= f * a[col * d + c];
                }
                b[r] -= f * b[col];
            }
        }
    }
    let mut x = vec![0.0; d];
    for r in (0..d).rev() {
        let diag = a[r * d + r];
        if diag.abs() < 1e-12 {
            continue;
        }
        let s: f64 = (r + 1..d).map(|c| a[r * d + c] * x[c]).sum();
        x[r] = (b[r] - s) / diag;
    }
    x
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GbdtParams {
    pub trees: usize,
    pub depth: usize,
    pub learning_rate: f64,
    pub min_leaf: usize,
    pub subsample: f64,
    pub seed: u64,
}

impl Default for GbdtParams {
    fn default() -> Self {
        GbdtParams {
            trees: 300,
            depth: 3,
            learning_rate: 0.05,
            min_leaf: 2000,
            subsample: 0.3,
            seed: 7,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Node {
    // Rows with bin <= threshold go left.
    Split {
        feature: usize,
        threshold: u8,
        left: usize,
        right: usize,
    },
    Leaf(f64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gbdt {
    pub base: f64,
    pub trees: Vec<Vec<Node>>,
}

pub fn bin_of(v: f32) -> u8 {
    let b = ((f64::from(v) + 0.5) * BINS as f64).floor();
    b.clamp(0.0, (BINS - 1) as f64) as u8
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn tree_predict(tree: &[Node], bins: &[u8]) -> f64 {
    let mut n = 0;
    loop {
        match tree[n] {
            Node::Leaf(v) => return v,
            Node::Split {
                feature,
                threshold,
                left,
                right,
            } => {
                n = if bins[feature] <= threshold {
                    left
                } else {
                    right
                }
            }
        }
    }
}

impl Gbdt {
    pub fn predict(&self, x: &[f32]) -> f64 {
        let bins: Vec<u8> = x.iter().map(|v| bin_of(*v)).collect();
        self.base
            + self
                .trees
                .iter()
                .map(|t| tree_predict(t, &bins))
                .sum::<f64>()
    }

    pub fn fit(panel: &Panel, train: &[usize], k: usize, p: usize, params: &GbdtParams) -> Gbdt {
        let n = train.len();
        let mut bins = vec![0u8; n * p];
        for (r, &i) in train.iter().enumerate() {
            for (c, v) in panel.rows[i].x.iter().take(p).enumerate() {
                bins[r * p + c] = bin_of(*v);
            }
        }
        let y: Vec<f64> = train
            .iter()
            .map(|&i| f64::from(panel.rows[i].y[k]))
            .collect();
        let base = y.iter().sum::<f64>() / n.max(1) as f64;
        let mut pred = vec![base; n];
        let mut state = params.seed;
        let mut trees = Vec::with_capacity(params.trees);
        let threshold = (params.subsample * u64::MAX as f64) as u64;
        for _ in 0..params.trees {
            let grad: Vec<f64> = y.iter().zip(&pred).map(|(a, b)| a - b).collect();
            let sample: Vec<usize> = (0..n)
                .filter(|_| splitmix(&mut state) < threshold)
                .collect();
            let tree = grow(&bins, p, &grad, sample, params);
            let updates = par_chunks(&(0..n).collect::<Vec<_>>(), |part| {
                part.iter()
                    .map(|&r| tree_predict(&tree, &bins[r * p..(r + 1) * p]))
                    .collect::<Vec<_>>()
            });
            for (pr, u) in pred.iter_mut().zip(updates.into_iter().flatten()) {
                *pr += u;
            }
            trees.push(tree);
        }
        Gbdt { base, trees }
    }
}

// Depth-limited least-squares tree on pre-binned rows; leaves are scaled by the learning rate.
fn grow(bins: &[u8], p: usize, grad: &[f64], rows: Vec<usize>, params: &GbdtParams) -> Vec<Node> {
    let mut nodes = vec![Node::Leaf(0.0)];
    let mut frontier = vec![(0usize, rows)];
    for _ in 0..params.depth {
        let mut next = Vec::new();
        for (id, rows) in frontier {
            match best_split(bins, p, grad, &rows, params.min_leaf) {
                Some((feature, threshold)) => {
                    let (l, r): (Vec<usize>, Vec<usize>) = rows
                        .into_iter()
                        .partition(|&i| bins[i * p + feature] <= threshold);
                    let left = nodes.len();
                    nodes.push(Node::Leaf(0.0));
                    nodes.push(Node::Leaf(0.0));
                    nodes[id] = Node::Split {
                        feature,
                        threshold,
                        left,
                        right: left + 1,
                    };
                    next.push((left, l));
                    next.push((left + 1, r));
                }
                None => next.push((id, rows)),
            }
        }
        frontier = next;
    }
    for (id, rows) in frontier {
        if matches!(nodes[id], Node::Leaf(_)) {
            let mean = rows.iter().map(|&i| grad[i]).sum::<f64>() / rows.len().max(1) as f64;
            nodes[id] = Node::Leaf(params.learning_rate * mean);
        }
    }
    nodes
}

fn best_split(
    bins: &[u8],
    p: usize,
    grad: &[f64],
    rows: &[usize],
    min_leaf: usize,
) -> Option<(usize, u8)> {
    if rows.len() < 2 * min_leaf {
        return None;
    }
    let hists = par_chunks(rows, |part| {
        let mut sum = vec![0.0; p * BINS];
        let mut cnt = vec![0usize; p * BINS];
        for &i in part {
            let g = grad[i];
            for c in 0..p {
                let b = c * BINS + bins[i * p + c] as usize;
                sum[b] += g;
                cnt[b] += 1;
            }
        }
        (sum, cnt)
    });
    let mut sum = vec![0.0; p * BINS];
    let mut cnt = vec![0usize; p * BINS];
    for (s, c) in hists {
        sum.iter_mut().zip(s).for_each(|(a, v)| *a += v);
        cnt.iter_mut().zip(c).for_each(|(a, v)| *a += v);
    }
    let total: f64 = (0..BINS).map(|b| sum[b]).sum();
    let n = rows.len();
    let parent = total * total / n as f64;
    let mut best: Option<(f64, usize, u8)> = None;
    for c in 0..p {
        let (mut sl, mut nl) = (0.0, 0usize);
        for b in 0..BINS - 1 {
            sl += sum[c * BINS + b];
            nl += cnt[c * BINS + b];
            let nr = n - nl;
            if nl < min_leaf || nr < min_leaf {
                continue;
            }
            let sr = total - sl;
            let gain = sl * sl / nl as f64 + sr * sr / nr as f64 - parent;
            if gain > 0.0 && best.is_none_or(|(g, _, _)| gain > g) {
                best = Some((gain, c, b as u8));
            }
        }
    }
    best.map(|(_, c, b)| (c, b))
}

/// One test window of the walk-forward.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Fold {
    pub test_from: NaiveDate,
    pub test_to: NaiveDate,
}

pub fn split(panel: &Panel, fold: Fold, embargo_days: u64) -> (Vec<usize>, Vec<usize>) {
    let cut = fold
        .test_from
        .checked_sub_days(Days::new(embargo_days))
        .unwrap_or(fold.test_from);
    let mut train = Vec::new();
    let mut test = Vec::new();
    for (i, r) in panel.rows.iter().enumerate() {
        if r.target_end < fold.test_from && r.date < cut {
            train.push(i);
        } else if r.date >= fold.test_from && r.target_end < fold.test_to {
            test.push(i);
        }
    }
    (train, test)
}

/// Net results of one rebalance period for a scored portfolio and the benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PeriodRecord {
    pub date: NaiveDate,
    pub offset: usize,
    pub top_net: f64,
    pub bench_net: f64,
    pub turnover: f64,
    #[serde(default)]
    pub bench_turnover: f64,
}

fn one_way_turnover(prev: &HashMap<&str, f64>, next: &HashMap<&str, f64>) -> f64 {
    let mut buys = 0.0;
    for (s, w) in next {
        buys += (w - prev.get(s).copied().unwrap_or(0.0)).max(0.0);
    }
    buys
}

fn equal_weights<'a>(names: impl Iterator<Item = &'a str>) -> HashMap<&'a str, f64> {
    let v: Vec<&str> = names.collect();
    let w = 1.0 / v.len().max(1) as f64;
    v.into_iter().map(|s| (s, w)).collect()
}

/// Per-date IC and every-offset top-decile chains over the test rows (sorted by date).
pub fn evaluate(
    panel: &Panel,
    test: &[usize],
    scores: &[f64],
    k: usize,
    skip: bool,
) -> (Vec<(NaiveDate, f64)>, Vec<PeriodRecord>) {
    let h_idx = CV_HORIZONS[k];
    let horizon = HORIZONS[h_idx];
    let mut dates: Vec<(NaiveDate, Vec<usize>)> = Vec::new();
    for (pos, &i) in test.iter().enumerate() {
        let d = panel.rows[i].date;
        match dates.last_mut() {
            Some((last, v)) if *last == d => v.push(pos),
            _ => dates.push((d, vec![pos])),
        }
    }
    let mut ics = Vec::new();
    let mut tops: Vec<Vec<usize>> = Vec::with_capacity(dates.len());
    for (d, members) in &dates {
        let s: Vec<f64> = members.iter().map(|&q| scores[q]).collect();
        let sr = centered_ranks(&s);
        let yr: Vec<f64> = members
            .iter()
            .map(|&q| f64::from(panel.rows[test[q]].y[k]))
            .collect();
        ics.push((*d, pearson(&sr, &yr)));
        let take = ((members.len() as f64) * TOP_FRACTION).ceil() as usize;
        let mut order: Vec<usize> = (0..members.len()).collect();
        order.sort_by(|&a, &b| s[b].total_cmp(&s[a]));
        tops.push(order.into_iter().take(take).map(|o| members[o]).collect());
    }
    let mut records = Vec::new();
    for offset in 0..horizon.min(dates.len()) {
        let mut prev_top: Option<HashMap<&str, f64>> = None;
        let mut prev_bench: Option<HashMap<&str, f64>> = None;
        let mut t = offset;
        while t < dates.len() {
            let (d, members) = &dates[t];
            let top = equal_weights(tops[t].iter().map(|&q| panel.rows[test[q]].symbol.as_str()));
            let bench = equal_weights(members.iter().map(|&q| panel.rows[test[q]].symbol.as_str()));
            let ret = |w: &HashMap<&str, f64>, set: &[usize]| -> f64 {
                set.iter()
                    .map(|&q| {
                        let r = &panel.rows[test[q]];
                        // With `skip`, entry is the next session's close instead of today's.
                        let ret = if skip {
                            (1.0 + r.ret[h_idx]) / (1.0 + r.ret[0]) - 1.0
                        } else {
                            r.ret[h_idx]
                        };
                        w.get(r.symbol.as_str()).copied().unwrap_or(0.0) * ret
                    })
                    .sum()
            };
            let tt = prev_top.as_ref().map_or(0.0, |p| one_way_turnover(p, &top));
            let tb = prev_bench
                .as_ref()
                .map_or(0.0, |p| one_way_turnover(p, &bench));
            records.push(PeriodRecord {
                date: *d,
                offset,
                top_net: ret(&top, &tops[t]) - tt * COST,
                bench_net: ret(&bench, members) - tb * COST,
                turnover: tt,
                bench_turnover: tb,
            });
            prev_top = Some(top);
            prev_bench = Some(bench);
            t += horizon;
        }
    }
    (ics, records)
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma) * (x - ma);
        sbb += (y - mb) * (y - mb);
    }
    if saa <= 0.0 || sbb <= 0.0 {
        0.0
    } else {
        sab / (saa * sbb).sqrt()
    }
}

/// Mean and t statistic of overlapping observations, with n / overlap independent ones.
pub fn mean_t(values: &[f64], overlap: usize) -> (f64, f64) {
    let n = values.len() as f64;
    if n < 2.0 {
        return (values.first().copied().unwrap_or(0.0), 0.0);
    }
    let m = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (n - 1.0);
    let eff = (n / overlap.max(1) as f64).max(1.0);
    let t = if var > 0.0 {
        m / (var / eff).sqrt()
    } else {
        0.0
    };
    (m, t)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub candidate: String,
    pub horizon: usize,
    pub window: String,
    pub dates: usize,
    pub ic_mean: f64,
    pub ic_t: f64,
    pub periods: usize,
    pub top_net_annual: f64,
    pub bench_net_annual: f64,
    pub excess_annual: f64,
    pub excess_t: f64,
    pub excess_win_rate: f64,
    pub mean_turnover: f64,
    // Candidate excess minus control excess, matched by date and offset.
    pub increment_annual: f64,
    pub increment_t: f64,
}

pub fn summarize(
    candidate: &str,
    horizon: usize,
    window: &str,
    ics: &[(NaiveDate, f64)],
    records: &[PeriodRecord],
    control: &[PeriodRecord],
) -> Summary {
    let per_year = TRADING_DAYS / horizon as f64;
    let ic: Vec<f64> = ics.iter().map(|(_, v)| *v).collect();
    let (ic_mean, ic_t) = mean_t(&ic, horizon);
    let excess: Vec<f64> = records.iter().map(|r| r.top_net - r.bench_net).collect();
    let (ex, ex_t) = mean_t(&excess, horizon);
    let ctrl: HashMap<(NaiveDate, usize), f64> = control
        .iter()
        .map(|r| ((r.date, r.offset), r.top_net - r.bench_net))
        .collect();
    let inc: Vec<f64> = records
        .iter()
        .filter_map(|r| {
            ctrl.get(&(r.date, r.offset))
                .map(|c| r.top_net - r.bench_net - c)
        })
        .collect();
    let (inc_m, inc_t) = mean_t(&inc, horizon);
    let n = records.len().max(1) as f64;
    Summary {
        candidate: candidate.to_owned(),
        horizon,
        window: window.to_owned(),
        dates: ics.len(),
        ic_mean,
        ic_t,
        periods: records.len(),
        top_net_annual: records.iter().map(|r| r.top_net).sum::<f64>() / n * per_year,
        bench_net_annual: records.iter().map(|r| r.bench_net).sum::<f64>() / n * per_year,
        excess_annual: ex * per_year,
        excess_t: ex_t,
        excess_win_rate: excess.iter().filter(|v| **v > 0.0).count() as f64 / n,
        mean_turnover: records.iter().map(|r| r.turnover).sum::<f64>() / n,
        increment_annual: inc_m * per_year,
        increment_t: inc_t,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub candidate: String,
    pub increment_annual: f64,
    pub increment_t: f64,
    pub ic_positive_every_fold: bool,
    pub excess_positive_folds: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CvReport {
    pub rows: usize,
    pub dates: usize,
    pub features: Vec<String>,
    pub embargo_days: u64,
    pub dev: Vec<Summary>,
    pub burnt: Vec<Summary>,
    pub verdicts: Vec<Verdict>,
    // Ridge weights per primary-horizon fit on the last development fold.
    pub ridge_weights: Vec<(String, f64)>,
}

pub const MIN_INCREMENT_T: f64 = 2.4;
// Bonferroni threshold once the title-topic trial makes four.
pub const ADDENDUM_MIN_T: f64 = 2.5;

struct Run {
    ics: Vec<(NaiveDate, f64)>,
    records: Vec<PeriodRecord>,
}

fn run_fold(panel: &Panel, fold: Fold, embargo: u64, k: usize, log: &dyn Fn(&str)) -> Vec<Run> {
    let (train, test) = split(panel, fold, embargo);
    Candidate::ALL
        .iter()
        .map(|&c| {
            let scorer = fit(c, panel, &train, k);
            let scores: Vec<f64> = par_chunks(&test, |part| {
                part.iter()
                    .map(|&i| scorer.score(&panel.rows[i].x))
                    .collect::<Vec<_>>()
            })
            .into_iter()
            .flatten()
            .collect();
            let (ics, records) = evaluate(panel, &test, &scores, k, false);
            log(&format!(
                "fold={} h={} candidate={} train={} test={}",
                fold.test_from,
                HORIZONS[CV_HORIZONS[k]],
                c.id(),
                train.len(),
                test.len()
            ));
            Run { ics, records }
        })
        .collect()
}

pub fn run_cv(
    panel: &Panel,
    folds: &[Fold],
    burnt: Option<Fold>,
    embargo: u64,
    log: &dyn Fn(&str),
) -> CvReport {
    let mut dev = Vec::new();
    let mut burnt_out = Vec::new();
    let mut verdicts = Vec::new();
    for (k, &h_idx) in CV_HORIZONS.iter().enumerate() {
        let horizon = HORIZONS[h_idx];
        let per_fold: Vec<Vec<Run>> = folds
            .iter()
            .map(|&f| run_fold(panel, f, embargo, k, log))
            .collect();
        for (ci, c) in Candidate::ALL.iter().enumerate() {
            let mut ics = Vec::new();
            let mut recs = Vec::new();
            let mut ctrl = Vec::new();
            let mut ic_ok = true;
            let mut ex_pos = 0;
            for (f, runs) in folds.iter().zip(&per_fold) {
                let s = summarize(
                    c.id(),
                    horizon,
                    &f.test_from.to_string(),
                    &runs[ci].ics,
                    &runs[ci].records,
                    &runs[0].records,
                );
                ic_ok &= s.ic_mean > 0.0;
                ex_pos += usize::from(s.excess_annual > 0.0);
                dev.push(s);
                ics.extend(runs[ci].ics.iter().copied());
                recs.extend(runs[ci].records.iter().copied());
                ctrl.extend(runs[0].records.iter().copied());
            }
            let pooled = summarize(c.id(), horizon, "pooled", &ics, &recs, &ctrl);
            if k == PRIMARY && ci > 0 {
                verdicts.push(Verdict {
                    candidate: c.id().to_owned(),
                    increment_annual: pooled.increment_annual,
                    increment_t: pooled.increment_t,
                    ic_positive_every_fold: ic_ok,
                    excess_positive_folds: ex_pos,
                    passed: pooled.increment_annual > 0.0
                        && pooled.increment_t >= MIN_INCREMENT_T
                        && ic_ok
                        && ex_pos >= 2,
                });
            }
            dev.push(pooled);
            // Addendum: the title-topic trial is judged against gbdt, not the control.
            if *c == Candidate::GbdtDart {
                let gi = Candidate::ALL
                    .iter()
                    .position(|x| *x == Candidate::Gbdt)
                    .unwrap_or(0);
                let (mut recs, mut base, mut pos) = (Vec::new(), Vec::new(), 0);
                for (f, runs) in folds.iter().zip(&per_fold) {
                    let s = summarize(
                        "gbdt_dart_vs_gbdt",
                        horizon,
                        &f.test_from.to_string(),
                        &runs[ci].ics,
                        &runs[ci].records,
                        &runs[gi].records,
                    );
                    pos += usize::from(s.increment_annual > 0.0);
                    dev.push(s);
                    recs.extend(runs[ci].records.iter().copied());
                    base.extend(runs[gi].records.iter().copied());
                }
                let pooled = summarize("gbdt_dart_vs_gbdt", horizon, "pooled", &ics, &recs, &base);
                if k == PRIMARY {
                    verdicts.push(Verdict {
                        candidate: "gbdt_dart_vs_gbdt".to_owned(),
                        increment_annual: pooled.increment_annual,
                        increment_t: pooled.increment_t,
                        ic_positive_every_fold: ic_ok,
                        excess_positive_folds: pos,
                        passed: pooled.increment_annual > 0.0
                            && pooled.increment_t >= ADDENDUM_MIN_T
                            && pos >= 2,
                    });
                }
                dev.push(pooled);
            }
        }
        if let Some(b) = burnt {
            let runs = run_fold(panel, b, embargo, k, log);
            for (ci, c) in Candidate::ALL.iter().enumerate() {
                burnt_out.push(summarize(
                    c.id(),
                    horizon,
                    &b.test_from.to_string(),
                    &runs[ci].ics,
                    &runs[ci].records,
                    &runs[0].records,
                ));
            }
        }
    }
    let ridge_weights = folds
        .last()
        .map(|&f| {
            let (train, _) = split(panel, f, embargo);
            match fit_ridge(panel, &train, PRIMARY, panel.base_cols) {
                Scorer::Linear { weights, .. } => panel
                    .names
                    .iter()
                    .map(|n| (*n).to_owned())
                    .zip(weights)
                    .collect(),
                Scorer::Trees(_) => Vec::new(),
            }
        })
        .unwrap_or_default();
    CvReport {
        rows: panel.rows.len(),
        dates: panel.groups.len(),
        features: panel.names.iter().map(|n| (*n).to_owned()).collect(),
        embargo_days: embargo,
        dev,
        burnt: burnt_out,
        verdicts,
        ridge_weights,
    }
}

/// Post-hoc diagnostics of the primary horizon; never used for selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub window: String,
    pub candidate: String,
    pub skip_one_session: bool,
    pub summary: Summary,
    pub bench_turnover: f64,
    pub kospi_annual: f64,
    pub kosdaq_annual: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecileRow {
    pub decile: usize,
    pub mean: f64,
    pub median: f64,
    pub count: usize,
}

fn index_annual(
    series: &crate::context::DailySeries,
    records: &[PeriodRecord],
    horizon: usize,
    skip: bool,
) -> f64 {
    let rets: Vec<f64> = records
        .iter()
        .filter_map(|r| {
            let i = series.index_at(r.date, false, 0)?;
            let a = if skip { i + 1 } else { i };
            let b = i + horizon;
            (b < series.len()).then(|| series.value(b) / series.value(a) - 1.0)
        })
        .collect();
    rets.iter().sum::<f64>() / rets.len().max(1) as f64 * TRADING_DAYS / horizon as f64
}

pub fn run_diagnostic(
    panel: &Panel,
    windows: &[Fold],
    embargo: u64,
    ctx: &crate::context::MarketContext,
) -> (Vec<Diagnostic>, Vec<DecileRow>) {
    let k = PRIMARY;
    let horizon = HORIZONS[CV_HORIZONS[k]];
    let mut out = Vec::new();
    let mut by_decile: Vec<Vec<f64>> = vec![Vec::new(); 10];
    for (wi, &w) in windows.iter().enumerate() {
        let (train, test) = split(panel, w, embargo);
        for skip in [false, true] {
            let mut control = Vec::new();
            for c in Candidate::ALL {
                let scorer = fit(c, panel, &train, k);
                let scores: Vec<f64> = test
                    .iter()
                    .map(|&i| scorer.score(&panel.rows[i].x))
                    .collect();
                if c == Candidate::ControlLowvol && !skip && wi + 1 < windows.len() {
                    decile_returns(panel, &test, &scores, CV_HORIZONS[k], &mut by_decile);
                }
                let (ics, records) = evaluate(panel, &test, &scores, k, skip);
                if c == Candidate::ControlLowvol {
                    control = records.clone();
                }
                let summary = summarize(
                    c.id(),
                    horizon,
                    &w.test_from.to_string(),
                    &ics,
                    &records,
                    &control,
                );
                let n = records.len().max(1) as f64;
                out.push(Diagnostic {
                    window: w.test_from.to_string(),
                    candidate: c.id().to_owned(),
                    skip_one_session: skip,
                    summary,
                    bench_turnover: records.iter().map(|r| r.bench_turnover).sum::<f64>() / n,
                    kospi_annual: index_annual(&ctx.kospi, &records, horizon, skip),
                    kosdaq_annual: index_annual(&ctx.kosdaq, &records, horizon, skip),
                });
            }
        }
    }
    let deciles = by_decile
        .into_iter()
        .enumerate()
        .map(|(d, mut v)| {
            v.sort_by(f64::total_cmp);
            DecileRow {
                decile: d + 1,
                mean: v.iter().sum::<f64>() / v.len().max(1) as f64,
                median: v.get(v.len() / 2).copied().unwrap_or(0.0),
                count: v.len(),
            }
        })
        .collect();
    (out, deciles)
}

// Forward returns grouped by per-date score decile (1 = lowest score).
fn decile_returns(
    panel: &Panel,
    test: &[usize],
    scores: &[f64],
    h_idx: usize,
    out: &mut [Vec<f64>],
) {
    let mut start = 0;
    while start < test.len() {
        let d = panel.rows[test[start]].date;
        let end = start + test[start..].partition_point(|&i| panel.rows[i].date == d);
        let ranks = centered_ranks(&scores[start..end]);
        for (q, r) in (start..end).zip(ranks) {
            let dec = (((r + 0.5) * 10.0).floor() as usize).min(9);
            out[dec].push(panel.rows[test[q]].ret[h_idx]);
        }
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_average_ties_and_center() {
        let r = centered_ranks(&[3.0, 1.0, 1.0, 5.0]);
        assert!((r[1] - r[2]).abs() < 1e-12);
        assert!((r[3] - 0.5).abs() < 1e-12);
        assert!((r[1] + 0.5 - 0.5 / 3.0).abs() < 1e-12);
        assert!(r.iter().sum::<f64>().abs() < 1e-12);
    }

    #[test]
    fn solve_recovers_known_solution() {
        let a = vec![4.0, 1.0, 1.0, 3.0];
        let x = solve(a, vec![1.0, 2.0], 2);
        assert!((4.0 * x[0] + x[1] - 1.0).abs() < 1e-12);
        assert!((x[0] + 3.0 * x[1] - 2.0).abs() < 1e-12);
    }

    #[test]
    fn bins_cover_the_rank_range() {
        assert_eq!(bin_of(-0.5), 0);
        assert_eq!(bin_of(0.5), (BINS - 1) as u8);
        assert_eq!(bin_of(0.0), (BINS / 2) as u8);
    }

    #[test]
    fn turnover_counts_buys_only() {
        let a: HashMap<&str, f64> = [("a", 0.5), ("b", 0.5)].into_iter().collect();
        let b: HashMap<&str, f64> = [("a", 0.5), ("c", 0.5)].into_iter().collect();
        assert!((one_way_turnover(&a, &b) - 0.5).abs() < 1e-12);
    }
}
