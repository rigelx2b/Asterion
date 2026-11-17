#!/usr/bin/env bash
# Union of point-in-time listings, then fetch prices for every code (cached codes are skipped).
set -u
BIN=${BIN:-data/bin/asterion-fetch.exe}
OUT=config/universe/snapshots
for d in 2020-01-02 2020-07-01 2021-01-04 2021-07-01 2022-01-03 2022-07-01 2023-01-02 2023-07-03 \
         2024-01-02 2024-07-01 2025-01-02 2025-07-01 2026-01-02 2026-07-01 2026-11-10; do
  f="$OUT/kr-$d.txt"
  [ -s "$f" ] || "$BIN" universe --as-of "$d" --top 5000 --out "$f" || echo "snapshot_failed $d"
done
cat "$OUT"/kr-*.txt config/universe/kr-all-20231228.txt | grep -v '^#' | sed 's/#.*//' | tr -d ' \r' | grep -E '^[0-9A-Z]{6}$' | sort -u > config/universe/kr-union.txt
echo "union_codes=$(wc -l < config/universe/kr-union.txt)"
"$BIN" fetch --universe config/universe/kr-union.txt --from 2015-01-01
echo "expand_universe_done exit=$?"
