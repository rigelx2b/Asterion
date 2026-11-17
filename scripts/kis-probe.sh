#!/usr/bin/env bash
# One-time check after KIS keys are set: fetch a short overlap window with both
# adjustment flags so they can be compared with data.go.kr before any bulk fetch.
set -u
BIN=${BIN:-data/bin/asterion-rank.exe}
"$BIN" fetch-history --symbols 005930,035720 --from 2019-11-01 --to 2020-03-31 --adjusted 0 --root data/raw/kis_probe
"$BIN" fetch-history --symbols 005930,035720 --from 2019-11-01 --to 2020-03-31 --adjusted 1 --root data/raw/kis_probe
"$BIN" fetch-overseas --list config/universe/us-core.txt --from 2024-01-01 --adjusted 1 --root data/raw/kis_probe_overseas
echo "kis_probe_done"
