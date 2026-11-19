"""IC diagnostics over `research-cv --audit` row dumps (audit-<mode>.csv).

Per-date IC matches research::evaluate: Pearson of the within-date centered ranks of a
score (among the test rows of that date) against the stored label rank `y20`.
"""

import argparse
import json
import sys

import numpy as np
import pandas as pd

CANDIDATES = ["control_lowvol", "composite6", "ridge", "gbdt"]
HORIZON = 20


def centered_rank(s: pd.Series) -> pd.Series:
    n = len(s)
    if n < 2:
        return s * 0.0
    return (s.rank(method="average") - 1.0) / (n - 1) - 0.5


def pearson(a: np.ndarray, b: np.ndarray) -> float:
    a = a - a.mean()
    b = b - b.mean()
    d = np.sqrt((a * a).sum() * (b * b).sum())
    return float((a * b).sum() / d) if d > 0 else 0.0


def mean_t(v: np.ndarray, overlap: int) -> tuple[float, float]:
    n = len(v)
    if n < 2:
        return (float(v.mean()) if n else 0.0, 0.0)
    m = float(v.mean())
    var = float(v.var(ddof=1))
    eff = max(n / overlap, 1.0)
    return m, (m / np.sqrt(var / eff) if var > 0 else 0.0)


def per_date(df: pd.DataFrame, score: str, label: str, rank_label: bool) -> pd.Series:
    def one(g: pd.DataFrame) -> float:
        sr = centered_rank(g[score]).to_numpy()
        y = centered_rank(g[label]).to_numpy() if rank_label else g[label].to_numpy()
        return pearson(sr, y)

    return df.groupby("date", sort=True).apply(one, include_groups=False)


def summarize_ic(ics: pd.Series) -> dict:
    v = ics.to_numpy()
    m, t = mean_t(v, HORIZON)
    every = v[::HORIZON]
    m2, t2 = mean_t(every, 1)
    month = ics.groupby(ics.index.str[:7]).mean()
    q = month.quantile([0.0, 0.25, 0.5, 0.75, 1.0]).to_numpy()
    return {
        "dates": int(len(v)),
        "ic_mean": m,
        "ic_t_overlap": t,
        "nonoverlap_n": int(len(every)),
        "nonoverlap_mean": m2,
        "nonoverlap_t": t2,
        "daily_ic_sd": float(v.std(ddof=1)),
        "months": int(len(month)),
        "months_positive_share": float((month > 0).mean()),
        "month_q": [float(x) for x in q],
    }


def load(path: str) -> pd.DataFrame:
    df = pd.read_csv(path, dtype={"symbol": str, "date": str})
    df["skip20"] = (1.0 + df["ret20"]) / (1.0 + df["ret1"]) - 1.0
    return df


def base_report(df: pd.DataFrame) -> dict:
    out = {}
    for c in CANDIDATES:
        ic = per_date(df, c, "y20", rank_label=False)
        r = {"rank_ic": summarize_ic(ic)}
        r["rank_ic_by_fold"] = {
            str(f): summarize_ic(per_date(g, c, "y20", False))["ic_mean"]
            for f, g in df.groupby("fold")
        }
        r["pearson_raw_ret_ic"] = summarize_ic(per_date(df, c, "ret20", rank_label=False))[
            "ic_mean"
        ]
        r["skip_one_session_ic"] = summarize_ic(per_date(df, c, "skip20", rank_label=True))[
            "ic_mean"
        ]
        # One IC over every test row at once (no per-date grouping).
        r["pooled_one_shot_spearman"] = pearson(
            df[c].rank().to_numpy(), df["ret20"].rank().to_numpy()
        )
        r["pooled_one_shot_pearson"] = pearson(df[c].to_numpy(), df["ret20"].to_numpy())
        out[c] = r
    return out


def deciles(df: pd.DataFrame, c: str) -> list[dict]:
    d = df.copy()
    d["dec"] = d.groupby("date")[c].transform(
        lambda s: np.minimum(((centered_rank(s) + 0.5) * 10).astype(int), 9)
    )
    rows = []
    for k, g in d.groupby("dec"):
        rows.append(
            {
                "decile": int(k) + 1,
                "mean_ret20": float(g["ret20"].mean()),
                "median_ret20": float(g["ret20"].median()),
                "n": int(len(g)),
            }
        )
    return rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    report = {}
    base = load(f"{args.dir}/audit-base.csv")
    report["base"] = base_report(base)
    report["deciles"] = {c: deciles(base, c) for c in ["control_lowvol", "gbdt"]}
    try:
        lag = load(f"{args.dir}/audit-lag.csv")
    except FileNotFoundError:
        lag = None
    if lag is not None:
        same = base[base["has_prev"] == 1]
        report["lag"] = {
            c: {
                "base_same_rows": summarize_ic(per_date(same, c, "y20", False))["ic_mean"],
                "lagged": summarize_ic(per_date(lag, c, "y20", False))["ic_mean"],
                "rows_base": int(len(same)),
                "rows_lag": int(len(lag)),
            }
            for c in CANDIDATES
        }
    try:
        placebo = load(f"{args.dir}/audit-placebo.csv")
    except FileNotFoundError:
        placebo = None
    if placebo is not None:
        report["placebo"] = {
            c: summarize_ic(per_date(placebo, c, "y20", False)) for c in CANDIDATES
        }
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2, ensure_ascii=False)
    json.dump(report, sys.stdout, indent=1, ensure_ascii=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
