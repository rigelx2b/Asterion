#!/usr/bin/env bash
# Runs every API collector at once; each stops at its own quota/finite work list,
# and all of them stop once data/ exceeds MAX_GB.
set -u
BIN=${BIN:-data/bin/asterion-collect.exe}
MAX_GB=${MAX_GB:-50}
"$BIN" fetch-macro --from 2015-01-01 > data/logs/fetch-macro.log 2>&1 &
"$BIN" fetch-financials --max-data-gb "$MAX_GB" > data/logs/fetch-financials.log 2>&1 &
wait
echo "collect_all_done data_bytes=$(du -sb data | cut -f1)"
