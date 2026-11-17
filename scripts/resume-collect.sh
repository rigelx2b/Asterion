#!/usr/bin/env bash
# Usage: scripts/resume-collect.sh <history|financials> [delay_secs] [min_free_mb]
# Waits delay_secs, then until free physical memory >= min_free_mb (rechecks every 30 s),
# then runs one resumable collector, appending to data/logs/<job>-resume-<date>.log.
set -u
cd "$(dirname "$0")/.."
JOB=${1:?job: history or financials}
DELAY=${2:-0}
MIN_FREE_MB=${3:-2048}
BIN=./target/release/asterion.exe
LOG="data/logs/${JOB}-resume-$(date +%Y%m%d).log"

free_mb() {
  powershell.exe -NoProfile -Command \
    "[int]((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1024)" | tr -d '\r'
}

sleep "$DELAY"
while :; do
  FREE=$(free_mb)
  echo "== $(date) free_mb=${FREE} min_free_mb=${MIN_FREE_MB}" >> "$LOG"
  [ "${FREE:-0}" -ge "$MIN_FREE_MB" ] && break
  sleep 30
done

case "$JOB" in
  history) CMD=("$BIN" fetch-history --universe config/universe/kr-union.txt) ;;
  financials) CMD=("$BIN" fetch-financials) ;;
  *) echo "unknown job: $JOB" >&2; exit 2 ;;
esac

echo "== $(date) start ${CMD[*]}" >> "$LOG"
"${CMD[@]}" >> "$LOG" 2>&1
CODE=$?
echo "== $(date) exit=${CODE}" >> "$LOG"
exit "$CODE"
