"""Coverage of the KIS investor-flow cache against data.go.kr traded sessions.

For every universe code, compares sessions with volume > 0 in the data.go.kr cache
(within the flow window) to sessions present in the investor-flow cache, and reports
missing sessions by year, by code, and for codes whose bars stop before the data end.
"""

import argparse
import collections
import glob
import json
import os


def dgk_sessions(root: str, code: str, lo: str, hi: str) -> tuple[set[str], str]:
    out: set[str] = set()
    last = ""
    for f in glob.glob(os.path.join(root, code, "*.json")):
        try:
            j = json.load(open(f, encoding="utf-8"))
        except Exception:
            continue
        items = j.get("response", {}).get("body", {}).get("items", {})
        it = items.get("item", []) if isinstance(items, dict) else []
        it = [it] if isinstance(it, dict) else it
        for x in it:
            d = x.get("basDt", "")
            last = max(last, d)
            if lo <= d <= hi and int(x.get("trqu", "0") or 0) > 0:
                out.add(d)
    return out, last


def flow_sessions(root: str, code: str) -> set[str]:
    out: set[str] = set()
    for f in glob.glob(os.path.join(root, code, "*.json")):
        j = json.load(open(f, encoding="utf-8"))
        # Investor and short-sale pages use output2/stck_bsop_date; lending uses output1/bsop_date.
        for r in (j.get("output2") or []) + (j.get("output1") or [] if isinstance(j.get("output1"), list) else []):
            d = r.get("stck_bsop_date") or r.get("bsop_date")
            if d:
                out.add(d)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--from", dest="lo", default="20191001")
    ap.add_argument("--to", dest="hi", default="20241231")
    ap.add_argument("--out", required=True)
    ap.add_argument("--cache", default="data/raw/kis_investor_daily")
    a = ap.parse_args()
    os.chdir(a.repo)
    codes = [l.split("#")[0].strip() for l in open("config/universe/kr-union.txt", encoding="utf-8")]
    codes = [c for c in codes if c]
    by_year = collections.defaultdict(lambda: [0, 0])
    stopped = [0, 0]
    per_code = []
    no_cache = []
    for code in codes:
        traded, last = dgk_sessions("data/raw/data_go_kr", code, a.lo, a.hi)
        if not traded:
            continue
        flows = flow_sessions(a.cache, code)
        if not flows:
            no_cache.append(code)
        miss = traded - flows
        for d in traded:
            y = by_year[d[:4]]
            y[0] += 1
            y[1] += d in miss
        if last < "20261001":
            stopped[0] += len(traded)
            stopped[1] += len(miss)
        if miss:
            per_code.append((code, len(traded), len(miss), min(miss), max(miss)))
    total = sum(v[0] for v in by_year.values())
    missed = sum(v[1] for v in by_year.values())
    per_code.sort(key=lambda r: -r[2])
    report = {
        "codes_with_traded_sessions": sum(1 for _ in codes),
        "traded_sessions": total,
        "missing_sessions": missed,
        "missing_share": missed / max(total, 1),
        "by_year": {k: {"traded": v[0], "missing": v[1], "share": v[1] / max(v[0], 1)} for k, v in sorted(by_year.items())},
        "stopped_names": {"traded": stopped[0], "missing": stopped[1], "share": stopped[1] / max(stopped[0], 1)},
        "codes_without_cache": no_cache,
        "codes_with_missing": len(per_code),
        "worst_codes": per_code[:30],
    }
    with open(a.out, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=1, ensure_ascii=False)
    print(json.dumps({k: v for k, v in report.items() if k != "worst_codes"}, ensure_ascii=False)[:3000])
    print("worst", per_code[:15])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
